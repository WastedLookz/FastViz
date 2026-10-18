//! File operations on a worker thread: copy, move, trash, delete, rename and
//! restoring from the trash. Results stream back as `Report`s.
//!
//! Nothing is ever overwritten: a name that is taken in the destination gets
//! " (2)", " (3)", ... before its extension.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::prelude::*;

use crate::scan::{self, Entry};

pub enum Op {
    Copy(Vec<PathBuf>, PathBuf),
    Move(Vec<PathBuf>, PathBuf),
    Trash(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
    Rename(PathBuf, String),
    /// Put items trashed from these paths back.
    Restore(Vec<PathBuf>),
}

impl Op {
    /// "Copying 3 of 12…" in the status bar.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Copy(..) => "Copying",
            Self::Move(..) => "Moving",
            Self::Trash(_) => "Moving to Trash",
            Self::Delete(_) => "Deleting",
            Self::Rename(..) => "Renaming",
            Self::Restore(_) => "Restoring",
        }
    }
}

pub enum Report {
    Progress {
        done: usize,
        total: usize,
    },
    /// A file or folder now exists (pasted, renamed, restored).
    Added(Entry),
    /// No longer in its folder (moved away, trashed, deleted, renamed).
    Removed(PathBuf),
    /// Moved or renamed: cached thumbnails can follow it.
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
    /// These are on a volume without a trash; deleting them needs a confirmation.
    NoTrash(Vec<PathBuf>),
    Done {
        succeeded: usize,
    },
}

pub fn start(op: Op) -> async_channel::Receiver<Report> {
    let (tx, rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("fileops".into())
        .spawn(move || {
            let succeeded = run(op, &tx);
            let _ = tx.send_blocking(Report::Done { succeeded });
        })
        .expect("failed to spawn file operation thread");
    rx
}

fn run(op: Op, tx: &async_channel::Sender<Report>) -> usize {
    let send = |report| {
        let _ = tx.send_blocking(report);
    };
    let added = |path: &Path| {
        if let Some(entry) = scan::entry_for(path) {
            send(Report::Added(entry));
        }
    };
    let mut succeeded = 0;
    match op {
        Op::Copy(sources, dir) => {
            for (done, source) in sources.iter().enumerate() {
                send(Report::Progress {
                    done,
                    total: sources.len(),
                });
                match copy_into(source, &dir) {
                    Ok(target) => {
                        succeeded += 1;
                        added(&target);
                    }
                    Err(e) => send(failed(source, e)),
                }
            }
        }
        Op::Move(sources, dir) => {
            for (done, source) in sources.iter().enumerate() {
                send(Report::Progress {
                    done,
                    total: sources.len(),
                });
                if source.parent() == Some(dir.as_path()) {
                    succeeded += 1;
                    continue;
                }
                match move_into(source, &dir) {
                    Ok(target) => {
                        succeeded += 1;
                        send(Report::Moved {
                            from: source.clone(),
                            to: target.clone(),
                        });
                        send(Report::Removed(source.clone()));
                        added(&target);
                    }
                    Err(e) => send(failed(source, e)),
                }
            }
        }
        Op::Trash(paths) => {
            let mut no_trash = Vec::new();
            for (done, path) in paths.iter().enumerate() {
                send(Report::Progress {
                    done,
                    total: paths.len(),
                });
                match gio::File::for_path(path).trash(None::<&gio::Cancellable>) {
                    Ok(()) => {
                        succeeded += 1;
                        send(Report::Removed(path.clone()));
                    }
                    Err(e) if e.matches(gio::IOErrorEnum::NotSupported) => {
                        no_trash.push(path.clone())
                    }
                    Err(e) => send(Report::Failed {
                        path: path.clone(),
                        error: e.message().to_owned(),
                    }),
                }
            }
            if !no_trash.is_empty() {
                send(Report::NoTrash(no_trash));
            }
        }
        Op::Delete(paths) => {
            for (done, path) in paths.iter().enumerate() {
                send(Report::Progress {
                    done,
                    total: paths.len(),
                });
                match remove(path) {
                    Ok(()) => {
                        succeeded += 1;
                        send(Report::Removed(path.clone()));
                    }
                    Err(e) => send(failed(path, e)),
                }
            }
        }
        Op::Rename(from, name) => {
            let to = from.with_file_name(&name);
            let result = if fs::symlink_metadata(&to).is_ok() {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("“{name}” already exists"),
                ))
            } else {
                fs::rename(&from, &to)
            };
            match result {
                Ok(()) => {
                    succeeded += 1;
                    send(Report::Moved {
                        from: from.clone(),
                        to: to.clone(),
                    });
                    send(Report::Removed(from));
                    added(&to);
                }
                Err(e) => send(failed(&from, e)),
            }
        }
        Op::Restore(paths) => {
            for path in restore(&paths) {
                succeeded += 1;
                added(&path);
            }
        }
    }
    succeeded
}

