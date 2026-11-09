//! Right-click → Properties: a file's or folder's details, or a summary of
//! several items. What needs the disk (dates, camera details, a clip's
//! length, a folder's contents) is read on a worker thread and fills in as
//! it arrives; a folder's count runs until it's done or the dialog closes.

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::{Window, display_name, group_digits};
use crate::decode;
use crate::grid;
use crate::media::{Kind, Meta};
use crate::model::FileItem;

/// How often a folder count reports progress.
const REPORT_EVERY: Duration = Duration::from_millis(150);

/// Width of the dialog's content.
const PAGE_WIDTH: i32 = 420;

/// Largest edge of the thumbnail at the top of the dialog.
const PICTURE_EDGE: f32 = 192.0;

/// One item the dialog describes, with what the grid already knows about it.
struct Subject {
    path: PathBuf,
    name: String,
    kind: Kind,
    /// From the folder scan; folders' sizes are counted.
    size: Option<u64>,
    mtime: Option<i64>,
    /// From the extension, and from the contents once read.
    format: &'static str,
    meta: Option<Meta>,
    content_type: Option<String>,
    texture: Option<gdk::Texture>,
}

impl Subject {
    fn from_item(item: &FileItem) -> Self {
        let entry = item.entry();
        Self {
            path: entry.path.clone(),
            name: entry.name.clone(),
            kind: entry.kind,
            size: (entry.kind != Kind::Folder).then_some(entry.size),
            mtime: Some(entry.mtime),
            format: entry.format,
            meta: item.meta(),
            content_type: entry.content_type.clone(),
            texture: item.texture(),
        }
    }

    /// A folder known only by its path (the open folder, a tree row).
    fn folder(path: PathBuf) -> Self {
        Self {
            name: display_name(&path),
            path,
            kind: Kind::Folder,
            size: None,
            mtime: None,
            format: "",
            meta: None,
            content_type: None,
            texture: None,
        }
    }
}

/// What the worker found.
enum Update {
    Modified(i64),
    Created(i64),
    Meta(Meta),
    Duration(f64),
    Camera(Vec<(&'static str, String)>),
    Contents(Count, bool),
}

/// Files, folders and bytes inside the counted folders.
#[derive(Clone, Copy, Default)]
struct Count {
    files: u64,
    folders: u64,
    bytes: u64,
    /// Some folder couldn't be listed.
    unreadable: bool,
}

/// The rows the worker's results go into (hidden until they have a value).
struct Rows {
    size: adw::ActionRow,
    contents: adw::ActionRow,
    dimensions: adw::ActionRow,
    duration: adw::ActionRow,
    taken: adw::ActionRow,
    modified: adw::ActionRow,
    created: adw::ActionRow,
    camera: adw::PreferencesGroup,
    /// One folder: its contents. Several items: the selected files' bytes,
    /// to which the selected folders' contents add.
    single_folder: bool,
    base_bytes: u64,
    kind: Kind,
}

impl Window {
    /// Properties of the selected items, or of the open folder if none are.
    pub(super) fn show_selection_properties(self: &Rc<Self>) {
        let subjects: Vec<Subject> = grid::selected(self.selection.upcast_ref())
            .map(|item| Subject::from_item(&item))
            .collect();
        if !subjects.is_empty() {
            self.show_properties(subjects);
        } else if let Some(folder) = self.current.borrow().clone() {
            self.show_properties(vec![Subject::folder(folder)]);
        }
    }

    pub(super) fn show_folder_properties(self: &Rc<Self>, folder: PathBuf) {
        self.show_properties(vec![Subject::folder(folder)]);
    }

    fn show_properties(&self, subjects: Vec<Subject>) {
        let single = subjects.len() == 1;
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(6)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        page.append(&header(&subjects));

        let group = adw::PreferencesGroup::new();
        let add = |title: &str, value: Option<String>| {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(value.as_deref().unwrap_or_default())
                .use_markup(false)
                .subtitle_selectable(true)
                .visible(value.is_some())
                .css_classes(["fv-property"])
                .build();
            group.add(&row);
            row
        };
        let first = &subjects[0];
        let kind = if single { first.kind } else { Kind::Other };
        // The first row takes the keyboard focus: the first selectable
        // label would, and it selects all its text when it does.
        let first_row = if single {
            add("Type", Some(type_text(first)))
        } else {
            add("Contents", Some(selection_text(&subjects)))
        };
        let base_bytes: u64 = subjects.iter().filter_map(|s| s.size).sum();
        let has_folders = subjects.iter().any(|s| s.kind == Kind::Folder);
        let size = add(
            if single { "Size" } else { "Total size" },
            Some(if has_folders {
                format!("{}…", glib::format_size(base_bytes))
            } else {
                size_text(base_bytes)
            }),
        );
        let contents = add("Contents", (single && has_folders).then(|| "…".to_owned()));
        let dimensions = add(
            "Dimensions",
            single
                .then(|| first.meta.map(|m| dimensions_text(&m, kind)))
                .flatten(),
        );
        let duration = add("Duration", None);
        let taken = add(
            if kind == Kind::Video {
                "Recorded"
            } else {
                "Date taken"
            },
            single
                .then(|| first.meta.and_then(|m| m.taken))
                .flatten()
                .and_then(date_text),
        );
        let modified = add(
            "Modified",
            single.then_some(first.mtime).flatten().and_then(date_text),
        );
        let created = add("Created", None);
        let parent = first.path.parent().map(Path::to_path_buf);
        let same_parent = subjects
            .iter()
            .all(|s| s.path.parent() == parent.as_deref());
        if let Some(parent) = parent.filter(|_| same_parent) {
            add("Location", Some(parent.display().to_string()));
        }
        page.append(&group);

        let camera = adw::PreferencesGroup::builder()
            .title("Camera")
            .visible(false)
            .build();
        page.append(&camera);

        let rows = Rows {
            size,
            contents,
            dimensions,
            duration,
            taken,
            modified,
            created,
            camera,
            single_folder: single && first.kind == Kind::Folder,
            base_bytes,
            kind,
        };

        // Labels wrap at this width rather than widening the dialog.
        let clamp = adw::Clamp::builder()
            .maximum_size(PAGE_WIDTH)
            .tightening_threshold(PAGE_WIDTH)
            .child(&page)
            .build();
        page.set_width_request(PAGE_WIDTH);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&scroller));
        let dialog = adw::Dialog::builder()
            .title("Properties")
            // Rows the worker fills in grow the dialog (up to the window's height).
            .follows_content_size(true)
            .child(&view)
            .build();
        dialog.set_focus(Some(&first_row));

        let cancel = Arc::new(AtomicBool::new(false));
        dialog.connect_closed({
            let cancel = cancel.clone();
            move |_| cancel.store(true, Ordering::Relaxed)
        });
        let (tx, rx) = async_channel::unbounded();
        let job = Job::new(&subjects);
        let spawned = std::thread::Builder::new()
            .name("properties".into())
            .spawn(move || job.run(&tx, &cancel));
        if spawned.is_ok() {
            glib::spawn_future_local(async move {
                while let Ok(update) = rx.recv().await {
                    rows.apply(update);
                }
            });
        }
        dialog.present(Some(&self.window));
    }
}

