//! Rotating and cropping files in place, without losing quality:
//!
//! - JPEG rotation changes the EXIF orientation tag (two bytes, no
//!   recompression); crops copy the compressed blocks (libjpeg-turbo).
//! - PNG is decoded and encoded again, which is lossless anyway.
//! - MP4/MOV/3GP rotation changes the video track's display matrix in the
//!   file header (36 bytes, whatever the file's size).
//!
//! The file keeps its modification date (so a Date Modified sort doesn't
//! move it), and cached thumbnails of it, ours and the desktop's, are
//! dropped. Files that are rewritten go to a hidden file next to them first,
//! which then replaces the original.

mod jpeg;
mod mp4;
mod png;
mod tiff;

use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};

use gtk::glib;

use crate::media::Kind;
use crate::scan::{self, Entry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Quarter turns clockwise (1-3).
    Rotate(u8),
    /// An area of the picture as displayed (orientation applied).
    Crop(Rect),
}

/// An area in whole pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Whether files of this format (a content or extension label) can be turned.
pub fn can_rotate(kind: Kind, format: &str) -> bool {
    match kind {
        Kind::Image => matches!(format, "JPEG" | "PNG"),
        Kind::Video => matches!(format, "MP4" | "M4V" | "MOV" | "3GP" | "3G2"),
        _ => false,
    }
}

/// Whether files of this format can be cropped.
pub fn can_crop(kind: Kind, format: &str) -> bool {
    kind == Kind::Image && matches!(format, "JPEG" | "PNG")
}

/// Applies `edit` to the file at `path` and returns its new entry.
pub fn apply(path: &Path, edit: Edit) -> Result<Entry, String> {
    let before = fs::metadata(path).map_err(|e| e.to_string())?;
    let mut times = FileTimes::new();
    if let Ok(modified) = before.modified() {
        times = times.set_modified(modified);
    }
    if let Ok(accessed) = before.accessed() {
        times = times.set_accessed(accessed);
    }
    let mut head = [0u8; 16];
    let read = {
        use std::io::Read;
        File::open(path)
            .and_then(|mut f| f.read(&mut head))
            .map_err(|e| e.to_string())?
    };
    let format = crate::decode::sniff(&head[..read]);
    let replacement = match (format, edit) {
        (Some("JPEG"), Edit::Rotate(turns)) => jpeg::rotate(path, turns)?,
        (Some("JPEG"), Edit::Crop(rect)) => Some(jpeg::crop(&read_all(path)?, rect)?),
        (Some("PNG"), Edit::Rotate(turns)) => Some(png::rotate(&read_all(path)?, turns)?),
        (Some("PNG"), Edit::Crop(rect)) => Some(png::crop(&read_all(path)?, rect)?),
        (None, Edit::Rotate(turns)) => {
            mp4::rotate(path, turns)?;
            None
        }
        _ => return Err("this kind of file can't be edited".to_owned()),
    };
    match replacement {
        Some(data) => replace(path, &data, &before, times)?,
        // Changed in place: put the date back.
        None => {
            let file = File::options()
                .write(true)
                .open(path)
                .map_err(|e| e.to_string())?;
            let _ = file.set_times(times);
        }
    }
    forget_thumbnails(path);
    scan::entry_for(path).ok_or_else(|| "the file is gone".to_owned())
}

fn read_all(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| e.to_string())
}

/// Writes `data` to a hidden file in the same folder, with the original's
/// permissions and dates, then renames it over the original.
fn replace(
    path: &Path,
    data: &[u8],
    before: &fs::Metadata,
    times: FileTimes,
) -> Result<(), String> {
    // Through a symlink, the file it points at is replaced.
    let path = fs::canonicalize(path).map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .ok_or("no file name")?
        .to_string_lossy()
        .into_owned();
    let temp = path.with_file_name(format!(".{name}.fastviz-edit"));
    let written = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(data)?;
        file.sync_all()?;
        // Best effort: FAT drives refuse permissions, some refuse dates.
        let _ = file.set_permissions(before.permissions());
        let _ = file.set_times(times);
        fs::rename(&temp, &path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e.to_string());
    }
    Ok(())
}

