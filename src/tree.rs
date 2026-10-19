//! Lazy folder tree with Home, mounted volumes and Computer (/) as roots.
//!
//! A folder's children are listed on a worker thread the first time it is
//! expanded. Rows show an expander until we know the folder has no
//! subfolders: instantly from the link count on ext4 and friends, or by
//! peeking inside each folder where link counts mean nothing (NTFS via FUSE).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use crate::natsort;
use crate::scan::is_hidden;

/// (root folder, folder) identifies a row; the same folder can appear under several roots.
type RowKey = (PathBuf, PathBuf);
type SelectHandler = Box<dyn Fn(PathBuf)>;
/// Called on right-click with the widget clicked, the point in it and its folder.
type ContextHandler = Box<dyn Fn(&gtk::Widget, f64, f64, PathBuf)>;

pub struct FolderTree {
    pub widget: gtk::ScrolledWindow,
    view: gtk::ListView,
    roots: gio::ListStore,
    model: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    monitor: gio::VolumeMonitor,
    show_hidden: Cell<bool>,
    /// Folder to select once its row appears, and the root to look under.
    reveal: RefCell<Option<RowKey>>,
    /// Rows to expand once they appear (restored state).
    pending: RefCell<HashSet<RowKey>>,
    /// Set while selecting programmatically so it doesn't count as navigation.
    quiet: Cell<bool>,
    on_select: RefCell<Option<SelectHandler>>,
    on_context: RefCell<Option<ContextHandler>>,
    /// Expanded-state watchers on bound rows, keyed by list item.
    bound: RefCell<HashMap<usize, (gtk::TreeListRow, glib::SignalHandlerId)>>,
}

impl FolderTree {
    pub fn new(show_hidden: bool, expanded: &[(String, String)]) -> Rc<Self> {
        let roots = gio::ListStore::new::<FolderNode>();
        // Every row gets a child model, even a leaf (whose arrow is hidden):
        // GTK asks only once, and a leaf can gain a subfolder by pasting.
        let model = gtk::TreeListModel::new(roots.clone(), false, false, |item| {
            let node = item.downcast_ref::<FolderNode>()?;
            Some(node.children().clone().upcast())
        });
        let selection = gtk::SingleSelection::builder()
            .model(&model)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let factory = gtk::SignalListItemFactory::new();
        let view = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .css_classes(["navigation-sidebar"])
            .build();
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&view)
            .css_classes(["fv-tree"])
            .build();

