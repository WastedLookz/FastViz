//! JPEG: rotation changes the EXIF orientation tag (two bytes written in
//! place, no recompression), cropping copies the compressed blocks
//! (libjpeg-turbo's lossless transform).

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::{Rect, orientation_after, tiff};
use crate::turbojpeg::Transformer;

const EXIF: &[u8] = b"Exif\0\0";
const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const MPF: &[u8] = b"MPF\0";
const APP0: u8 = 0xE0;
const APP1: u8 = 0xE1;
const APP2: u8 = 0xE2;
/// EXIF sits at the start; this much of the file covers it.
const PREFIX: usize = 256 << 10;

/// A marker segment before the image data.
struct Segment {
    marker: u8,
    /// From its 0xFF to its end.
    whole: Range<usize>,
    /// Its contents, after the length.
    body: Range<usize>,
}

/// The segments between SOI and the image data (or the end of `data`).
fn segments(data: &[u8]) -> Vec<Segment> {
    let mut out = Vec::new();
    if !data.starts_with(&[0xFF, 0xD8]) {
        return out;
    }
    let mut at = 2;
    while data.get(at) == Some(&0xFF) {
        let mut marker_at = at + 1;
        while data.get(marker_at) == Some(&0xFF) {
            marker_at += 1;
        }
        let Some(&marker) = data.get(marker_at) else {
            break;
        };
        // Start of scan or end of image: no more metadata.
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let Some(length) = data.get(marker_at + 1..marker_at + 3) else {
            break;
        };
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        let end = marker_at + 1 + length;
        if length < 2 || end > data.len() {
            break;
        }
        out.push(Segment {
            marker,
            whole: at..end,
            body: marker_at + 3..end,
        });
        at = end;
    }
    out
}

/// The EXIF block's TIFF data within `data`.
fn exif_range(data: &[u8], segments: &[Segment]) -> Option<Range<usize>> {
    segments
        .iter()
        .find(|s| s.marker == APP1 && data[s.body.clone()].starts_with(EXIF))
        .map(|s| s.body.start + EXIF.len()..s.body.end)
}

