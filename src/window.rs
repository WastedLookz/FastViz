//! Main window: header bar with breadcrumb and sort menu, folder tree,
//! thumbnail grid and status bar.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::grid::{self, Grid};
use crate::loader::Thumbnailer;
use crate::media::Kind;
use crate::model::{self, FileItem, SortKey};
use crate::prefs::{self, Startup};
use crate::scan::{self, ScanMsg};
use crate::settings::Settings;
use crate::storage;
use crate::trace;
use crate::tree::FolderTree;
use crate::viewer::Viewer;

mod files;
mod fullscreen;

pub struct Window {
    window: adw::ApplicationWindow,
    paned: gtk::Paned,
    tree: Rc<FolderTree>,
    grid: Grid,
    thumbnailer: Rc<Thumbnailer>,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    store: gio::ListStore,
    selection: gtk::MultiSelection,
    sorter: gtk::CustomSorter,
    sort: Rc<Cell<(SortKey, bool)>>,
    sort_label: gtk::Label,
    sort_icon: gtk::Image,
    crumbs: gtk::Box,
    count_label: gtk::Label,
    selection_label: gtk::Label,
    size: gtk::Adjustment,
    pane_width: Cell<i32>,
    show_hidden: Cell<bool>,
    startup_folder: Rc<RefCell<Startup>>,
    current: RefCell<Option<PathBuf>>,
    scan_task: RefCell<Option<glib::JoinHandle<()>>>,
    /// Item count and total bytes of the current folder.
    totals: Cell<(u32, u64)>,
    scanning: Cell<bool>,
    /// True until the first folder finishes loading (falls back to Home if it fails).
    startup: Cell<bool>,
    /// A live re-sort is scheduled (metadata arriving while sorting by it).
    resort_queued: Cell<bool>,
    /// Holds the paned and status bar; context menus are parented here.
    content: gtk::Box,
    toasts: adw::ToastOverlay,
    /// What we last put on the clipboard: (paths, cut).
    clipboard: RefCell<Option<(Vec<PathBuf>, bool)>>,
    /// The browser and the fullscreen viewer.
    pages: gtk::Stack,
    viewer: Rc<Viewer>,
    viewer_black: Rc<Cell<bool>>,
}

impl Window {
    pub fn new(app: &adw::Application, settings: Settings) -> Rc<Self> {
        let sort = Rc::new(Cell::new((settings.sort_key, settings.sort_desc)));
        let store = gio::ListStore::new::<FileItem>();
        let sorter = model::sorter(sort.clone());
        let sorted = gtk::SortListModel::new(Some(store.clone()), Some(sorter.clone()));
        let selection = gtk::MultiSelection::new(Some(sorted));

        let size = gtk::Adjustment::new(
            settings
                .thumb_size
                .clamp(grid::MIN_SIZE, grid::MAX_SIZE)
                .into(),
            grid::MIN_SIZE.into(),
            grid::MAX_SIZE.into(),
            1.0,
            16.0,
            0.0,
        );
        let thumbnailer = Thumbnailer::new();
        let grid = Grid::new(&selection, &size, &thumbnailer);

        let empty = adw::StatusPage::builder()
            .icon_name("folder-pictures-symbolic")
            .build();
        let stack = gtk::Stack::new();
        stack.add_child(&grid.scroller);
        stack.add_child(&empty);

        trace!("building window");
        let tree = FolderTree::new(settings.show_hidden, &settings.expanded);
        trace!("folder tree ready");
        tree.widget.set_visible(settings.pane_visible);
        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&tree.widget)
            .end_child(&stack)
            .resize_start_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(settings.pane_width)
            .vexpand(true)
            .build();