        let tree = Rc::new(Self {
            widget,
            view,
            roots,
            model,
            selection,
            monitor: gio::VolumeMonitor::get(),
            show_hidden: Cell::new(show_hidden),
            reveal: RefCell::default(),
            pending: RefCell::new(
                expanded
                    .iter()
                    .map(|(root, path)| (PathBuf::from(root), PathBuf::from(path)))
                    .collect(),
            ),
            quiet: Cell::new(false),
            on_select: RefCell::default(),
            on_context: RefCell::default(),
            bound: RefCell::default(),
        });
        tree.setup_factory(&factory);
        tree.connect_signals();
        tree.populate_roots();
        tree
    }

    /// Called with the folder the user picks in the tree.
    pub fn connect_selected(&self, f: impl Fn(PathBuf) + 'static) {
        self.on_select.replace(Some(Box::new(f)));
    }

    /// Called on right-click on a row, with that row's folder. Doesn't select it.
    pub fn connect_context_menu(&self, f: impl Fn(&gtk::Widget, f64, f64, PathBuf) + 'static) {
        self.on_context.replace(Some(Box::new(f)));
    }

    /// The deepest root containing `path` and its display name.
    pub fn root_for(&self, path: &Path) -> (PathBuf, String) {
        self.roots
            .iter::<FolderNode>()
            .flatten()
            .filter(|root| path.starts_with(root.path()))
            .max_by_key(|root| root.path().components().count())
            .map(|root| (root.path().to_owned(), root.name().to_owned()))
            .unwrap_or_else(|| (PathBuf::from("/"), "Computer".to_owned()))
    }

    /// Expands the tree down to `path` under its deepest root and selects it.
    pub fn reveal(self: &Rc<Self>, path: &Path) {
        let (root, _) = self.root_for(path);
        if let Some(row) = self
            .selection
            .selected_item()
            .and_downcast::<gtk::TreeListRow>()
            && let Some(node) = row.item().and_downcast::<FolderNode>()
            && node.path() == path
            && root_of(&row).path() == root
        {
            return;
        }
        self.quiet.set(true);
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        self.quiet.set(false);
        self.reveal.replace(Some((root, path.to_owned())));
        // Rows already present won't be announced again, so walk them now.
        self.rows_added(0, self.model.n_items());
    }

    pub fn set_show_hidden(self: &Rc<Self>, show: bool) {
        if self.show_hidden.replace(show) == show {
            return;
        }
        // Rebuild from scratch, keeping the expanded rows.
        let expanded = self
            .expanded_rows()
            .into_iter()
            .map(|(root, path)| (PathBuf::from(root), PathBuf::from(path)))
            .collect();
        self.pending.replace(expanded);
        self.populate_roots();
    }

    /// Currently expanded rows as (root, folder) pairs, for saving.
    pub fn expanded_rows(&self) -> Vec<(String, String)> {
        (0..self.model.n_items())
            .filter_map(|i| self.model.item(i).and_downcast::<gtk::TreeListRow>())
            .filter(|row| row.is_expanded())
            .filter_map(|row| {
                let node = row.item().and_downcast::<FolderNode>()?;
                if node.leaf() {
                    return None;
                }
                let root = root_of(&row);
                Some((
                    root.path().to_str()?.to_owned(),
                    node.path().to_str()?.to_owned(),
                ))
            })
            .collect()
    }

    fn populate_roots(&self) {
        let mut volumes: Vec<FolderNode> = self
            .monitor
            .mounts()
            .iter()
            .filter_map(mount_node)
            .collect();
        volumes.sort_by_key(|node| node.name().to_lowercase());

        let mut nodes = vec![FolderNode::new(
            glib::home_dir(),
            "Home".into(),
            themed("user-home-symbolic"),
            false,
        )];
        nodes.extend(volumes);
        nodes.push(FolderNode::new(
            "/".into(),
            "Computer".into(),
            themed("drive-harddisk-symbolic"),
            false,
        ));
        self.roots.splice(0, self.roots.n_items(), &nodes);
    }

    fn setup_factory(self: &Rc<Self>, factory: &gtk::SignalListItemFactory) {
        factory.connect_setup(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            move |_, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("tree list item");
                let icon = gtk::Image::new();
                let label = gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                content.append(&icon);
                content.append(&label);
                let expander = gtk::TreeExpander::builder()
                    .child(&content)
                    .indent_for_icon(true)
                    .build();
                // Hide the arrow once the folder is known to have no subfolders.
                item.property_expression("item")
                    .chain_property::<gtk::TreeListRow>("item")
                    .chain_property::<FolderNode>("leaf")
                    .bind(&expander, "hide-expander", Some(item));
                expander.add_controller(tree.context_click(item));
                item.set_child(Some(&expander));
            }
        ));

        factory.connect_bind(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            move |_, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("tree list item");
                let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else {
                    return;
                };
                let Some(node) = row.item().and_downcast::<FolderNode>() else {
                    return;
                };
                let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
                    return;
                };
                expander.set_list_row(Some(&row));
                let content = expander.child().expect("expander content");
                if let Some(icon) = content.first_child().and_downcast::<gtk::Image>() {
                    icon.set_from_gicon(node.icon());
                }
                if let Some(label) = content.last_child().and_downcast::<gtk::Label>() {
                    label.set_text(node.name());
                }

                // Load children when the user expands this row.
                if row.is_expanded() {
                    tree.load(&node);
                }
                let handler = row.connect_expanded_notify(glib::clone!(
                    #[weak]
                    tree,
                    move |row| {
                        if row.is_expanded()
                            && let Some(node) = row.item().and_downcast::<FolderNode>()
                        {
                            tree.load(&node);
                        }
                    }
                ));
                let key = item.as_ptr() as usize;
                if let Some((old_row, old)) = tree.bound.borrow_mut().insert(key, (row, handler)) {
                    old_row.disconnect(old);
                }
            }
        ));

        factory.connect_unbind(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            move |_, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("tree list item");
                let key = item.as_ptr() as usize;
                if let Some((row, handler)) = tree.bound.borrow_mut().remove(&key) {
                    row.disconnect(handler);
                }
                if let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() {
                    expander.set_list_row(None::<&gtk::TreeListRow>);
                }
            }
        ));
    }

    fn context_click(self: &Rc<Self>, item: &gtk::ListItem) -> gtk::GestureClick {
        let click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        click.connect_pressed(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            #[weak]
            item,
            move |gesture, _, x, y| {
                let Some(node) = item
                    .item()
                    .and_downcast::<gtk::TreeListRow>()
                    .and_then(|row| row.item())
                    .and_downcast::<FolderNode>()
                else {
                    return;
                };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                if let (Some(f), Some(widget)) =
                    (tree.on_context.borrow().as_ref(), gesture.widget())
                {
                    f(&widget, x, y, node.path().to_owned());
                }
            }
        ));
        click
    }

    fn connect_signals(self: &Rc<Self>) {
        self.model.connect_items_changed(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            move |_, position, _removed, added| tree.rows_added(position, added)
        ));

        self.selection.connect_selected_item_notify(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            move |selection| {
                if tree.quiet.get() {
                    return;
                }
                let Some(row) = selection.selected_item().and_downcast::<gtk::TreeListRow>() else {
                    return;
                };
                let Some(node) = row.item().and_downcast::<FolderNode>() else {
                    return;
                };
                tree.reveal.replace(None);
                if let Some(f) = tree.on_select.borrow().as_ref() {
                    f(node.path().to_owned());
                }
            }
        ));

        // Enter or double-click toggles expansion.
        self.view.connect_activate(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            move |_, position| {
                let Some(row) = tree.model.item(position).and_downcast::<gtk::TreeListRow>() else {
                    return;
                };
                row.set_expanded(!row.is_expanded());
                if let Some(node) = row.item().and_downcast::<FolderNode>() {
                    tree.load(&node);
                }
            }
        ));

        self.monitor.connect_mount_added(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            move |_, mount| {
                if let Some(node) = mount_node(mount) {
                    // Keep Computer last.
                    tree.roots
                        .insert(tree.roots.n_items().saturating_sub(1), &node);
                }
            }
        ));
        self.monitor.connect_mount_removed(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            move |_, mount| {
                let Some(path) = mount.root().path() else {
                    return;
                };
                let last = tree.roots.n_items().saturating_sub(1);
                let found = (1..last).find(|&i| {
                    tree.roots
                        .item(i)
                        .and_downcast::<FolderNode>()
                        .is_some_and(|node| node.path() == path)
                });
                if let Some(i) = found {
                    tree.roots.remove(i);
                }
            }
        ));
    }

    /// Expands rows on the way to the reveal target or restored from settings.
    fn rows_added(self: &Rc<Self>, position: u32, added: u32) {
        for i in position..position + added {
            let Some(row) = self.model.item(i).and_downcast::<gtk::TreeListRow>() else {
                continue;
            };
            let Some(node) = row.item().and_downcast::<FolderNode>() else {
                continue;
            };
            let key = (root_of(&row).path().to_owned(), node.path().to_owned());
            let mut expand = self.pending.borrow_mut().remove(&key);
            let mut select = false;
            if let Some((root, target)) = self.reveal.borrow().as_ref()
                && *root == key.0
                && target.starts_with(&key.1)
            {
                if *target == key.1 {
                    select = true;
                } else {
                    expand = true;
                }
            }
            if !expand && !select {
                continue;
            }
            // Defer: expanding inside items-changed would re-enter this handler.
            glib::idle_add_local_once(glib::clone!(
                #[weak(rename_to = tree)]
                self,
                move || {
                    if expand {
                        row.set_expanded(true);
                        tree.load(&node);
                    }
                    if select && tree.reveal.take().is_some() {
                        tree.quiet.set(true);
                        tree.view
                            .scroll_to(row.position(), gtk::ListScrollFlags::SELECT, None);
                        tree.quiet.set(false);
                        // The first scroll uses estimated heights for rows that were
                        // just added and can land a row short; repeat it once they have
                        // been laid out.
                        let frames = Cell::new(0);
                        tree.view.add_tick_callback(move |view, _| {
                            frames.set(frames.get() + 1);
                            if frames.get() == 1 {
                                return glib::ControlFlow::Continue;
                            }
                            view.scroll_to(row.position(), gtk::ListScrollFlags::NONE, None);
                            if frames.get() < 3 {
                                glib::ControlFlow::Continue
                            } else {
                                glib::ControlFlow::Break
                            }
                        });
                    }
                }
            ));
        }
    }

    /// Brings listed folders up to date after a file operation in the app:
    /// drops removed subfolders and adds new ones, without reading the
    /// folders again. Changes made by other apps still need a reopen.
    pub fn update_folders(&self, added: &[PathBuf], removed: &[PathBuf]) {
        let show_hidden = self.show_hidden.get();
        let added: Vec<(&Path, String)> = added
            .iter()
            .filter_map(|path| Some((path.as_path(), path.file_name()?.to_str()?.to_owned())))
            .filter(|(_, name)| show_hidden || !is_hidden(name))
            .collect();
        let removed: HashSet<&Path> = removed.iter().map(PathBuf::as_path).collect();
        if added.is_empty() && removed.is_empty() {
            return;
        }
        let mut new_nodes = Vec::new();
        // Visit the roots and every listed folder; the same folder can sit under several roots.
        let mut nodes: Vec<FolderNode> = self.roots.iter::<FolderNode>().flatten().collect();
        while let Some(node) = nodes.pop() {
            let listed = node.imp().loaded.get();
            let store = node.children();
            if listed {
                for i in (0..store.n_items()).rev() {
                    let Some(child) = store.item(i).and_downcast::<FolderNode>() else {
                        continue;
                    };
                    if removed.contains(child.path()) {
                        store.remove(i);
                    } else {
                        nodes.push(child);
                    }
                }
            }
            for (path, name) in added
                .iter()
                .filter(|(p, _)| p.parent() == Some(node.path()))
            {
                node.set_leaf(false);
                // Unlisted folders pick it up when they are expanded.
                if !listed || find(store, |n| n.path() == *path).is_some() {
                    continue;
                }
                let key = name.to_lowercase();
                let position = find(store, |n| {
                    natsort::compare(&key, &n.name().to_lowercase())
                        .then_with(|| name.as_str().cmp(n.name()))
                        .is_lt()
                })
                .unwrap_or(store.n_items());
                let new = FolderNode::new(
                    path.to_path_buf(),
                    name.clone(),
                    themed("folder-symbolic"),
                    false,
                );
                store.insert(position, &new);
                new_nodes.push(new);
            }
            if listed && store.n_items() == 0 {
                node.set_leaf(true);
            }
        }
        probe_leaves(new_nodes, show_hidden);
    }

    /// Lists `node`'s subfolders on a worker thread, once.
    fn load(&self, node: &FolderNode) {
        if node.imp().loading.replace(true) {
            return;
        }
        let weak = node.downgrade();
        let dir = node.path().to_owned();
        let show_hidden = self.show_hidden.get();
        let (tx, rx) = async_channel::unbounded();
        let started = std::time::Instant::now();
        crate::trace!("tree load start: {}", dir.display());
        std::thread::Builder::new()
            .name("tree".into())
            .spawn(move || list_subfolders(&dir, show_hidden, &tx))
            .expect("failed to spawn tree thread");

        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                let Some(node) = weak.upgrade() else {
                    return;
                };
                let store = node.children();
                match msg {
                    TreeMsg::Children(dirs) => {
                        crate::trace!(
                            "tree children: {} folders after {:.1} ms",
                            dirs.len(),
                            started.elapsed().as_secs_f64() * 1e3
                        );
                        let nodes: Vec<FolderNode> = dirs
                            .into_iter()
                            .map(|d| {
                                FolderNode::new(d.path, d.name, themed("folder-symbolic"), d.leaf)
                            })
                            .collect();
                        store.extend_from_slice(&nodes);
                        node.imp().loaded.set(true);
                        // Also hides the arrow of folders we can't read.
                        if nodes.is_empty() {
                            node.set_leaf(true);
                        }
                    }
                    TreeMsg::Leaf(index) => {
                        if let Some(node) = store.item(index).and_downcast::<FolderNode>() {
                            node.set_leaf(true);
                        }
                    }
                }
            }
        });
    }
}