fn failed(path: &Path, error: io::Error) -> Report {
    Report::Failed {
        path: path.to_owned(),
        error: error.to_string(),
    }
}

/// `dir/name`, or `dir/stem (2).ext` and so on if that is taken.
fn unique_path(dir: &Path, name: &OsStr) -> PathBuf {
    let candidate = dir.join(name);
    if fs::symlink_metadata(&candidate).is_err() {
        return candidate;
    }
    let name = name.to_string_lossy();
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name.as_ref(), ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|path| fs::symlink_metadata(path).is_err())
        .expect("some numbered name is free")
}

fn copy_into(source: &Path, dir: &Path) -> io::Result<PathBuf> {
    if dir.starts_with(source) {
        return Err(io::Error::other("can't copy a folder into itself"));
    }
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?;
    let target = unique_path(dir, name);
    copy_tree(source, &target)?;
    Ok(target)
}

/// Copies a file (keeping its dates and permissions) or a folder recursively.
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        Ok(())
    } else if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)
    } else {
        gio::File::for_path(source)
            .copy(
                &gio::File::for_path(target),
                gio::FileCopyFlags::ALL_METADATA | gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
                None::<&gio::Cancellable>,
                None,
            )
            .map_err(|e| io::Error::other(e.message().to_owned()))
    }
}

fn move_into(source: &Path, dir: &Path) -> io::Result<PathBuf> {
    if dir.starts_with(source) {
        return Err(io::Error::other("can't move a folder into itself"));
    }
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?;
    let target = unique_path(dir, name);
    match fs::rename(source, &target) {
        Ok(()) => Ok(target),
        // Another drive: copy, and remove the original only once that worked.
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_tree(source, &target)?;
            remove(source)?;
            Ok(target)
        }
        Err(e) => Err(e),
    }
}

fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Moves the most recently trashed item for each path back; returns those restored.
fn restore(paths: &[PathBuf]) -> Vec<PathBuf> {
    let trash = gio::File::for_uri("trash:///");
    let Ok(children) = trash.enumerate_children(
        "standard::name,trash::orig-path,trash::deletion-date",
        gio::FileQueryInfoFlags::NONE,
        None::<&gio::Cancellable>,
    ) else {
        return Vec::new();
    };
    let mut items: Vec<gio::FileInfo> = children.flatten().collect();
    items.sort_by_key(|info| {
        std::cmp::Reverse(
            info.attribute_string("trash::deletion-date")
                .unwrap_or_default(),
        )
    });
    let mut wanted: HashSet<&PathBuf> = paths.iter().collect();
    let mut restored = Vec::new();
    for info in items {
        let Some(original) = info.attribute_byte_string("trash::orig-path") else {
            continue;
        };
        let original = PathBuf::from(original.as_str());
        if !wanted.remove(&original) {
            continue;
        }
        let moved = trash.child(info.name()).move_(
            &gio::File::for_path(&original),
            gio::FileCopyFlags::NONE,
            None::<&gio::Cancellable>,
            None,
        );
        if moved.is_ok() {
            restored.push(original);
        }
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("fastviz-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, OsStr::new("a.jpg")), dir.join("a.jpg"));
        fs::write(dir.join("a.jpg"), b"x").unwrap();
        assert_eq!(
            unique_path(&dir, OsStr::new("a.jpg")),
            dir.join("a (2).jpg")
        );
        fs::write(dir.join("a (2).jpg"), b"x").unwrap();
        assert_eq!(
            unique_path(&dir, OsStr::new("a.jpg")),
            dir.join("a (3).jpg")
        );
        fs::write(dir.join(".hidden"), b"x").unwrap();
        assert_eq!(
            unique_path(&dir, OsStr::new(".hidden")),
            dir.join(".hidden (2)")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_and_move_folders() {
        let root = std::env::temp_dir().join(format!("fastviz-ops-{}", std::process::id()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(src.join("inner")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("inner/photo.jpg"), b"jpeg").unwrap();

        let copied = copy_into(&src, &dst).unwrap();
        assert_eq!(fs::read(copied.join("inner/photo.jpg")).unwrap(), b"jpeg");
        let again = copy_into(&src, &dst).unwrap();
        assert_eq!(again, dst.join("src (2)"));
        assert!(copy_into(&src, &src.join("inner")).is_err());

        let moved = move_into(&src, &dst.join("src (2)")).unwrap();
        assert!(!src.exists() && moved.join("inner/photo.jpg").exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
