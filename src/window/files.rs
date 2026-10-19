//! Cut, copy, paste, rename and delete in the grid: the actions, their
//! shortcuts and context menus (grid tiles, and Paste Into Folder on tree
//! rows), the clipboard, and running `fileops` jobs with progress, in-place
//! grid and tree updates and undo.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene};

use super::{Window, display_name};
use crate::fileops::{self, Op, Report};
use crate::grid;
use crate::media::Kind;
use crate::model::FileItem;
use crate::scan;

/// GNOME's clipboard format telling a paste whether to copy or move.
const COPIED_FILES: &str = "x-special/gnome-copied-files";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Job {
    Copy,
    Move,
    Trash,
    Delete,
    Rename,
    Restore,
}

/// Changes reported by a running job, applied to the grid in batches.
#[derive(Default)]
struct Pending {
    added: Vec<scan::Entry>,
    removed: HashSet<PathBuf>,
    /// Where the first removed item sat, so the selection can move on from there.
    gap: Option<u32>,
    /// Items the job added, to select when it finishes.
    new_ids: HashSet<u64>,
}

impl Window {
    pub(super) fn setup_file_actions(self: &Rc<Self>) {
        let add = |name: &str, run: fn(&Rc<Self>)| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_, _| run(&this)
            ));
            self.window.add_action(&action);
        };
        add("cut", |this| this.copy_selection(true));
        add("copy", |this| this.copy_selection(false));
        add("paste", |this| this.paste());
        let paste_into =
            gio::SimpleAction::new("paste-into", Some(&PathBuf::static_variant_type()));
        paste_into.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, folder| {
                if let Some(folder) = folder.and_then(PathBuf::from_variant) {
                    this.paste_into(folder);
                }
            }
        ));
        self.window.add_action(&paste_into);
        add("rename", |this| this.rename_selected());
        add("trash", |this| {
            let paths = this.selected_paths();
            if !paths.is_empty() {
                this.run(Op::Trash(paths), Job::Trash);
            }
        });
        add("delete", |this| {
            let paths = this.selected_paths();
            if !paths.is_empty() {
                this.confirm_delete(paths, false);
            }
        });

        // Shortcuts live on the grid, not the window: window-wide ones would
        // reach the app before a text field, so Delete or Backspace while
        // typing a new name would act on files.
        let grid_keys = shortcuts(&[
            ("<Control>x", "win.cut"),
            ("<Control>c", "win.copy"),
            ("<Control>v", "win.paste"),
            ("F2", "win.rename"),
            ("Delete", "win.trash"),
            ("KP_Delete", "win.trash"),
            ("<Shift>Delete", "win.delete"),
            ("<Shift>KP_Delete", "win.delete"),
        ]);
        self.grid.scroller.add_controller(grid_keys);
        let pane_keys = shortcuts(&[("BackSpace", "win.go-up"), ("<Alt>Up", "win.go-up")]);
        self.paned.add_controller(pane_keys);

        self.grid.connect_context_menu(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |widget, x, y| this.show_context_menu(widget, x, y)
        ));
        self.tree.connect_context_menu(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |widget, x, y, folder| {
                let menu = gio::Menu::new();
                menu.append_item(&paste_into_item(&folder));
                this.popup_menu(&menu, widget, x, y);
            }
        ));
        self.selection.connect_selection_changed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _, _| this.update_file_actions()
        ));
        self.window.clipboard().connect_changed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.update_file_actions()
        ));
        self.update_file_actions();
    }

    fn update_file_actions(&self) {
        let selected = self.selection.selection().size();
        let enable = |name: &str, enabled: bool| {
            if let Some(action) = self
                .window
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(enabled);
            }
        };
        for name in ["cut", "copy", "trash", "delete"] {
            enable(name, selected > 0);
        }
        enable("rename", selected == 1);
        let formats = self.window.clipboard().formats();
        let files = formats.contains_type(gdk::FileList::static_type())
            || formats.contain_mime_type("text/uri-list")
            || formats.contain_mime_type(COPIED_FILES);
        enable("paste", files);
        enable("paste-into", files);
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        grid::selected(self.selection.upcast_ref())
            .map(|item| item.entry().path.clone())
            .collect()
    }

    fn show_context_menu(self: &Rc<Self>, widget: &gtk::Widget, x: f64, y: f64) {
        let menu = match self.selection.selection().size() {
            0 => {
                let menu = gio::Menu::new();
                menu.append_item(&menu_item("Paste", "win.paste", "<Control>v"));
                menu
            }
            1 => {
                let folder = grid::selected(self.selection.upcast_ref())
                    .next()
                    .filter(|item| item.entry().kind == Kind::Folder)
                    .map(|item| item.entry().path.clone());
                item_menu(folder.as_deref())
            }
            _ => item_menu(None),
        };
        self.popup_menu(&menu, widget, x, y);
    }

    /// Opens `menu` at the point (`x`, `y`) in `widget`.
    fn popup_menu(&self, menu: &gio::Menu, widget: &gtk::Widget, x: f64, y: f64) {
        let popover = gtk::PopoverMenu::from_model(Some(menu));
        popover.set_parent(&self.content);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        let point = widget
            .compute_point(&self.content, &graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| graphene::Point::new(x as f32, y as f32));
        popover.set_pointing_to(Some(&gdk::Rectangle::new(
            point.x() as i32,
            point.y() as i32,
            1,
            1,
        )));
        // Unparent later: the chosen item's action runs after "closed".
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }

    fn copy_selection(&self, cut: bool) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
        let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
        let copied = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
        let provider = gdk::ContentProvider::new_union(&[
            gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value()),
            gdk::ContentProvider::for_bytes(
                COPIED_FILES,
                &glib::Bytes::from_owned(copied.into_bytes()),
            ),
        ]);
        if self.window.clipboard().set_content(Some(&provider)).is_ok() {
            self.clipboard.replace(Some((paths, cut)));
        }
    }

    /// Pastes files from the clipboard into this folder.
    fn paste(self: &Rc<Self>) {
        let Some(folder) = self.current.borrow().clone() else {
            return;
        };
        self.paste_into(folder);
    }

    /// Pastes files from the clipboard (ours or another app's) into `folder`.
    fn paste_into(self: &Rc<Self>, folder: PathBuf) {
        let clipboard = self.window.clipboard();
        let ours = self.clipboard.borrow().clone();
        if clipboard.is_local()
            && let Some((paths, cut)) = ours
        {
            self.paste_paths(paths, cut, folder);
            return;
        }
        if clipboard.formats().contain_mime_type(COPIED_FILES) {
            // Says "cut" or "copy", then one URI per line.
            clipboard.read_async(
                &[COPIED_FILES],
                glib::Priority::DEFAULT,
                None::<&gio::Cancellable>,
                glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    move |result| {
                        let Ok((stream, _)) = result else {
                            return;
                        };
                        let buffer = gio::MemoryOutputStream::new_resizable();
                        buffer.clone().splice_async(
                            &stream,
                            gio::OutputStreamSpliceFlags::CLOSE_SOURCE
                                | gio::OutputStreamSpliceFlags::CLOSE_TARGET,
                            glib::Priority::DEFAULT,
                            None::<&gio::Cancellable>,
                            move |result| {
                                if result.is_err() {
                                    return;
                                }
                                let bytes = buffer.steal_as_bytes();
                                let text = String::from_utf8_lossy(&bytes);
                                let mut lines = text.lines();
                                let cut = lines.next().map(str::trim) == Some("cut");
                                let paths = lines
                                    .filter_map(|uri| gio::File::for_uri(uri.trim()).path())
                                    .collect();
                                this.paste_paths(paths, cut, folder);
                            },
                        );
                    }
                ),
            );
        } else {
            clipboard.read_value_async(
                gdk::FileList::static_type(),
                glib::Priority::DEFAULT,
                None::<&gio::Cancellable>,
                glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    move |result| {
                        let Some(list) = result.ok().and_then(|v| v.get::<gdk::FileList>().ok())
                        else {
                            return;
                        };
                        let paths = list.files().iter().filter_map(|f| f.path()).collect();
                        this.paste_paths(paths, false, folder);
                    }
                ),
            );
        }
    }

    fn paste_paths(self: &Rc<Self>, paths: Vec<PathBuf>, cut: bool, folder: PathBuf) {
        if paths.is_empty() {
            return;
        }
        if cut {
            // A cut is used up by pasting it, as in Files.
            self.window
                .clipboard()
                .set_content(None::<&gdk::ContentProvider>)
                .ok();
            self.clipboard.replace(None);
            self.run(Op::Move(paths, folder), Job::Move);
        } else {
            self.run(Op::Copy(paths, folder), Job::Copy);
        }
    }

    fn rename_selected(self: &Rc<Self>) {
        let items: Vec<FileItem> = grid::selected(self.selection.upcast_ref()).collect();
        let [item] = items.as_slice() else {
            return;
        };
        let entry = item.entry();
        let (path, name) = (entry.path.clone(), entry.name.clone());
        let field = gtk::Entry::builder()
            .text(&name)
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::builder()
            .heading("Rename")
            .extra_child(&field)
            .default_response("rename")
            .close_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("rename", false);
        field.connect_changed(glib::clone!(
            #[weak]
            dialog,
            #[strong]
            name,
            move |field| dialog.set_response_enabled("rename", valid_name(&field.text(), &name))
        ));
        dialog.connect_response(
            Some("rename"),
            glib::clone!(
                #[weak(rename_to = this)]
                self,
                #[weak]
                field,
                move |_, _| this.run(Op::Rename(path.clone(), field.text().into()), Job::Rename)
            ),
        );
        dialog.present(Some(&self.window));

        // Select the name without its extension, as file managers do.
        let stem = match name.rfind('.') {
            Some(dot) if dot > 0 && entry.kind != Kind::Folder => name[..dot].chars().count(),
            _ => name.chars().count(),
        };
        // GTK selects all text when the field gets focus, which happens as the
        // dialog appears; narrow the selection right after that.
        if let Some(text) = field.delegate() {
            let handler = Rc::new(Cell::new(None));
            let id = text.connect_has_focus_notify(glib::clone!(
                #[strong]
                handler,
                #[weak]
                field,
                move |text| {
                    if !text.has_focus() {
                        return;
                    }
                    glib::idle_add_local_once(move || field.select_region(0, stem as i32));
                    if let Some(id) = handler.take() {
                        text.disconnect(id);
                    }
                }
            ));
            handler.set(Some(id));
        }
    }

    fn confirm_delete(self: &Rc<Self>, paths: Vec<PathBuf>, no_trash: bool) {
        let heading = match paths.as_slice() {
            [one] => format!(
                "Delete “{}” permanently?",
                one.file_name().unwrap_or_default().to_string_lossy()
            ),
            many => format!("Delete {} items permanently?", many.len()),
        };
        let body = if no_trash {
            "This drive has no Trash, so they can't be restored."
        } else {
            "They can't be restored."
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body(body)
            .default_response("cancel")
            .close_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.connect_response(
            Some("delete"),
            glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_, _| this.run(Op::Delete(paths.clone()), Job::Delete)
            ),
        );
        dialog.present(Some(&self.window));
    }

    /// Moves files to the Trash (with Undo), e.g. from the fullscreen viewer.
    pub(super) fn trash(self: &Rc<Self>, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            self.run(Op::Trash(paths), Job::Trash);
        }
    }

    /// Runs a file operation, showing progress and updating the grid in place.
    fn run(self: &Rc<Self>, op: Op, job: Job) {
        let verb = op.verb();
        // Pasting elsewhere (a tree folder or folder tile) changes nothing in
        // view, so the toast says where the files went.
        let into = match &op {
            Op::Copy(_, dir) | Op::Move(_, dir) if self.current.borrow().as_ref() != Some(dir) => {
                format!(" into “{}”", display_name(dir))
            }
            _ => String::new(),
        };
        let reports = fileops::start(op);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut pending = Pending::default();
            let mut gone: Vec<PathBuf> = Vec::new();
            let mut new_folders: Vec<PathBuf> = Vec::new();
            let mut failures: Vec<(PathBuf, String)> = Vec::new();
            let mut last_flush = Instant::now();
            while let Ok(report) = reports.recv().await {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                match report {
                    Report::Progress { done, total } => {
                        if last_flush.elapsed() > Duration::from_millis(200) {
                            this.apply(&mut pending);
                            last_flush = Instant::now();
                        }
                        if total > 1 {
                            this.count_label
                                .set_text(&format!("{verb} {} of {total}…", done + 1));
                        }
                    }
                    Report::Added(entry) => {
                        if entry.kind == Kind::Folder {
                            new_folders.push(entry.path.clone());
                        }
                        pending.added.push(entry);
                    }
                    Report::Removed(path) => {
                        gone.push(path.clone());
                        pending.removed.insert(path);
                    }
                    Report::Moved { from, to } => this.thumbnailer.file_moved(&from, &to),
                    Report::Failed { path, error } => failures.push((path, error)),
                    Report::NoTrash(paths) => this.confirm_delete(paths, true),
                    Report::Done { succeeded } => {
                        this.apply(&mut pending);
                        this.tree.update_folders(&new_folders, &gone);
                        this.finish(job, succeeded, &into, gone, &failures, &pending);
                        return;
                    }
                }
            }
        });
    }

    /// Applies a job's additions and removals to the grid.
    fn apply(&self, pending: &mut Pending) {
        let (mut count, mut bytes) = self.totals.get();
        if !pending.removed.is_empty() {
            if pending.gap.is_none() {
                pending.gap = (0..self.selection.n_items()).find(|&position| {
                    self.selection
                        .item(position)
                        .and_downcast::<FileItem>()
                        .is_some_and(|item| pending.removed.contains(&item.entry().path))
                });
            }
            for index in (0..self.store.n_items()).rev() {
                let Some(item) = self.store.item(index).and_downcast::<FileItem>() else {
                    continue;
                };
                if pending.removed.contains(&item.entry().path) {
                    count = count.saturating_sub(1);
                    bytes = bytes.saturating_sub(item.entry().size);
                    self.store.remove(index);
                }
            }
            pending.removed.clear();
        }

        let folder = self.current.borrow().clone();
        let show_hidden = self.show_hidden.get();
        let mut entries: Vec<scan::Entry> = pending
            .added
            .drain(..)
            .filter(|e| e.path.parent() == folder.as_deref())
            .filter(|e| show_hidden || !scan::is_hidden(&e.name))
            .collect();
        if !entries.is_empty() {
            let present: HashSet<PathBuf> = self
                .store
                .iter::<FileItem>()
                .flatten()
                .map(|item| item.entry().path.clone())
                .collect();
            entries.retain(|e| !present.contains(&e.path));
            count += entries.len() as u32;
            bytes += entries.iter().map(|e| e.size).sum::<u64>();
            let items: Vec<FileItem> = entries.into_iter().map(FileItem::new).collect();
            self.thumbnailer.register(&items);
            // Dimensions and dates for the status bar and metadata sorts.
            self.thumbnailer.want_metadata(&items);
            pending.new_ids.extend(items.iter().map(FileItem::id));
            self.store.extend_from_slice(&items);
        }
        self.totals.set((count, bytes));
        self.update_count_status();
        if self.store.n_items() == 0 {
            self.empty.set_title("Empty Folder");
            self.empty.set_description(None);
            self.stack.set_visible_child(&self.empty);
        } else {
            self.stack.set_visible_child(&self.grid.scroller);
        }
    }

    fn finish(
        self: &Rc<Self>,
        job: Job,
        succeeded: usize,
        into: &str,
        gone: Vec<PathBuf>,
        failures: &[(PathBuf, String)],
        pending: &Pending,
    ) {
        self.update_count_status();
        // Select what was pasted, renamed or restored; after a removal,
        // move on to the next item.
        let n = self.selection.n_items();
        if !pending.new_ids.is_empty() {
            self.selection.unselect_all();
            let mut first = None;
            for position in 0..n {
                let Some(item) = self.selection.item(position).and_downcast::<FileItem>() else {
                    continue;
                };
                if pending.new_ids.contains(&item.id()) {
                    self.selection.select_item(position, false);
                    first.get_or_insert(position);
                }
            }
            if let Some(position) = first {
                self.grid
                    .view
                    .scroll_to(position, gtk::ListScrollFlags::FOCUS, None);
            }
        } else if let Some(gap) = pending.gap
            && n > 0
        {
            let position = gap.min(n - 1);
            self.grid.view.scroll_to(
                position,
                gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
                None,
            );
        }

        let items = |n: usize| {
            if n == 1 {
                "1 item".to_owned()
            } else {
                format!("{n} items")
            }
        };
        let message = match job {
            Job::Copy => format!("Pasted {}{into}", items(succeeded)),
            Job::Move => format!("Moved {}{into}", items(succeeded)),
            Job::Trash => format!("Moved {} to Trash", items(succeeded)),
            Job::Delete => format!("Deleted {}", items(succeeded)),
            Job::Restore => format!("Restored {}", items(succeeded)),
            Job::Rename => String::new(),
        };
        if succeeded > 0 && !message.is_empty() {
            let toast = adw::Toast::new(&message);
            if job == Job::Trash {
                toast.set_button_label(Some("Undo"));
                toast.connect_button_clicked(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    move |_| this.run(Op::Restore(gone.clone()), Job::Restore)
                ));
            }
            self.toasts.add_toast(toast);
        }
        if let Some((path, error)) = failures.first() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let more = match failures.len() {
                1 => String::new(),
                n => format!(" (and {} more)", n - 1),
            };
            let toast = adw::Toast::builder()
                .title(format!("Couldn't handle “{name}”: {error}{more}"))
                .timeout(8)
                .build();
            self.toasts.add_toast(toast);
        }
    }
}