/// The position of the first node in `store` that matches.
fn find(store: &gio::ListStore, f: impl Fn(&FolderNode) -> bool) -> Option<u32> {
    (0..store.n_items()).find(|&i| {
        store
            .item(i)
            .and_downcast::<FolderNode>()
            .is_some_and(|n| f(&n))
    })
}

/// Hides the arrow of newly added folders that have no subfolders.
fn probe_leaves(nodes: Vec<FolderNode>, show_hidden: bool) {
    if nodes.is_empty() {
        return;
    }
    let paths: Vec<PathBuf> = nodes.iter().map(|node| node.path().to_owned()).collect();
    let (tx, rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("tree".into())
        .spawn(move || {
            for (index, path) in paths.iter().enumerate() {
                if !has_subfolder(path, show_hidden) && tx.send_blocking(index).is_err() {
                    return;
                }
            }
        })
        .expect("failed to spawn tree thread");
    let nodes: Vec<_> = nodes.iter().map(ObjectExt::downgrade).collect();
    glib::spawn_future_local(async move {
        while let Ok(index) = rx.recv().await {
            if let Some(node) = nodes[index].upgrade() {
                node.set_leaf(true);
            }
        }
    });
}

fn root_of(row: &gtk::TreeListRow) -> FolderNode {
    let mut row = row.clone();
    while let Some(parent) = row.parent() {
        row = parent;
    }
    row.item()
        .and_downcast()
        .expect("tree rows hold folder nodes")
}

fn mount_node(mount: &gio::Mount) -> Option<FolderNode> {
    if mount.is_shadowed() {
        return None;
    }
    let path = mount.root().path()?;
    Some(FolderNode::new(
        path,
        mount.name().into(),
        mount.symbolic_icon(),
        false,
    ))
}

fn themed(name: &str) -> gio::Icon {
    gio::ThemedIcon::new(name).upcast()
}

enum TreeMsg {
    Children(Vec<SubFolder>),
    Leaf(u32),
}

struct SubFolder {
    path: PathBuf,
    name: String,
    key: String,
    leaf: bool,
    probe: bool,
}

fn list_subfolders(dir: &Path, show_hidden: bool, tx: &async_channel::Sender<TreeMsg>) {
    let Ok(entries) = fs::read_dir(dir) else {
        let _ = tx.send_blocking(TreeMsg::Children(Vec::new()));
        return;
    };
    let mut dirs = Vec::new();
    for dirent in entries.flatten() {
        let name = dirent.file_name().to_string_lossy().into_owned();
        if !show_hidden && is_hidden(&name) {
            continue;
        }
        let Ok(file_type) = dirent.file_type() else {
            continue;
        };
        let metadata = if file_type.is_dir() {
            dirent.metadata().ok()
        } else if file_type.is_symlink() {
            fs::metadata(dirent.path()).ok().filter(|m| m.is_dir())
        } else {
            None
        };
        let Some(metadata) = metadata else { continue };
        // On ext4 and similar, a directory's link count is 2 + its subfolder
        // count. NTFS-3G and btrfs always report 1, so those need a peek.
        let nlink = metadata.nlink();
        dirs.push(SubFolder {
            path: dirent.path(),
            key: name.to_lowercase(),
            name,
            leaf: nlink == 2,
            probe: nlink < 2,
        });
    }
    dirs.sort_by(|a, b| natsort::compare(&a.key, &b.key).then_with(|| a.name.cmp(&b.name)));

    let probes: Vec<(u32, PathBuf)> = dirs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.probe)
        .map(|(i, d)| (i as u32, d.path.clone()))
        .collect();
    if tx.send_blocking(TreeMsg::Children(dirs)).is_err() {
        return;
    }
    for (index, path) in probes {
        if tx.is_closed() {
            return;
        }
        if !has_subfolder(&path, show_hidden) && tx.send_blocking(TreeMsg::Leaf(index)).is_err() {
            return;
        }
    }
}