/// Drops cached thumbnails of the old picture: ours (the date and size it
/// was cached under may not have changed) and the desktop's, which Files
/// and other apps share.
fn forget_thumbnails(path: &Path) {
    if let Err(e) = crate::cache::forget(path) {
        eprintln!("Couldn't drop cached thumbnails of {}: {e}", path.display());
    }
    let Ok(uri) = glib::filename_to_uri(path, None) else {
        return;
    };
    let name = format!(
        "{}.png",
        glib::compute_checksum_for_string(glib::ChecksumType::Md5, &uri).unwrap_or_default()
    );
    let root = glib::user_cache_dir().join("thumbnails");
    let mut folders: Vec<PathBuf> = ["normal", "large", "x-large", "xx-large"]
        .iter()
        .map(|size| root.join(size))
        .collect();
    if let Ok(failed) = fs::read_dir(root.join("fail")) {
        folders.extend(failed.flatten().map(|entry| entry.path()));
    }
    for folder in folders {
        let _ = fs::remove_file(folder.join(&name));
    }
}

/// The EXIF orientation after turning the picture `turns` quarter turns
/// clockwise. Orientations are a mirror (2, 4, 5, 7) or not, then a turn.
fn orientation_after(orientation: u16, turns: u8) -> u16 {
    // (mirrored, clockwise quarter turns) for orientations 1-8.
    const PARTS: [(bool, u8); 8] = [
        (false, 0),
        (true, 0),
        (false, 2),
        (true, 2),
        (true, 3),
        (false, 1),
        (true, 1),
        (false, 3),
    ];
    let index = usize::from(valid(orientation) - 1);
    let (mirrored, turned) = PARTS[index];
    let wanted = (mirrored, (turned + turns) % 4);
    PARTS
        .iter()
        .position(|&parts| parts == wanted)
        .map_or(1, |i| i as u16 + 1)
}

/// An orientation tag's value, with nonsense read as 1 (as decoding does).
fn valid(orientation: u16) -> u16 {
    if (1..=8).contains(&orientation) {
        orientation
    } else {
        1
    }
}

