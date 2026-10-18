//! Recognizing media files by extension.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Video,
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
}
