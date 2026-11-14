//! PNG: decoded, turned or cropped, and encoded again (lossless). Color
//! information, text and EXIF chunks are carried over; chunks tied to the
//! old pixel format (palette transparency, background, significant bits)
//! are dropped, since the encoder writes 8 or 16-bit gray or RGB(A).

use std::collections::HashSet;
use std::io::Cursor;

use image::DynamicImage;
use image::codecs::png::PngEncoder;

use super::{Rect, tiff};

const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Ancillary chunks that stay valid when the pixels change.
const KEPT: [&[u8; 4]; 11] = [
    b"iCCP", b"sRGB", b"gAMA", b"cHRM", b"cICP", b"pHYs", b"tEXt", b"zTXt", b"iTXt", b"tIME",
    b"eXIf",
];
/// Chunks that describe the old pixel format or an animation.
const DROPPED: [&[u8; 4]; 8] = [
    b"tRNS", b"bKGD", b"sBIT", b"hIST", b"sPLT", b"acTL", b"fcTL", b"fdAT",
];

struct Chunk<'a> {
    kind: [u8; 4],
    body: &'a [u8],
    /// The whole chunk as stored (length, type, body, CRC).
    raw: &'a [u8],
}

fn chunks(data: &[u8]) -> Result<Vec<Chunk<'_>>, String> {
    if !data.starts_with(SIGNATURE) {
        return Err("not a PNG file".to_owned());
    }
    let mut out = Vec::new();
    let mut at = SIGNATURE.len();
    loop {
        let header = data.get(at..at + 8).ok_or("the file is truncated")?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap_or_default()) as usize;
        let kind: [u8; 4] = header[4..8].try_into().unwrap_or_default();
        let end = at + 12 + length;
        let raw = data.get(at..end).ok_or("the file is truncated")?;
        out.push(Chunk {
            kind,
            body: &raw[8..8 + length],
            raw,
        });
        if &kind == b"IEND" {
            return Ok(out);
        }
        at = end;
    }
}

fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32(&out[4..]).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub fn rotate(data: &[u8], turns: u8) -> Result<Vec<u8>, String> {
    edit(data, turns % 2 == 1, |image| match turns % 4 {
        1 => image.rotate90(),
        2 => image.rotate180(),
        3 => image.rotate270(),
        _ => image,
    })
}

pub fn crop(data: &[u8], rect: Rect) -> Result<Vec<u8>, String> {
    edit(data, false, |image| {
        image.crop_imm(rect.x, rect.y, rect.width, rect.height)
    })
}

/// Decodes (applying an EXIF orientation), changes the picture, and encodes
/// it with the original's chunks that still apply. `turned`: the width and
/// height swap (so does the pixel aspect ratio).
fn edit(
    data: &[u8],
    turned: bool,
    change: impl FnOnce(DynamicImage) -> DynamicImage,
) -> Result<Vec<u8>, String> {
    let original = chunks(data)?;
    if original.iter().any(|c| &c.kind == b"acTL") {
        return Err("animated PNGs can't be edited".to_owned());
    }
    let orientation = original
        .iter()
        .find(|c| &c.kind == b"eXIf")
        .and_then(|c| tiff::orientation(c.body))
        .map_or(1, |(_, _, value)| u32::from(value));
    let image = image::ImageReader::with_format(Cursor::new(data), image::ImageFormat::Png)
        .decode()
        .map_err(|e| e.to_string())?;
    let image = change(crate::decode::orient(image, orientation));
    let mut encoded = Vec::new();
    image
        .write_with_encoder(PngEncoder::new(&mut encoded))
        .map_err(|e| e.to_string())?;
    let fresh = chunks(&encoded)?;

    let written: HashSet<[u8; 4]> = fresh.iter().map(|c| c.kind).collect();
    let carried = |c: &Chunk| {
        let ancillary = c.kind[0].is_ascii_lowercase();
        // Unknown chunks may be copied only if marked safe to copy.
        let safe = KEPT.contains(&&c.kind) || c.kind[3].is_ascii_lowercase();
        ancillary && safe && !DROPPED.contains(&&c.kind) && !written.contains(&c.kind)
    };
    let patched = |c: &Chunk| -> Vec<u8> {
        match &c.kind {
            b"pHYs" if turned && c.body.len() == 9 => {
                let mut body = c.body.to_vec();
                body[..8].rotate_left(4);
                chunk(&c.kind, &body)
            }
            // The pixels now carry the orientation.
            b"eXIf" if orientation != 1 => {
                let mut body = c.body.to_vec();
                tiff::set_orientation(&mut body, 1);
                chunk(&c.kind, &body)
            }
            _ => c.raw.to_vec(),
        }
    };
    let first_data = original
        .iter()
        .position(|c| &c.kind == b"IDAT")
        .unwrap_or(original.len());
    let mut out = SIGNATURE.to_vec();
    for c in fresh.iter().filter(|c| &c.kind == b"IHDR") {
        out.extend_from_slice(c.raw);
    }
    for c in original[..first_data].iter().filter(|c| carried(c)) {
        out.extend(patched(c));
    }
    for c in fresh
        .iter()
        .filter(|c| &c.kind != b"IHDR" && &c.kind != b"IEND")
    {
        out.extend_from_slice(c.raw);
    }
    for c in original[first_data..].iter().filter(|c| carried(c)) {
        out.extend(patched(c));
    }
    out.extend(chunk(b"IEND", &[]));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, Rgba, RgbaImage};

    fn png() -> Vec<u8> {
        let image = RgbaImage::from_fn(30, 20, |x, y| Rgba([x as u8 * 8, y as u8 * 12, 7, 200]));
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_with_encoder(PngEncoder::new(&mut out))
            .unwrap();
        // Add a text chunk and a pixel aspect before the data.
        let parts = chunks(&out).unwrap();
        let mut with = SIGNATURE.to_vec();
        with.extend_from_slice(parts[0].raw);
        with.extend(chunk(b"tEXt", b"Comment\0hello"));
        with.extend(chunk(b"pHYs", &[0, 0, 0, 1, 0, 0, 0, 2, 0]));
        for part in &parts[1..] {
            with.extend_from_slice(part.raw);
        }
        with
    }

    fn decode(data: &[u8]) -> DynamicImage {
        image::load_from_memory_with_format(data, image::ImageFormat::Png).unwrap()
    }

    #[test]
    fn rotates_and_keeps_chunks() {
        let data = png();
        let turned = rotate(&data, 1).unwrap();
        let image = decode(&turned);
        assert_eq!(image.dimensions(), (20, 30));
        // The original's bottom-left corner is now the top-left one.
        assert_eq!(image.get_pixel(0, 0), decode(&data).get_pixel(0, 19));
        let parts = chunks(&turned).unwrap();
        let text = parts.iter().find(|c| &c.kind == b"tEXt").unwrap();
        assert_eq!(text.body, b"Comment\0hello");
        let aspect = parts.iter().find(|c| &c.kind == b"pHYs").unwrap();
        assert_eq!(aspect.body, &[0, 0, 0, 2, 0, 0, 0, 1, 0]);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn crops() {
        let data = png();
        let cropped = crop(
            &data,
            Rect {
                x: 5,
                y: 3,
                width: 10,
                height: 4,
            },
        )
        .unwrap();
        let image = decode(&cropped);
        assert_eq!(image.dimensions(), (10, 4));
        assert_eq!(image.get_pixel(0, 0), decode(&data).get_pixel(5, 3));
    }
}
