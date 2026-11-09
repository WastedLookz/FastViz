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
        // CR3 and some other RAW formats keep their EXIF where the reader
        // can't find it; LibRaw knows the main fields.
        return crate::libraw::details(path).map_or_else(Vec::new, raw_rows);
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

/// The fields LibRaw reads, in the same order as the EXIF list.
fn raw_rows(d: crate::libraw::Details) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let mut add = |name: &str, value: String| {
        if !value.is_empty() {
            rows.push((name.to_owned(), value));
        }
    };
    add("Make", d.make.clone());
    add("Model", d.model.clone());
    add("Lens model", d.lens);
    let taken = d
        .taken
        .and_then(|t| gtk::glib::DateTime::from_unix_local(t).ok())
        .and_then(|t| t.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|t| t.to_string());
    add("Date/time original", taken.unwrap_or_default());
    if d.shutter > 0.0 {
        let exposure = if d.shutter < 1.0 {
            format!("1/{} s", (1.0 / d.shutter).round())
        } else {
            format!("{} s", trim(d.shutter, 1))
        };
        add("Exposure time", exposure);
    }
    if d.aperture > 0.0 {
        add("FNumber", format!("f/{}", trim(d.aperture, 1)));
    }
    if d.iso > 0.0 {
        add("ISO", format!("{}", d.iso.round()));
    }
    if d.focal > 0.0 {
        add("Focal length", format!("{} mm", trim(d.focal, 1)));
    }
    if d.focal35 > 0 {
        add("Focal length in 35mm film", format!("{} mm", d.focal35));
    }
    add("Flash", if d.flash { "Fired" } else { "" }.to_owned());
    if d.raw_size.0 > 0 {
        add(
            "Sensor pixels",
            format!("{} × {}", d.raw_size.0, d.raw_size.1),
        );
    }
    add("Software", d.software);
    add("Artist", d.artist);
    add("Body serial number", d.serial);
    if let Some((latitude, longitude, altitude)) = d.gps {
        add("GPS position", format!("{latitude:.5}, {longitude:.5}"));
        if altitude != 0.0 {
            add("GPS altitude", format!("{altitude:.0} m"));
        }
    }
    rows
}

/// `value` with at most `places` decimals and no trailing zeros ("2.8", "4").
fn trim(value: f32, places: usize) -> String {
    let text = format!("{value:.places$}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    text.to_owned()
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

    #[test]
    fn trims_numbers() {
        assert_eq!(super::trim(2.8, 1), "2.8");
        assert_eq!(super::trim(4.0, 1), "4");
        assert_eq!(super::trim(50.0, 1), "50");
    }

    /// `FASTVIZ_RAW=/path/to/file.CR3 cargo test --release raw_details -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn raw_details() {
        let path = std::env::var("FASTVIZ_RAW").expect("set FASTVIZ_RAW");
        for (name, value) in super::read(std::path::Path::new(&path)) {
            println!("{name}: {value}");
        }
    }
}
