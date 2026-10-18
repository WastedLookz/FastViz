//! Streaming folder scan on a worker thread.
//!
//! One `getdents` pass reads names and types (d_type), then each entry gets
//! one `statx`, relative to the open directory. Folders and every file are
//! listed; media is recognized by extension, and files without one are
//! sniffed. Metadata already in the cache is attached up front, so a warm
//! folder can sort by date taken or dimensions from the first batch.

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::cache;
use crate::decode;
use crate::media::{self, Kind, Meta};

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    /// Lowercased name, the natural-sort key.
    pub key: String,
    pub size: u64,
    /// Modification time, seconds since the epoch.
    pub mtime: i64,
    pub kind: Kind,
    /// From the extension (or sniffed, for files without one).
    pub format: &'static str,
    /// MIME type for non-media entries, used to pick their icon.
    pub content_type: Option<String>,
    /// Cached metadata, if this version of the file was seen before.
    pub meta: Option<Meta>,
    /// The current decoder already failed on this version of the file.
    pub failed: bool,
}

pub enum ScanMsg {
    Batch(Vec<Entry>),
    Done(Result<(), String>),
}

/// A running scan. Dropping it cancels the worker.
pub struct Scan {
    pub rx: async_channel::Receiver<ScanMsg>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The first batch is small so the grid paints immediately; later ones are larger.
const FIRST_BATCH: usize = 64;
const BATCH: usize = 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(40);

pub fn start(dir: PathBuf, show_hidden: bool) -> Scan {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let started = Instant::now();
            let result = run(&dir, show_hidden, &flag, &tx);
            crate::trace!(
                "scan worker finished in {:.1} ms",
                started.elapsed().as_secs_f64() * 1e3
            );
            if !flag.load(Ordering::Relaxed) {
                let _ = tx.send_blocking(ScanMsg::Done(result));
            }
        })
        .expect("failed to spawn scan thread");
    Scan { rx, cancel }
}

fn run(
    dir: &Path,
    show_hidden: bool,
    cancel: &AtomicBool,
    tx: &async_channel::Sender<ScanMsg>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| e.to_string())?;
    let known = cache::open()
        .and_then(|conn| cache::folder(&conn, dir))
        .unwrap_or_default();
    let mut batch = Vec::with_capacity(FIRST_BATCH);
    let mut limit = FIRST_BATCH;
    let mut last_flush = Instant::now();

    for dirent in entries {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Ok(dirent) = dirent else { continue };
        let file_name = dirent.file_name();
        let name = file_name.to_string_lossy();
        if !show_hidden && is_hidden(&name) {
            continue;
        }
        let Ok(file_type) = dirent.file_type() else {
            continue;
        };
        let metadata = if file_type.is_symlink() {
            fs::metadata(dirent.path())
        } else {
            dirent.metadata()
        };
        let Ok(metadata) = metadata else { continue };
        let path = dirent.path();
        let Some(mut entry) = make_entry(path, name.into_owned(), &metadata) else {
            continue;
        };
        if let Some((_, _, known)) = known
            .get(&entry.path)
            .filter(|(m, s, _)| (*m, *s) == (entry.mtime, entry.size))
        {
            entry.meta = known.meta;
            entry.failed = known.failed;
        }
        batch.push(entry);

        if batch.len() >= limit || last_flush.elapsed() >= FLUSH_INTERVAL {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH));
            if tx.send_blocking(ScanMsg::Batch(full)).is_err() {
                return Ok(());
            }
            limit = BATCH;
            last_flush = Instant::now();
        }
    }
    if !batch.is_empty() {
        let _ = tx.send_blocking(ScanMsg::Batch(batch));
    }
    Ok(())
}

/// The entry for one path, e.g. a file just pasted or renamed.
pub fn entry_for(path: &Path) -> Option<Entry> {
    let metadata = fs::metadata(path).ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    make_entry(path.to_owned(), name, &metadata)
}

/// Classifies a folder or file; `None` for sockets, devices and such.
fn make_entry(path: PathBuf, name: String, metadata: &fs::Metadata) -> Option<Entry> {
    let (kind, format) = if metadata.is_dir() {
        (Kind::Folder, "Folder")
    } else if !metadata.is_file() {
        return None;
    } else if let Some(media) = media::classify(&name) {
        media
    } else if let Some(sniffed) = (!name.contains('.')).then(|| sniff_file(&path)).flatten() {
        sniffed
    } else {
        (Kind::Other, media::other_label(&name))
    };
    let content_type = match kind {
        Kind::Folder => Some("inode/directory".to_owned()),
        Kind::Other => Some(
            gtk::gio::content_type_guess(Some(&*name), None)
                .0
                .to_string(),
        ),
        _ => None,
    };
    Some(Entry {
        key: name.to_lowercase(),
        size: if kind == Kind::Folder {
            0
        } else {
            metadata.len()
        },
        mtime: metadata.mtime(),
        path,
        name,
        kind,
        format,
        content_type,
        meta: None,
        failed: false,
    })
}

/// Recognizes images saved without an extension from their first bytes.
fn sniff_file(path: &Path) -> Option<(Kind, &'static str)> {
    let mut head = [0u8; 64];
    let len = fs::File::open(path).ok()?.read(&mut head).ok()?;
    decode::sniff(&head[..len]).map(|format| (Kind::Image, format))
}

/// Dot-files, plus the Windows system folders every NTFS drive carries.
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("$RECYCLE.BIN")
        || name == "System Volume Information"
}