/// The thumbnail or icon and the name; for several items, how many.
fn header(subjects: &[Subject]) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let title = if let [subject] = subjects {
        if let Some(texture) = &subject.texture {
            let (width, height) = (texture.width() as f32, texture.height() as f32);
            let scale = (PICTURE_EDGE / width.max(height)).min(1.0);
            let picture = gtk::Picture::builder()
                .paintable(texture)
                .can_shrink(true)
                .content_fit(gtk::ContentFit::Contain)
                .halign(gtk::Align::Center)
                .width_request((width * scale).round() as i32)
                .height_request((height * scale).round() as i32)
                .build();
            header.append(&picture);
        } else {
            let image = gtk::Image::from_gicon(&icon(subject));
            image.set_pixel_size(128);
            header.append(&image);
        }
        subject.name.clone()
    } else {
        format!("{} items", group_digits(subjects.len() as u64))
    };
    let name = gtk::Label::builder()
        .label(title)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .justify(gtk::Justification::Center)
        .selectable(true)
        .css_classes(["title-3"])
        .build();
    header.append(&name);
    header
}

fn icon(subject: &Subject) -> gio::Icon {
    if subject.kind == Kind::Folder {
        return gio::ThemedIcon::new("folder").upcast();
    }
    let content_type = subject.content_type.clone().unwrap_or_else(|| {
        gio::content_type_guess(Some(&subject.path), None)
            .0
            .to_string()
    });
    gio::content_type_get_icon(&content_type)
}

/// "JPEG image", "Canon CR3 raw image", "MPEG-4 video", ...
fn type_text(subject: &Subject) -> String {
    if subject.kind == Kind::Folder {
        return "Folder".to_owned();
    }
    let (content_type, uncertain) = match &subject.content_type {
        Some(content_type) => (content_type.clone(), false),
        None => {
            let (guess, uncertain) = gio::content_type_guess(Some(&subject.path), None);
            (guess.to_string(), uncertain)
        }
    };
    let mut text = if uncertain || content_type == "application/octet-stream" {
        match subject.format {
            "File" | "" => "File".to_owned(),
            format => format!("{format} file"),
        }
    } else {
        gio::content_type_get_description(&content_type).to_string()
    };
    // A file whose contents aren't what its name says (a PNG named .jpg).
    if let Some(meta) = subject.meta
        && subject.kind.is_media()
        && meta.format != subject.format
    {
        text.push_str(&format!(" (contents: {})", meta.format));
    }
    text
}

