//! The metadata panel at the right edge of the fullscreen viewer: every
//! readable EXIF field (camera, lens, exposure, white balance, GPS, ...).

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use exif::{Context, In, Tag, Value};

/// Tags that are binary blobs or bookkeeping, not worth listing.
const SKIPPED: [Tag; 5] = [
    Tag::MakerNote,
    Tag::ComponentsConfiguration,
    Tag::JPEGInterchangeFormat,
    Tag::JPEGInterchangeFormatLength,
    Tag::StripOffsets,
];

/// All EXIF fields of the main image as (name, value), in file order, with
/// the ones people look for (camera, lens, exposure) first. Empty if the file
/// has none or can't be read.
pub fn read(path: &Path) -> Vec<(String, String)> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut BufReader::new(file)) else {
        return Vec::new();
    };
    let mut rows: Vec<(Tag, String, String)> = Vec::new();
    for field in exif.fields() {
        if field.ifd_num != In::PRIMARY
            || SKIPPED.contains(&field.tag)
            || field.tag.description().is_none()
            || matches!(field.tag.context(), Context::Interop)
        {
            continue;
        }
        if matches!(&field.value, Value::Undefined(bytes, _) if bytes.len() > 64)
            || matches!(&field.value, Value::Byte(bytes) if bytes.len() > 64)
        {
            continue;
        }
        let value = field
            .display_value()
            .with_unit(&exif)
            .to_string()
            .trim_matches('"')
            .trim()
            .to_owned();
        if value.is_empty() || value.len() > 200 {
            continue;
        }
        rows.push((field.tag, words(&field.tag.to_string()), value));
    }
    let priority = |tag: Tag| {
        [
            Tag::Make,
            Tag::Model,
            Tag::LensModel,
            Tag::DateTimeOriginal,
            Tag::ExposureTime,
            Tag::FNumber,
            Tag::PhotographicSensitivity,
            Tag::FocalLength,
        ]
        .iter()
        .position(|t| *t == tag)
        .unwrap_or(usize::MAX)
    };
    rows.sort_by_key(|(tag, _, _)| priority(*tag));
    rows.into_iter().map(|(_, n, v)| (n, v)).collect()
}

/// "WhiteBalance" → "White balance".
fn words(name: &str) -> String {
    let mut out = String::new();
    let mut previous: Option<char> = None;
    for c in name.chars() {
        let boundary = previous.is_some_and(|p| p.is_lowercase() && c.is_uppercase());
        if boundary {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
        previous = Some(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn splits_camel_case() {
        assert_eq!(super::words("WhiteBalance"), "White balance");
        assert_eq!(super::words("FNumber"), "FNumber");
        assert_eq!(super::words("Make"), "Make");
    }
}