/// `rect` of the picture as displayed, in the stored picture's coordinates
/// (`width` x `height`, before `orientation` is applied). `None` if it
/// doesn't fit.
fn to_stored(rect: Rect, orientation: u16, width: u32, height: u32) -> Option<Rect> {
    let orientation = valid(orientation);
    let (shown_w, shown_h) = if orientation >= 5 {
        (height, width)
    } else {
        (width, height)
    };
    if rect.width == 0
        || rect.height == 0
        || rect.x.checked_add(rect.width)? > shown_w
        || rect.y.checked_add(rect.height)? > shown_h
    {
        return None;
    }
    // Undo the display transform (a turn, after an optional mirror) on the
    // rectangle's corners, as continuous coordinates.
    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (rect.x + rect.width, rect.y + rect.height);
    let (mirrored, turns) = match orientation {
        2 => (true, 0),
        3 => (false, 2),
        4 => (true, 2),
        5 => (true, 3),
        6 => (false, 1),
        7 => (true, 1),
        8 => (false, 3),
        _ => (false, 0),
    };
    // Turning back: a clockwise turn maps (x, y) of a w x h picture to
    // (h - y, x); undoing it maps (x, y) of the turned one to (y, h - x).
    let (mut w, mut h) = (shown_w, shown_h);
    let mut corners = [(x0, y0), (x1, y1)];
    for _ in 0..turns {
        for corner in &mut corners {
            *corner = (corner.1, w - corner.0);
        }
        (w, h) = (h, w);
    }
    if mirrored {
        for corner in &mut corners {
            corner.0 = w - corner.0;
        }
    }
    let _ = h;
    let [(ax, ay), (bx, by)] = corners;
    Some(Rect {
        x: ax.min(bx),
        y: ay.min(by),
        width: ax.abs_diff(bx),
        height: ay.abs_diff(by),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, GenericImageView, Rgb, RgbImage};

    fn picture() -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(7, 4, |x, y| Rgb([x as u8, y as u8, 0])))
    }

    #[test]
    fn orientation_turns_compose() {
        for orientation in 1..=8u16 {
            let shown = crate::decode::orient(picture(), u32::from(orientation));
            for turns in 1..=3u8 {
                let turned = match turns {
                    1 => shown.rotate90(),
                    2 => shown.rotate180(),
                    _ => shown.rotate270(),
                };
                let after = orientation_after(orientation, turns);
                let direct = crate::decode::orient(picture(), u32::from(after));
                assert_eq!(
                    turned.to_rgb8(),
                    direct.to_rgb8(),
                    "orientation {orientation} + {turns} turns = {after}"
                );
            }
        }
    }

    #[test]
    fn crop_rectangles_map_back_to_the_stored_picture() {
        let rect = Rect {
            x: 1,
            y: 2,
            width: 2,
            height: 3,
        };
        for orientation in 1..=8u16 {
            let shown = crate::decode::orient(picture(), u32::from(orientation));
            if rect.x + rect.width > shown.width() || rect.y + rect.height > shown.height() {
                continue;
            }
            let expected = shown.crop_imm(rect.x, rect.y, rect.width, rect.height);
            let stored = to_stored(rect, orientation, 7, 4).unwrap();
            let cropped = crate::decode::orient(
                picture().crop_imm(stored.x, stored.y, stored.width, stored.height),
                u32::from(orientation),
            );
            assert_eq!(
                cropped.to_rgb8(),
                expected.to_rgb8(),
                "orientation {orientation}"
            );
        }
        assert!(
            to_stored(
                Rect {
                    x: 6,
                    y: 0,
                    width: 2,
                    height: 1
                },
                1,
                7,
                4
            )
            .is_none()
        );
    }

    #[test]
    fn edits_keep_the_date() {
        let dir = std::env::temp_dir().join(format!("fastviz-edit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("photo.png");
        picture().save(&path).unwrap();
        let old = fs::metadata(&path).unwrap().modified().unwrap();
        let older = old - std::time::Duration::from_secs(86_400);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let entry = apply(&path, Edit::Rotate(1)).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), older);
        assert_eq!(image::open(&path).unwrap().dimensions(), (4, 7));
        let seconds = older
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(entry.mtime, seconds as i64);
        apply(
            &path,
            Edit::Crop(Rect {
                x: 0,
                y: 1,
                width: 3,
                height: 5,
            }),
        )
        .unwrap();
        assert_eq!(image::open(&path).unwrap().dimensions(), (3, 5));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), older);
        // Only the picture: no temporary file left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert!(apply(&dir.join("missing.png"), Edit::Rotate(1)).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Edits a real file: `FASTVIZ_EDIT=/path/file.jpg FASTVIZ_EDIT_OP=r|l|x,y,w,h
    /// cargo test --release edit_file -- --ignored --nocapture` (a copy!).
    #[test]
    #[ignore]
    fn edit_file() {
        let path = std::env::var("FASTVIZ_EDIT").expect("set FASTVIZ_EDIT");
        let op = std::env::var("FASTVIZ_EDIT_OP").unwrap_or_else(|_| "r".to_owned());
        let edit = match op.as_str() {
            "r" => Edit::Rotate(1),
            "l" => Edit::Rotate(3),
            rect => {
                let v: Vec<u32> = rect.split(',').map(|n| n.parse().unwrap()).collect();
                Edit::Crop(Rect {
                    x: v[0],
                    y: v[1],
                    width: v[2],
                    height: v[3],
                })
            }
        };
        let started = std::time::Instant::now();
        let entry = apply(Path::new(&path), edit).unwrap();
        println!(
            "{edit:?}: {:.1} ms, now {} bytes",
            started.elapsed().as_secs_f64() * 1e3,
            entry.size
        );
    }
}
