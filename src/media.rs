//! Recognizing media files by extension.

use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Folder,
    Image,
    Video,
    /// Any other file: listed with its type's icon, not previewed.
    Other,
}

impl Kind {
    /// Every kind, in order (`kind as usize` indexes per-kind tables).
    pub const ALL: [Kind; 4] = [Self::Folder, Self::Image, Self::Video, Self::Other];

    pub fn is_media(self) -> bool {
        matches!(self, Self::Image | Self::Video)
    }
}

/// Facts about an image that need its contents, filled in lazily.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Pixel size as displayed (after EXIF/HEIF rotation).
    pub width: u32,
    pub height: u32,
    /// EXIF DateTimeOriginal as Unix time.
    pub taken: Option<i64>,
    /// Format detected from the content, which can differ from the extension.
    pub format: &'static str,
}

/// Every format label we produce, for mapping stored text back to `&'static str`.
const FORMATS: &[&str] = &[
    "JPEG", "PNG", "WebP", "GIF", "BMP", "TIFF", "HEIF", "AVIF", "JPEG XL", "MP4", "M4V", "MOV",
    "3GP", "3G2", "MKV", "WebM", "AVI", "WMV", "FLV", "MPEG", "MPEG-TS", "Ogg", "MXF", "DV",
];

pub fn format_label(name: &str) -> &'static str {
    FORMATS
        .iter()
        .copied()
        .find(|f| *f == name)
        .unwrap_or("Unknown")
}

/// The label for a non-media file: its extension in capitals ("PDF"), or "File".
/// Labels are interned; there are only ever a few dozen distinct extensions.
pub fn other_label(name: &str) -> &'static str {
    static LABELS: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let ext = match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            ext.to_ascii_uppercase()
        }
        _ => return "File",
    };
    let mut labels = LABELS.lock().unwrap_or_else(|e| e.into_inner());
    let labels = labels.get_or_insert_with(HashSet::new);
    if let Some(&label) = labels.get(ext.as_str()) {
        return label;
    }
    let label: &'static str = Box::leak(ext.into_boxed_str());
    labels.insert(label);
    label
}

/// Classifies a file name by its extension, returning its kind and a short format label.
/// Content sniffing for files with missing or wrong extensions comes with the metadata pass.
pub fn classify(name: &str) -> Option<(Kind, &'static str)> {
    let (_, ext) = name.rsplit_once('.')?;
    if ext.len() > 4 {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    use Kind::{Image, Video};
    Some(match ext.as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => (Image, "JPEG"),
        "png" => (Image, "PNG"),
        "webp" => (Image, "WebP"),
        "gif" => (Image, "GIF"),
        "bmp" => (Image, "BMP"),
        "tif" | "tiff" => (Image, "TIFF"),
        "heic" | "heif" | "hif" => (Image, "HEIF"),
        "avif" => (Image, "AVIF"),
        "jxl" => (Image, "JPEG XL"),
        "mp4" => (Video, "MP4"),
        "m4v" => (Video, "M4V"),
        "mov" | "qt" => (Video, "MOV"),
        "3gp" | "3gpp" => (Video, "3GP"),
        "3g2" => (Video, "3G2"),
        "mkv" => (Video, "MKV"),
        "webm" => (Video, "WebM"),
        "avi" | "divx" => (Video, "AVI"),
        "wmv" | "asf" => (Video, "WMV"),
        "flv" | "f4v" => (Video, "FLV"),
        "mpg" | "mpeg" | "mpe" | "m1v" | "m2v" | "vob" => (Video, "MPEG"),
        "ts" | "mts" | "m2ts" | "m2t" => (Video, "MPEG-TS"),
        "ogv" => (Video, "Ogg"),
        "mxf" => (Video, "MXF"),
        "dv" => (Video, "DV"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{Kind, classify};

    #[test]
    fn classifies_common_names() {
        assert_eq!(classify("IMG_0001.JPG"), Some((Kind::Image, "JPEG")));
        assert_eq!(classify("clip.MOV"), Some((Kind::Video, "MOV")));
        assert_eq!(classify("a.b.m2ts"), Some((Kind::Video, "MPEG-TS")));
        assert_eq!(classify("notes.txt"), None);
        assert_eq!(classify("noextension"), None);
        assert_eq!(classify("weird.jpegxl"), None);
    }

    #[test]
    fn other_labels() {
        use super::other_label;
        assert_eq!(other_label("report.pdf"), "PDF");
        assert_eq!(other_label("archive.tar.gz"), "GZ");
        assert_eq!(other_label("README"), "File");
        assert_eq!(other_label(".bashrc"), "File");
        assert!(std::ptr::eq(other_label("a.zip"), other_label("b.ZIP")));
    }
}
