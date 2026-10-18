//! Streaming folder scan on a worker thread.
//!
//! One `getdents` pass reads names and types (d_type), files are filtered by
//! extension before anything else, and only media files get one `statx` each,
//! relative to the open directory. Results stream to the UI in batches.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::media::{self, Kind};

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    /// Lowercased name, the natural-sort key.
    pub key: String,
    pub size: u64,
    /// Modification time, seconds since the epoch.
    pub mtime: i64,
    pub kind: Kind,
    pub format: &'static str,
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
    dir: &std::path::Path,
    show_hidden: bool,
    cancel: &AtomicBool,
    tx: &async_channel::Sender<ScanMsg>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| e.to_string())?;
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
        let Some((kind, format)) = media::classify(&name) else {
            continue;
        };
        let Ok(file_type) = dirent.file_type() else {
            continue;
        };
        let metadata = if file_type.is_file() {
            dirent.metadata()
        } else if file_type.is_symlink() {
            fs::metadata(dirent.path())
        } else {
            continue;
        };
        let Ok(metadata) = metadata else { continue };
        if !metadata.is_file() {
            continue;
        }

        let name = name.into_owned();
        batch.push(Entry {
            path: dirent.path(),
            key: name.to_lowercase(),
            name,
            size: metadata.len(),
            mtime: metadata.mtime(),
            kind,
            format,
        });

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

/// Dot-files, plus the Windows system folders every NTFS drive carries.
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("$RECYCLE.BIN")
        || name == "System Volume Information"
}