/// "3 photos, 1 video, 2 folders".
fn selection_text(subjects: &[Subject]) -> String {
    let mut counts = [0u64; Kind::ALL.len()];
    for subject in subjects {
        counts[subject.kind as usize] += 1;
    }
    let only_files = counts[Kind::Other as usize] as usize == subjects.len();
    let labels = [
        (Kind::Image, "photo", "photos"),
        (Kind::Video, "video", "videos"),
        (Kind::Folder, "folder", "folders"),
        if only_files {
            (Kind::Other, "file", "files")
        } else {
            (Kind::Other, "other file", "other files")
        },
    ];
    labels
        .iter()
        .filter(|(kind, _, _)| counts[*kind as usize] > 0)
        .map(|&(kind, one, many)| plural(counts[kind as usize], one, many))
        .collect::<Vec<_>>()
        .join(", ")
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

/// "27.0 MB (27,012,345 bytes)".
fn size_text(bytes: u64) -> String {
    let short = glib::format_size(bytes);
    if bytes < 1000 {
        short.to_string()
    } else {
        format!("{short} ({} bytes)", group_digits(bytes))
    }
}

/// "5536 × 3692 (20.4 MP)"; videos without the megapixels.
fn dimensions_text(meta: &Meta, kind: Kind) -> String {
    let size = format!("{} × {}", meta.width, meta.height);
    if kind == Kind::Video {
        return size;
    }
    let megapixels = f64::from(meta.width) * f64::from(meta.height) / 1e6;
    if megapixels >= 0.1 {
        format!("{size} ({megapixels:.1} MP)")
    } else {
        size
    }
}

/// "1:23" or "1:02:05".
fn duration_text(seconds: f64) -> String {
    let total = seconds.round() as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn date_text(time: i64) -> Option<String> {
    glib::DateTime::from_unix_local(time)
        .ok()?
        .format("%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|s| s.to_string())
}

/// "1,234 files, 56 folders", with "…" while counting.
fn contents_text(count: &Count, done: bool) -> String {
    if done && count.files == 0 && count.folders == 0 {
        return if count.unreadable {
            "Couldn't be read".to_owned()
        } else {
            "Empty".to_owned()
        };
    }
    let mut text = plural(count.files, "file", "files");
    if count.folders > 0 {
        text.push_str(&format!(", {}", plural(count.folders, "folder", "folders")));
    }
    if !done {
        text.push('…');
    } else if count.unreadable {
        text.push_str(" (some folders couldn't be read)");
    }
    text
}

impl Rows {
    fn apply(&self, update: Update) {
        let set = |row: &adw::ActionRow, value: String| {
            row.set_subtitle(&value);
            row.set_visible(true);
        };
        match update {
            Update::Modified(time) => {
                if let Some(text) = date_text(time) {
                    set(&self.modified, text);
                }
            }
            Update::Created(time) => {
                if let Some(text) = date_text(time) {
                    set(&self.created, text);
                }
            }
            Update::Meta(meta) => {
                set(&self.dimensions, dimensions_text(&meta, self.kind));
                if let Some(text) = meta.taken.and_then(date_text) {
                    set(&self.taken, text);
                }
            }
            Update::Duration(seconds) => set(&self.duration, duration_text(seconds)),
            Update::Camera(fields) => {
                for (name, value) in fields {
                    let row = adw::ActionRow::builder()
                        .title(name)
                        .subtitle(value)
                        .use_markup(false)
                        .subtitle_selectable(true)
                        .css_classes(["fv-property"])
                        .build();
                    self.camera.add(&row);
                    self.camera.set_visible(true);
                }
            }
            Update::Contents(count, done) => {
                let bytes = self.base_bytes + count.bytes;
                set(
                    &self.size,
                    if done {
                        size_text(bytes)
                    } else {
                        format!("{}…", glib::format_size(bytes))
                    },
                );
                if self.single_folder {
                    set(&self.contents, contents_text(&count, done));
                }
            }
        }
    }
}

/// The disk work for one dialog, on its own thread.
struct Job {
    /// One item: its path, kind, format and whether its metadata is known.
    single: Option<(PathBuf, Kind, &'static str, bool)>,
    folders: Vec<PathBuf>,
}

impl Job {
    fn new(subjects: &[Subject]) -> Self {
        let single = match subjects {
            [s] => Some((s.path.clone(), s.kind, s.format, s.meta.is_some())),
            _ => None,
        };
        let folders = subjects
            .iter()
            .filter(|s| s.kind == Kind::Folder)
            .map(|s| s.path.clone())
            .collect();
        Self { single, folders }
    }

    fn run(self, tx: &async_channel::Sender<Update>, cancel: &AtomicBool) {
        let send = |update| {
            let _ = tx.send_blocking(update);
        };
        if let Some((path, kind, format, has_meta)) = &self.single {
            if let Ok(metadata) = fs::metadata(path) {
                let seconds = |time: std::io::Result<std::time::SystemTime>| {
                    let since = time.ok()?.duration_since(UNIX_EPOCH).ok()?;
                    i64::try_from(since.as_secs()).ok()
                };
                if let Some(time) = seconds(metadata.modified()) {
                    send(Update::Modified(time));
                }
                if let Some(time) = seconds(metadata.created()) {
                    send(Update::Created(time));
                }
            }
            match kind {
                Kind::Image => {
                    if !has_meta && let Some(meta) = image_meta(path) {
                        send(Update::Meta(meta));
                    }
                    let fields = crate::viewer::camera::summary(path);
                    if !fields.is_empty() {
                        send(Update::Camera(fields));
                    }
                }
                Kind::Video => {
                    if !has_meta && let Some(meta) = crate::video::probe(path, format) {
                        send(Update::Meta(meta));
                    }
                    if let Some(seconds) = crate::video::duration(path) {
                        send(Update::Duration(seconds));
                    }
                }
                Kind::Folder | Kind::Other => {}
            }
        }
        if !self.folders.is_empty() {
            let count = count(&self.folders, cancel, |count| {
                send(Update::Contents(*count, false))
            });
            if !cancel.load(Ordering::Relaxed) {
                send(Update::Contents(count, true));
            }
        }
    }
}

/// An image's size and date from its first bytes (the whole file for the
/// formats that keep them further in).
fn image_meta(path: &Path) -> Option<Meta> {
    let mut data = Vec::new();
    let mut file = fs::File::open(path).ok()?;
    (&mut file)
        .take(decode::PREFIX_LEN as u64)
        .read_to_end(&mut data)
        .ok()?;
    if let Some(meta) = decode::metadata(&data) {
        return Some(meta);
    }
    file.read_to_end(&mut data).ok()?;
    decode::metadata(&data)
}

/// Counts everything inside `roots`, reporting progress now and then. Other
/// drives mounted inside aren't entered, and symlinks aren't followed.
fn count(roots: &[PathBuf], cancel: &AtomicBool, mut report: impl FnMut(&Count)) -> Count {
    let mut count = Count::default();
    let mut reported = Instant::now();
    for root in roots {
        let Ok(device) = fs::metadata(root).map(|m| m.dev()) else {
            count.unreadable = true;
            continue;
        };
        let mut stack = vec![root.clone()];
        while let Some(folder) = stack.pop() {
            let Ok(entries) = fs::read_dir(&folder) else {
                count.unreadable = true;
                continue;
            };
            for entry in entries.flatten() {
                if cancel.load(Ordering::Relaxed) {
                    return count;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    count.folders += 1;
                    match entry.metadata() {
                        Ok(metadata) if metadata.dev() == device => stack.push(entry.path()),
                        Ok(_) => {}
                        Err(_) => count.unreadable = true,
                    }
                } else {
                    count.files += 1;
                    if let Ok(metadata) = entry.metadata() {
                        count.bytes += metadata.len();
                    }
                }
                if reported.elapsed() >= REPORT_EVERY {
                    report(&count);
                    reported = Instant::now();
                }
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_values() {
        assert_eq!(size_text(512), "512 bytes");
        assert_eq!(duration_text(83.4), "1:23");
        assert_eq!(duration_text(3725.0), "1:02:05");
        assert_eq!(duration_text(0.2), "0:00");
        let meta = Meta {
            width: 5536,
            height: 3692,
            taken: None,
            format: "CR3",
        };
        assert_eq!(dimensions_text(&meta, Kind::Image), "5536 × 3692 (20.4 MP)");
        assert_eq!(dimensions_text(&meta, Kind::Video), "5536 × 3692");
        let count = Count {
            files: 1234,
            folders: 1,
            bytes: 0,
            unreadable: false,
        };
        assert_eq!(contents_text(&count, true), "1,234 files, 1 folder");
        assert_eq!(contents_text(&count, false), "1,234 files, 1 folder…");
        assert_eq!(contents_text(&Count::default(), true), "Empty");
    }

    #[test]
    fn counts_a_folder() {
        let root = std::env::temp_dir().join(format!("fastviz-count-{}", std::process::id()));
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("one"), [0u8; 10]).unwrap();
        fs::write(root.join("a/two"), [0u8; 20]).unwrap();
        fs::write(root.join("a/b/three"), [0u8; 30]).unwrap();
        let count = count(std::slice::from_ref(&root), &AtomicBool::new(false), |_| {});
        fs::remove_dir_all(&root).unwrap();
        assert_eq!((count.files, count.folders, count.bytes), (3, 2, 60));
        assert!(!count.unreadable);
    }
}