/// Where XMP's copy of the orientation (one digit) is, if there is one.
fn xmp_orientation(data: &[u8], segments: &[Segment]) -> Vec<usize> {
    let mut found = Vec::new();
    for segment in segments {
        let body = &data[segment.body.clone()];
        if segment.marker != APP1 || !body.starts_with(XMP) {
            continue;
        }
        for pattern in [&b"tiff:Orientation=\""[..], b"<tiff:Orientation>"] {
            let mut from = 0;
            while let Some(position) = find(&body[from..], pattern) {
                let digit = from + position + pattern.len();
                if body.get(digit).is_some_and(|d| (b'1'..=b'8').contains(d))
                    && !body.get(digit + 1).is_some_and(u8::is_ascii_digit)
                {
                    found.push(segment.body.start + digit);
                }
                from = digit;
            }
        }
    }
    found
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Turns the picture `turns` quarter turns clockwise. Usually two bytes are
/// written in place; a file without an orientation tag is rewritten with one.
/// Returns the new contents when the file has to be replaced.
pub fn rotate(path: &Path, turns: u8) -> Result<Option<Vec<u8>>, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut prefix = Vec::with_capacity(PREFIX);
    (&file)
        .take(PREFIX as u64)
        .read_to_end(&mut prefix)
        .map_err(|e| e.to_string())?;
    let segments = segments(&prefix);
    if let Some(range) = exif_range(&prefix, &segments)
        && let Some((tiff, at, value)) = tiff::orientation(&prefix[range.clone()])
    {
        let value = orientation_after(value, turns);
        write_at(&file, &tiff.u16_bytes(value), range.start + at)?;
        for digit in xmp_orientation(&prefix, &segments) {
            write_at(&file, &[b'0' + value as u8], digit)?;
        }
        return Ok(None);
    }
    drop(file);
    let mut data = std::fs::read(path).map_err(|e| e.to_string())?;
    let segments = self::segments(&data);
    let value = orientation_after(1, turns);
    for digit in xmp_orientation(&data, &segments) {
        data[digit] = b'0' + value as u8;
    }
    let (replace, tiff) = match exif_range(&data, &segments) {
        Some(range) => {
            let tiff = tiff::with_orientation(&data[range.clone()], value)
                .ok_or("its EXIF data can't be read")?;
            let segment = segments
                .iter()
                .find(|s| s.body.start + EXIF.len() == range.start)
                .ok_or("its EXIF data can't be read")?;
            (segment.whole.clone(), tiff)
        }
        None => {
            // A new EXIF block after SOI (and after a JFIF header, if any).
            let at = segments
                .first()
                .filter(|s| s.marker == APP0)
                .map_or(2, |s| s.whole.end);
            (at..at, tiff::minimal(value))
        }
    };
    let length = u16::try_from(2 + EXIF.len() + tiff.len()).map_err(|_| "its EXIF data is full")?;
    let mut out = Vec::with_capacity(data.len() + tiff.len() + 10);
    out.extend_from_slice(&data[..replace.start]);
    out.extend_from_slice(&[0xFF, APP1]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(EXIF);
    out.extend_from_slice(&tiff);
    out.extend_from_slice(&data[replace.end..]);
    Ok(Some(out))
}

fn write_at(file: &File, bytes: &[u8], at: usize) -> Result<(), String> {
    file.write_all_at(bytes, at as u64)
        .map_err(|e| e.to_string())
}

/// The picture's orientation tag (1 if there is none).
fn orientation(data: &[u8]) -> u16 {
    exif_range(data, &segments(data))
        .and_then(|range| tiff::orientation(&data[range]))
        .map_or(1, |(_, _, value)| value)
}

/// Crops to `rect` of the picture as displayed, without recompressing. The
/// stored image's left and top edges must fall on its 8 or 16 pixel blocks,
/// so the area moves left or up by a few pixels where needed (its size stays).
pub fn crop(data: &[u8], rect: Rect) -> Result<Vec<u8>, String> {
    let mut transformer = Transformer::new().ok_or("libjpeg-turbo failed to start")?;
    let blocks = transformer.blocks(data)?;
    let orientation = orientation(data);
    let stored = super::to_stored(rect, orientation, blocks.width, blocks.height)
        .ok_or("the area is outside the picture")?;
    let (mcu_x, mcu_y) = blocks.mcu;
    let region = (
        stored.x - stored.x % mcu_x,
        stored.y - stored.y % mcu_y,
        stored.width,
        stored.height,
    );
    let cropped = transformer.crop(data, region)?;
    Ok(tidy(cropped, stored.width, stored.height))
}

/// Metadata copied from the uncropped file that would now be wrong: the
/// EXIF thumbnail (still the whole picture) and pixel size, and the MPF
/// index of extra images that were after the original's end.
fn tidy(mut data: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
    let segments = segments(&data);
    if let Some(range) = exif_range(&data, &segments) {
        let tiff = &mut data[range];
        let _ = tiff::drop_thumbnail(tiff);
        let _ = tiff::set_pixel_size(tiff, width, height);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut copied = 0;
    for segment in &segments {
        if segment.marker == APP2 && data[segment.body.clone()].starts_with(MPF) {
            out.extend_from_slice(&data[copied..segment.whole.start]);
            copied = segment.whole.end;
        }
    }
    out.extend_from_slice(&data[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turbojpeg::{Decoder, Encoder};
    use image::RgbImage;

    fn jpeg() -> Vec<u8> {
        let image = RgbImage::from_fn(320, 240, |x, y| {
            image::Rgb([
                (x % 256) as u8,
                (y % 256) as u8,
                if x < 160 { 0 } else { 255 },
            ])
        });
        Encoder::new().unwrap().encode(&image, 90).unwrap()
    }

    fn temp(name: &str, data: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fastviz-jpeg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn rotation_adds_then_changes_the_tag() {
        let path = temp("rotate.jpg", &jpeg());
        // No EXIF yet: the file is rewritten with one.
        let new = rotate(&path, 1).unwrap().expect("a rewrite");
        assert_eq!(orientation(&new), 6);
        std::fs::write(&path, &new).unwrap();
        // Then two bytes change in place.
        assert!(rotate(&path, 1).unwrap().is_none());
        let data = std::fs::read(&path).unwrap();
        assert_eq!(orientation(&data), 3);
        assert_eq!(data.len(), new.len());
        // The picture data is untouched and still decodes.
        let header = Decoder::new().unwrap().header(&data).unwrap();
        assert_eq!((header.width, header.height), (320, 240));
        assert!(rotate(&path, 2).unwrap().is_none());
        assert_eq!(orientation(&std::fs::read(&path).unwrap()), 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn xmp_orientation_follows() {
        let mut data = jpeg();
        let xmp = [XMP, b"<x tiff:Orientation=\"1\"/>"].concat();
        let mut segment = vec![0xFF, APP1];
        segment.extend_from_slice(&((xmp.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(&xmp);
        data.splice(2..2, segment);
        let path = temp("xmp.jpg", &data);
        let new = rotate(&path, 3).unwrap().unwrap();
        assert_eq!(orientation(&new), 8);
        assert!(find(&new, b"tiff:Orientation=\"8\"").is_some());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn crops_in_display_orientation() {
        let path = temp("crop.jpg", &jpeg());
        let rotated = rotate(&path, 1).unwrap().unwrap();
        std::fs::remove_file(&path).unwrap();
        // Shown 240 x 320 (turned right): the top 100 rows shown are the
        // stored picture's left 100 columns (the dark-blue half).
        let cropped = crop(
            &rotated,
            Rect {
                x: 0,
                y: 0,
                width: 240,
                height: 100,
            },
        )
        .unwrap();
        assert_eq!(orientation(&cropped), 6);
        let mut decoder = Decoder::new().unwrap();
        let stored = decoder.decode_scaled(&cropped, 10_000).unwrap();
        assert_eq!(stored.dimensions(), (100, 240));
        assert!(stored.get_pixel(50, 120)[2] < 40);
    }
}