        let count_label = gtk::Label::builder().xalign(0.0).build();
        let selection_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let slider = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .adjustment(&size)
            .draw_value(false)
            .width_request(140)
            .tooltip_text("Thumbnail size (Ctrl+wheel)")
            .build();
        let statusbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .css_classes(["fv-statusbar"])
            .build();
        statusbar.append(&count_label);
        statusbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        statusbar.append(&selection_label);
        statusbar.append(&slider);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&paned);
        content.append(&statusbar);

        let crumbs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let crumb_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_width(true)
            .child(&crumbs)
            .build();
        // Keep the deepest folder in view when the path is too long to fit.
        crumb_scroller
            .hadjustment()
            .connect_changed(|adj| adj.set_value(adj.upper() - adj.page_size()));

        let sort_label = gtk::Label::new(None);
        let sort_icon = gtk::Image::new();
        let sort_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sort_content.append(&sort_icon);
        sort_content.append(&sort_label);
        let sort_button = gtk::MenuButton::builder()
            .child(&sort_content)
            .menu_model(&sort_menu())
            .tooltip_text("Sort")
            .build();
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&main_menu())
            .tooltip_text("Menu")
            .build();
        let header = adw::HeaderBar::builder()
            .title_widget(&crumb_scroller)
            .build();
        header.pack_end(&menu_button);
        header.pack_end(&sort_button);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&content));
        let viewer = Viewer::new(settings.viewer_black);
        let pages = gtk::Stack::new();
        pages.add_named(&view, Some("browser"));
        pages.add_named(&viewer.widget, Some("viewer"));
        // Toasts (like Trash with Undo) show over both pages.
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&pages));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("FastViz")
            .default_width(settings.window_width)
            .default_height(settings.window_height)
            .maximized(settings.maximized)
            .content(&toasts)
            .build();

        let this = Rc::new(Self {
            window,
            paned,
            tree,
            grid,
            thumbnailer,
            stack,
            empty,
            store,
            selection,
            sorter,
            sort,
            sort_label,
            sort_icon,
            crumbs,
            count_label,
            selection_label,
            size,
            pane_width: Cell::new(settings.pane_width),
            show_hidden: Cell::new(settings.show_hidden),
            startup_folder: Rc::new(RefCell::new(Startup {
                folder: settings.start_folder.as_ref().map(PathBuf::from),
                open_last: settings.open_last_folder,
            })),
            current: RefCell::default(),
            scan_task: RefCell::default(),
            totals: Cell::new((0, 0)),
            scanning: Cell::new(false),
            startup: Cell::new(true),
            resort_queued: Cell::new(false),
            content,
            toasts,
            clipboard: RefCell::default(),
            pages,
            viewer,
            viewer_black: Rc::new(Cell::new(settings.viewer_black)),
        });
        this.setup_actions(&settings);
        this.setup_file_actions();
        this.setup_viewer();
        this.connect_signals();
        this.update_sort_button();

        let start = if settings.open_last_folder {
            settings.last_folder.as_ref()
        } else {
            settings.start_folder.as_ref()
        };
        let start = start.map(PathBuf::from).unwrap_or_else(glib::home_dir);
        this.navigate(start, true);
        this
    }

    pub fn present(&self) {
        if trace::enabled() {
            self.window.connect_map(|window| {
                let Some(clock) = window.frame_clock() else {
                    return;
                };
                let handler = Rc::new(Cell::new(None));
                let id = clock.connect_after_paint(glib::clone!(
                    #[strong]
                    handler,
                    move |clock| {
                        trace!("first paint");
                        if let Some(id) = handler.take() {
                            clock.disconnect(id);
                        }
                    }
                ));
                handler.set(Some(id));
            });
        }
        self.window.present();
        self.grid.view.grab_focus();
        trace::snapshot_on_request(self.window.upcast_ref(), 1500);
        trace::autoscroll_on_request(&self.grid.scroller);
    }

    fn setup_actions(self: &Rc<Self>, settings: &Settings) {
        let sort_by = gio::SimpleAction::new_stateful(
            "sort-by",
            Some(glib::VariantTy::STRING),
            &settings.sort_key.id().to_variant(),
        );
        sort_by.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |action, param| {
                let Some(key) = param.and_then(|p| p.str()).and_then(SortKey::from_id) else {
                    return;
                };
                action.set_state(&key.id().to_variant());
                this.set_sort(key, this.sort.get().1);
            }
        ));

        let sort_desc = gio::SimpleAction::new_stateful(
            "sort-desc",
            Some(glib::VariantTy::BOOLEAN),
            &settings.sort_desc.to_variant(),
        );
        sort_desc.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |action, param| {
                let Some(desc) = param.and_then(|p| p.get::<bool>()) else {
                    return;
                };
                action.set_state(&desc.to_variant());
                this.set_sort(this.sort.get().0, desc);
            }
        ));

        let show_hidden = toggle_action(
            "show-hidden",
            settings.show_hidden,
            glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |show| {
                    this.show_hidden.set(show);
                    this.tree.set_show_hidden(show);
                    this.reload();
                }
            ),
        );

        let show_pane = toggle_action(
            "show-pane",
            settings.pane_visible,
            glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |show| {
                    if !show {
                        this.pane_width.set(this.paned.position());
                    }
                    this.tree.widget.set_visible(show);
                    if show {
                        this.paned.set_position(this.pane_width.get());
                    }
                }
            ),
        );

        let preferences = gio::SimpleAction::new("preferences", None);
        preferences.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _| {
                prefs::present(
                    &this.window,
                    &this.startup_folder,
                    this.current.borrow().clone(),
                    this.viewer_black.get(),
                    glib::clone!(
                        #[weak]
                        this,
                        move |black| this.set_viewer_black(black)
                    ),
                )
            }
        ));

        let go_up = gio::SimpleAction::new("go-up", None);
        go_up.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _| {
                let parent = this
                    .current
                    .borrow()
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(Path::to_path_buf);
                if let Some(parent) = parent {
                    this.navigate(parent, true);
                }
            }
        ));

        for action in [
            sort_by,
            sort_desc,
            show_hidden,
            show_pane,
            preferences,
            go_up,
        ] {
            self.window.add_action(&action);
        }
    }

    fn connect_signals(self: &Rc<Self>) {
        // Double-click or Enter: folders open in place, other files in their
        // default app. Photos and videos get the fullscreen viewer in Stage 3.
        self.grid.view.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, position| {
                let Some(item) = this.selection.item(position).and_downcast::<FileItem>() else {
                    return;
                };
                let entry = item.entry();
                match entry.kind {
                    Kind::Folder => this.navigate(entry.path.clone(), true),
                    Kind::Other => {
                        let file = gio::File::for_path(&entry.path);
                        gtk::FileLauncher::new(Some(&file)).launch(
                            Some(&this.window),
                            None::<&gio::Cancellable>,
                            |_| {},
                        );
                    }
                    Kind::Image | Kind::Video => this.open_viewer(position),
                }
            }
        ));

        self.thumbnailer.connect_meta(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |item| this.meta_arrived(item)
        ));

        self.tree.connect_selected(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |path| this.navigate(path, false)
        ));

        self.selection.connect_selection_changed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _, _| this.update_selection_status()
        ));

        self.size.connect_value_changed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |adj| this.grid.set_size(adj.value().round() as i32)
        ));

        self.window.connect_realize(|window| {
            let backend = WidgetExt::display(window).type_().name();
            let renderer = window.renderer().map_or("none", |r| r.type_().name());
            trace!("window realized: {backend}, {renderer}");
        });

        // The handler holds the only strong reference, keeping `self` alive
        // for the window's lifetime.
        let this = self.clone();
        self.window.connect_close_request(move |_| {
            this.save_settings();
            glib::Propagation::Proceed
        });
    }

    /// Shows `path` in the grid. `reveal` also selects it in the folder tree.
    fn navigate(self: &Rc<Self>, path: PathBuf, reveal: bool) {
        if self.current.borrow().as_ref() == Some(&path) {
            return;
        }
        if let Some(task) = self.scan_task.take() {
            task.abort();
        }
        self.store.remove_all();
        let rotational = storage::is_rotational(&path);
        trace!("storage: rotational = {rotational:?}");
        self.thumbnailer.reset(rotational);
        self.totals.set((0, 0));
        self.scanning.set(true);
        self.update_count_status();
        self.selection_label.set_text("");
        self.stack.set_visible_child(&self.grid.scroller);
        self.set_crumbs(&path);
        if reveal {
            self.tree.reveal(&path);
        }
        self.window
            .set_title(Some(&format!("{} — FastViz", display_name(&path))));
        self.current.replace(Some(path.clone()));

        trace!("scan start: {}", path.display());
        let started = Instant::now();
        let scan = scan::start(path, self.show_hidden.get());
        let weak = Rc::downgrade(self);
        let task = glib::spawn_future_local(async move {
            let mut first = true;
            while let Ok(msg) = scan.rx.recv().await {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                match msg {
                    ScanMsg::Batch(entries) => {
                        if first {
                            first = false;
                            trace!(
                                "scan first batch: {} items after {:.1} ms",
                                entries.len(),
                                started.elapsed().as_secs_f64() * 1e3
                            );
                        }
                        let batch_started = Instant::now();
                        let count = entries.len();
                        this.add_entries(entries);
                        trace!(
                            "scan batch: {count} items inserted in {:.1} ms",
                            batch_started.elapsed().as_secs_f64() * 1e3
                        );
                    }
                    ScanMsg::Done(result) => {
                        trace!(
                            "scan done: {} items in {:.1} ms",
                            this.totals.get().0,
                            started.elapsed().as_secs_f64() * 1e3
                        );
                        this.scan_done(result);
                        return;
                    }
                }
            }
        });
        self.scan_task.replace(Some(task));
    }

    /// Rescans the current folder (e.g. after toggling hidden files).
    fn reload(self: &Rc<Self>) {
        if let Some(path) = self.current.take() {
            self.navigate(path, true);
        }
    }

    fn add_entries(&self, entries: Vec<scan::Entry>) {
        let (count, bytes) = self.totals.get();
        let added: u64 = entries.iter().map(|e| e.size).sum();
        self.totals
            .set((count + entries.len() as u32, bytes + added));
        let items: Vec<FileItem> = entries.into_iter().map(FileItem::new).collect();
        self.thumbnailer.register(&items);
        if self.sort.get().0.needs_meta() {
            self.thumbnailer.want_metadata(&items);
        }
        self.keeping_top(|| self.store.extend_from_slice(&items));
        self.update_count_status();
    }

    /// Runs `change` to the model; if the grid was scrolled to the top it
    /// stays there. Otherwise GTK keeps its anchor item in view, so items
    /// sorting in above it would drag the view down the folder.
    fn keeping_top(&self, change: impl FnOnce()) {
        let at_top = self.grid.scroller.vadjustment().value() < 1.0;
        change();
        if at_top && self.selection.n_items() > 0 {
            self.grid
                .view
                .scroll_to(0, gtk::ListScrollFlags::NONE, None);
        }
    }

    fn meta_arrived(self: &Rc<Self>, item: &FileItem) {
        if self.sort.get().0.needs_meta() && !self.resort_queued.replace(true) {
            // Re-sort live as metadata streams in, at most a few times a second.
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(300),
                glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    move || {
                        this.resort_queued.set(false);
                        this.keeping_top(|| this.sorter.changed(gtk::SorterChange::Different));
                        this.thumbnailer.order_changed();
                    }
                ),
            );
        }
        let selected = self.selection.selection();
        if selected.size() == 1
            && self.selection.item(selected.nth(0)).as_ref() == Some(item.upcast_ref())
        {
            self.update_selection_status();
        }
    }

    fn scan_done(self: &Rc<Self>, result: Result<(), String>) {
        self.scan_task.take();
        self.thumbnailer.scan_finished();
        self.scanning.set(false);
        self.update_count_status();
        let startup = self.startup.replace(false);
        match result {
            // The remembered folder is gone (e.g. its drive isn't mounted): use Home.
            Err(_) if startup => {
                let this = self.clone();
                glib::idle_add_local_once(move || this.navigate(glib::home_dir(), true));
            }
            Err(error) => {
                self.empty.set_title("Can't Open Folder");
                self.empty.set_description(Some(&error));
                self.stack.set_visible_child(&self.empty);
            }
            Ok(()) if self.totals.get().0 == 0 => {
                self.empty.set_title("Empty Folder");
                self.empty.set_description(None);
                self.stack.set_visible_child(&self.empty);
            }
            Ok(()) => {}
        }
    }

    fn set_sort(&self, key: SortKey, descending: bool) {
        self.sort.set((key, descending));
        if key.needs_meta() {
            let items: Vec<FileItem> = self.store.iter::<FileItem>().flatten().collect();
            self.thumbnailer.want_metadata(&items);
        } else {
            self.thumbnailer.cancel_metadata();
        }
        let started = Instant::now();
        self.sorter.changed(gtk::SorterChange::Different);
        self.thumbnailer.order_changed();
        trace!(
            "sorted {} items by {} in {:.1} ms",
            self.selection.n_items(),
            key.id(),
            started.elapsed().as_secs_f64() * 1e3
        );
        self.update_sort_button();
        // The grid would otherwise stay on its old top item, now somewhere else.
        if self.selection.n_items() > 0 {
            let selected = self.selection.selection();
            let target = if selected.is_empty() {
                0
            } else {
                selected.minimum()
            };
            self.grid
                .view
                .scroll_to(target, gtk::ListScrollFlags::NONE, None);
        }
    }

    fn update_sort_button(&self) {
        let (key, descending) = self.sort.get();
        self.sort_label.set_text(key.label());
        self.sort_icon.set_icon_name(Some(if descending {
            "view-sort-descending-symbolic"
        } else {
            "view-sort-ascending-symbolic"
        }));
    }

    fn set_crumbs(self: &Rc<Self>, path: &Path) {
        while let Some(child) = self.crumbs.first_child() {
            self.crumbs.remove(&child);
        }
        let (root, root_name) = self.tree.root_for(path);
        let mut parts = vec![(root.clone(), root_name)];
        if let Ok(rest) = path.strip_prefix(&root) {
            let mut current = root;
            for component in rest.components() {
                current.push(component);
                let name = component.as_os_str().to_string_lossy().into_owned();
                parts.push((current.clone(), name));
            }
        }
        let last = parts.len() - 1;
        for (i, (target, name)) in parts.into_iter().enumerate() {
            if i > 0 {
                let separator = gtk::Label::builder()
                    .label("/")
                    .css_classes(["dim-label", "fv-crumb-separator"])
                    .build();
                self.crumbs.append(&separator);
            }
            let button = gtk::Button::builder()
                .label(name)
                .css_classes(["flat", "fv-crumb"])
                .build();
            if i == last {
                button.add_css_class("fv-crumb-current");
            }
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_| this.navigate(target.clone(), true)
            ));
            self.crumbs.append(&button);
        }
    }

    fn update_count_status(&self) {
        let (count, bytes) = self.totals.get();
        let noun = if count == 1 { "object" } else { "objects" };
        let mut text = format!(
            "{} {noun} [{}]",
            group_digits(count.into()),
            glib::format_size(bytes)
        );
        if self.scanning.get() {
            text.insert_str(0, "Loading… ");
        }
        self.count_label.set_text(&text);
    }

    fn update_selection_status(&self) {
        let selected = self.selection.selection();
        let text = match selected.size() {
            0 => String::new(),
            1 => self
                .selection
                .item(selected.nth(0))
                .and_downcast::<FileItem>()
                .map(|item| describe(&item, self.sort.get().0 == SortKey::Taken))
                .unwrap_or_default(),
            n => {
                let mut bytes = 0;
                if let Some((iter, first)) = gtk::BitsetIter::init_first(&selected) {
                    for position in std::iter::once(first).chain(iter) {
                        if let Some(item) = self.selection.item(position).and_downcast::<FileItem>()
                        {
                            bytes += item.entry().size;
                        }
                    }
                }
                format!(
                    "{} selected [{}]",
                    group_digits(n),
                    glib::format_size(bytes)
                )
            }
        };
        self.selection_label.set_text(&text);
    }

    fn save_settings(&self) {
        let (sort_key, sort_desc) = self.sort.get();
        let (window_width, window_height) = self.window.default_size();
        let pane_visible = self.tree.widget.is_visible();
        let startup = self.startup_folder.borrow();
        Settings {
            last_folder: self
                .current
                .borrow()
                .as_ref()
                .and_then(|p| p.to_str())
                .map(String::from),
            start_folder: startup
                .folder
                .as_ref()
                .and_then(|p| p.to_str())
                .map(String::from),
            open_last_folder: startup.open_last,
            window_width,
            window_height,
            maximized: self.window.is_maximized(),
            pane_width: if pane_visible {
                self.paned.position()
            } else {
                self.pane_width.get()
            },
            pane_visible,
            show_hidden: self.show_hidden.get(),
            sort_key,
            sort_desc,
            thumb_size: self.size.value().round() as i32,
            viewer_black: self.viewer_black.get(),
            expanded: self.tree.expanded_rows(),
        }
        .save();
    }
}