fn shortcuts(bindings: &[(&str, &str)]) -> gtk::ShortcutController {
    let controller = gtk::ShortcutController::new();
    for (trigger, action) in bindings {
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(gtk::NamedAction::new(action)),
        ));
    }
    controller
}

/// The menu for selected tiles; `folder` when it is a single folder.
fn item_menu(folder: Option<&Path>) -> gio::Menu {
    let edit = gio::Menu::new();
    edit.append_item(&menu_item("Cut", "win.cut", "<Control>x"));
    edit.append_item(&menu_item("Copy", "win.copy", "<Control>c"));
    edit.append_item(&menu_item("Paste", "win.paste", "<Control>v"));
    if let Some(folder) = folder {
        edit.append_item(&paste_into_item(folder));
    }
    let file = gio::Menu::new();
    file.append_item(&menu_item("Rename…", "win.rename", "F2"));
    let trash = gio::Menu::new();
    trash.append_item(&menu_item("Move to Trash", "win.trash", "Delete"));
    let menu = gio::Menu::new();
    menu.append_section(None, &edit);
    menu.append_section(None, &file);
    menu.append_section(None, &trash);
    menu
}

fn paste_into_item(folder: &Path) -> gio::MenuItem {
    let item = gio::MenuItem::new(Some("Paste Into Folder"), None);
    item.set_action_and_target_value(Some("win.paste-into"), Some(&folder.to_variant()));
    item
}

/// A menu item showing its (grid-local) shortcut.
fn menu_item(label: &str, action: &str, accel: &str) -> gio::MenuItem {
    let item = gio::MenuItem::new(Some(label), Some(action));
    item.set_attribute_value("accel", Some(&accel.to_variant()));
    item
}

fn valid_name(name: &str, old: &str) -> bool {
    !name.is_empty() && name != old && name != "." && name != ".." && !name.contains(['/', '\0'])
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn rename_validation() {
        assert!(valid_name("new.jpg", "old.jpg"));
        assert!(!valid_name("old.jpg", "old.jpg"));
        assert!(!valid_name("", "old.jpg"));
        assert!(!valid_name("a/b.jpg", "old.jpg"));
        assert!(!valid_name("..", "old.jpg"));
    }
}