fn has_subfolder(dir: &Path, show_hidden: bool) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|dirent| {
        dirent.file_type().is_ok_and(|t| t.is_dir())
            && (show_hidden || !is_hidden(&dirent.file_name().to_string_lossy()))
    })
}

mod imp {
    use std::cell::OnceCell;

    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::FolderNode)]
    pub struct FolderNode {
        /// True once the folder is known to have no subfolders.
        #[property(get, set)]
        leaf: Cell<bool>,
        pub path: OnceCell<PathBuf>,
        pub name: OnceCell<String>,
        pub icon: OnceCell<gio::Icon>,
        pub children: OnceCell<gio::ListStore>,
        /// Listing requested (on first expand).
        pub loading: Cell<bool>,
        /// Listing arrived: `children` holds the subfolders.
        pub loaded: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FolderNode {
        const NAME: &'static str = "FvFolderNode";
        type Type = super::FolderNode;
    }

    #[glib::derived_properties]
    impl ObjectImpl for FolderNode {}
}

glib::wrapper! {
    pub struct FolderNode(ObjectSubclass<imp::FolderNode>);
}

impl FolderNode {
    fn new(path: PathBuf, name: String, icon: gio::Icon, leaf: bool) -> Self {
        let node: Self = glib::Object::builder().property("leaf", leaf).build();
        let imp = node.imp();
        let _ = imp.path.set(path);
        let _ = imp.name.set(name);
        let _ = imp.icon.set(icon);
        node
    }

    pub fn path(&self) -> &Path {
        self.imp().path.get().expect("folder node path")
    }

    pub fn name(&self) -> &str {
        self.imp().name.get().expect("folder node name")
    }

    fn icon(&self) -> &gio::Icon {
        self.imp().icon.get().expect("folder node icon")
    }

    /// The child model; filled by `FolderTree::load` when first expanded.
    fn children(&self) -> &gio::ListStore {
        self.imp()
            .children
            .get_or_init(gio::ListStore::new::<FolderNode>)
    }
}