/// A boolean action that flips its state on activation and reports the new value.
fn toggle_action(name: &str, initial: bool, apply: impl Fn(bool) + 'static) -> gio::SimpleAction {
    let action = gio::SimpleAction::new_stateful(name, None, &initial.to_variant());
    action.connect_activate(move |action, _| {
        let value = !action
            .state()
            .and_then(|s| s.get::<bool>())
            .unwrap_or(false);
        action.set_state(&value.to_variant());
        apply(value);
    });
    action
}

fn sort_menu() -> gio::Menu {
    let keys = gio::Menu::new();
    for key in SortKey::ALL {
        let item = gio::MenuItem::new(Some(key.label()), None);
        item.set_action_and_target_value(Some("win.sort-by"), Some(&key.id().to_variant()));
        keys.append_item(&item);
    }
    let order = gio::Menu::new();
    for (label, descending) in [("Ascending", false), ("Descending", true)] {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some("win.sort-desc"), Some(&descending.to_variant()));
        order.append_item(&item);
    }
    let menu = gio::Menu::new();
    menu.append_section(None, &keys);
    menu.append_section(None, &order);
    menu
}

fn main_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("Show Hidden Folders"), Some("win.show-hidden"));
    menu.append(Some("Show Folder Pane"), Some("win.show-pane"));
    let app = gio::Menu::new();
    app.append(Some("Preferences"), Some("win.preferences"));
    menu.append_section(None, &app);
    menu
}

/// Status-bar summary of one item: name, date, format, dimensions and size.
/// The date is when it was taken if sorting by that, else when modified.
fn describe(item: &FileItem, taken: bool) -> String {
    let entry = item.entry();
    if entry.kind == Kind::Folder {
        let date = glib::DateTime::from_unix_local(entry.mtime)
            .and_then(|d| d.format("%Y-%m-%d %H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        return [entry.name.as_str(), &date, "Folder"].join("    ");
    }
    let meta = item.meta();
    let time = match meta.and_then(|m| m.taken) {
        Some(taken_at) if taken => taken_at,
        _ => entry.mtime,
    };
    let date = glib::DateTime::from_unix_local(time)
        .and_then(|d| d.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    let mut parts = vec![entry.name.clone(), date, item.format().to_owned()];
    if let Some(meta) = meta {
        parts.push(format!("{} × {}", meta.width, meta.height));
    }
    parts.push(glib::format_size(entry.size).to_string());
    parts.join("    ")
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// 4568 -> "4,568".
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::group_digits;

    #[test]
    fn groups_thousands() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(4568), "4,568");
        assert_eq!(group_digits(1234567), "1,234,567");
    }
}
