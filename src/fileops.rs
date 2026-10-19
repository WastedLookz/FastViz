//! File operations on a worker thread: copy, move, trash, delete, rename and
//! restoring from the trash. Results stream back as `Report`s.
//!
//! Nothing is ever overwritten: a name that is taken in the destination gets
//! " (2)", " (3)", ... before its extension. Drives are never moved, trashed
//! or deleted, nor folders with a drive mounted inside.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
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
    /// About to handle item `done` (of `total`), called `name`.
    Progress {
        done: usize,
        total: usize,
        name: String,
    },
    /// Files copied so far within the current item (a folder being copied).
    Files(usize),
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
    let progress = |done: usize, paths: &[PathBuf]| {
        send(Report::Progress {
            done,
            total: paths.len(),
            name: paths[done]
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        });
    };
    // Counts files copied, for progress in the status bar.
    let counter = || {
        let mut files = 0;
        move || {
            files += 1;
            send(Report::Files(files));
        }
    };
    let drives = mount_points();
    let mut succeeded = 0;
    match op {
        Op::Copy(sources, dir) => {
            for (done, source) in sources.iter().enumerate() {
                progress(done, &sources);
                match copy_into(source, &dir, &mut counter()) {
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
                progress(done, &sources);
                if source.parent() == Some(dir.as_path()) {
                    succeeded += 1;
                    continue;
                }
                match check_no_drive(source, &drives)
                    .and_then(|()| move_into(source, &dir, &mut counter()))
                {
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
                progress(done, &paths);
                if let Err(e) = check_no_drive(path, &drives) {
                    send(failed(path, e));
                    continue;
                }
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
                progress(done, &paths);
                match check_no_drive(path, &drives).and_then(|()| remove(path)) {
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

/// Mount points from the kernel's table.
fn mount_points() -> Vec<PathBuf> {
    let Ok(table) = fs::read("/proc/self/mounts") else {
        return Vec::new();
    };
    table
        .split(|&b| b == b'\n')
        .filter_map(|line| line.split(|&b| b == b' ').nth(1))
        .map(unescape)
        .collect()
}

/// The mount table writes spaces, tabs, newlines and backslashes as `\ooo`.
fn unescape(field: &[u8]) -> PathBuf {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        let octal = field
            .get(i + 1..i + 4)
            .filter(|_| field[i] == b'\\')
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match octal {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(field[i]);
                i += 1;
            }
        }
    }
    PathBuf::from(OsString::from_vec(out))
}

/// Refuses a drive's mount point or a folder with one inside: moving it to
/// another drive would copy the drive and then delete it, and the Trash or
/// Delete would empty it.
fn check_no_drive(path: &Path, drives: &[PathBuf]) -> io::Result<()> {
    match drives.iter().find(|mount| mount.starts_with(path)) {
        Some(mount) if mount == path => Err(io::Error::other("it's a mounted drive")),
        Some(_) => Err(io::Error::other("a drive is mounted inside it")),
        None => Ok(()),
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

fn copy_into(source: &Path, dir: &Path, copied: &mut dyn FnMut()) -> io::Result<PathBuf> {
    if dir.starts_with(source) {
        return Err(io::Error::other("can't copy a folder into itself"));
    }
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?;
    let target = unique_path(dir, name);
    copy_tree(source, &target, copied)?;
    Ok(target)
}

/// Copies a file (keeping its dates and permissions) or a folder
/// recursively, calling `copied` after each file.
fn copy_tree(source: &Path, target: &Path, copied: &mut dyn FnMut()) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()), copied)?;
        }
        return Ok(());
    }
    if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
    } else {
        gio::File::for_path(source)
            .copy(
                &gio::File::for_path(target),
                gio::FileCopyFlags::ALL_METADATA | gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
                None::<&gio::Cancellable>,
                None,
            )
            .map_err(|e| io::Error::other(e.message().to_owned()))?;
    }
    copied();
    Ok(())
}

fn move_into(source: &Path, dir: &Path, copied: &mut dyn FnMut()) -> io::Result<PathBuf> {
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
            copy_tree(source, &target, copied)?;
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

        let mut files = 0;
        let copied = copy_into(&src, &dst, &mut || files += 1).unwrap();
        assert_eq!(fs::read(copied.join("inner/photo.jpg")).unwrap(), b"jpeg");
        assert_eq!(files, 1);
        let again = copy_into(&src, &dst, &mut || {}).unwrap();
        assert_eq!(again, dst.join("src (2)"));
        assert!(copy_into(&src, &src.join("inner"), &mut || {}).is_err());

        let moved = move_into(&src, &dst.join("src (2)"), &mut || {}).unwrap();
        assert!(!src.exists() && moved.join("inner/photo.jpg").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn drives_are_left_alone() {
        assert_eq!(
            unescape(br"/media/k/My\040Drive"),
            Path::new("/media/k/My Drive")
        );
        assert_eq!(unescape(br"/a\b"), Path::new(r"/a\b"));
        let drives = mount_points();
        assert!(drives.iter().any(|m| m == Path::new("/proc")));
        assert!(check_no_drive(Path::new("/proc"), &drives).is_err());
        assert!(check_no_drive(Path::new("/"), &drives).is_err());
        let dir = std::env::temp_dir().join(format!("fastviz-drive-{}", std::process::id()));
        assert!(check_no_drive(&dir, &drives).is_ok());
    }
}
