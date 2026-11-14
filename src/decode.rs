//! Thumbnail generation: content sniffing, EXIF, cheap decoding, orientation.
//!
//! JPEGs use the embedded EXIF thumbnail when it is big enough, otherwise a
//! DCT-scaled decode (1/8 to 1/1). HEIF/AVIF use libheif and prefer the
//! thumbnail stored in the file. Other formats go through the `image` crate.

use std::io::Cursor;

use exif::{Context, In, Tag};
use gtk::glib;
use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

use crate::media::Meta;
use crate::turbojpeg;

/// Bump when decoding improves, so files that failed before get retried.
pub const DECODER_VERSION: u32 = 2;

/// Bytes read first from each file: enough for a JPEG's EXIF block (at most
/// 64 KB) including its embedded thumbnail, and for every format's header.
pub const PREFIX_LEN: usize = 128 * 1024;

/// JPEG quality for cached thumbnails.
const CACHE_QUALITY: i32 = 85;

pub enum Pixels {
    Rgb(RgbImage),
    Rgba(RgbaImage),
}

impl Pixels {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Rgb(image) => image.dimensions(),
            Self::Rgba(image) => image.dimensions(),
        }
    }

    pub fn has_alpha(&self) -> bool {
        matches!(self, Self::Rgba(_))
    }

    pub fn into_raw(self) -> Vec<u8> {
        match self {
            Self::Rgb(image) => image.into_raw(),
            Self::Rgba(image) => image.into_raw(),
        }
    }

    /// Keeps an alpha channel only if some pixel actually uses it.
    fn from_image(image: DynamicImage) -> Self {
        if image.color().has_alpha() {
            let rgba = image.to_rgba8();
            if rgba.pixels().any(|p| p[3] != u8::MAX) {
                return Self::Rgba(rgba);
            }
            return Self::Rgb(DynamicImage::ImageRgba8(rgba).to_rgb8());
        }
        match image {
            DynamicImage::ImageRgb8(rgb) => Self::Rgb(rgb),
            other => Self::Rgb(other.to_rgb8()),
        }
    }
}

pub struct Thumbnail {
    pub pixels: Pixels,
    pub meta: Meta,
    /// The pixels are the whole image at 100%, so it must not be upscaled.
    pub native: bool,
}

/// Per-thread decoder state.
pub struct Codec {
    jpeg: Option<turbojpeg::Decoder>,
    jpeg_encoder: Option<turbojpeg::Encoder>,
    heif: LibHeif,
}

impl Codec {
    pub fn new() -> Self {
        Self {
            jpeg: turbojpeg::Decoder::new(),
            jpeg_encoder: turbojpeg::Encoder::new(),
            heif: LibHeif::new(),
        }
    }

    fn jpeg(&mut self) -> Result<&mut turbojpeg::Decoder, String> {
        self.jpeg
            .as_mut()
            .ok_or_else(|| "TurboJPEG unavailable".to_owned())
    }
}

/// Detects the image format from the first bytes, ignoring the file name.
pub fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("JPEG");
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("PNG");
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some("GIF");
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("WebP");
    }
    // Canon CR2 is a TIFF with "CR" and a version after the first IFD offset.
    if data.len() >= 12 && data.starts_with(b"II*\0") && &data[8..10] == b"CR" {
        return Some("CR2");
    }
    if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        return Some("TIFF");
    }
    if data.starts_with(&[0xFF, 0x0A]) || data.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") {
        return Some("JPEG XL");
    }
    // RAW formats that aren't TIFF-based; LibRaw tells which camera.
    if data.starts_with(b"FUJIFILMCCD-RAW")
        || data.starts_with(b"FOVb")
        || data.starts_with(b"\0MRM")
        || data.starts_with(b"IIRO")
        || data.starts_with(b"IIRS")
        || data.starts_with(b"IIU\0")
        || (data.len() >= 14 && data.starts_with(b"II\x1a\0\0\0HEAPCCDR"))
    {
        return Some("RAW");
    }
    if data.len() >= 16 && &data[4..8] == b"ftyp" {
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let end = size.clamp(16, data.len());
        // The major brand, then the compatible brands after the minor version.
        let (compatible, _) = data[16..end].as_chunks::<4>();
        let brands = std::iter::once(&data[8..12]).chain(compatible.iter().map(|b| &b[..]));
        let mut heif = false;
        for brand in brands {
            match brand {
                b"avif" | b"avis" => return Some("AVIF"),
                b"crx " => return Some("CR3"),
                b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1" => {
                    heif = true
                }
                _ => {}
            }
        }
        return heif.then_some("HEIF");
    }
    if data.len() >= 14 && data.starts_with(b"BM") {
        return Some("BMP");
    }
    None
}

/// Tries the EXIF thumbnail embedded in a JPEG, using only the file's first
/// bytes. `None` if there is none, it is smaller than `tier`, or its shape
/// differs from the photo (some cameras letterbox it).
pub fn embedded_jpeg_thumbnail(prefix: &[u8], tier: u32, codec: &mut Codec) -> Option<Thumbnail> {
    if sniff(prefix) != Some("JPEG") {
        return None;
    }
    let exif = read_exif(prefix)?;
    let embedded = embedded_thumbnail(&exif)?;
    let decoder = codec.jpeg().ok()?;
    let photo = decoder.header(prefix).ok()?;
    let small = decoder.header(embedded).ok()?;
    if small.width.max(small.height) < tier {
        return None;
    }
    let photo_aspect = f64::from(photo.width) / f64::from(photo.height);
    let small_aspect = f64::from(small.width) / f64::from(small.height);
    if (photo_aspect / small_aspect - 1.0).abs() > 0.03 {
        return None;
    }
    let image = DynamicImage::ImageRgb8(decoder.decode_scaled(embedded, tier).ok()?);
    let orientation = orientation(&exif);
    let (width, height) = oriented(photo.width, photo.height, orientation);
    Some(Thumbnail {
        pixels: Pixels::from_image(orient(fit(image, tier), orientation)),
        meta: Meta {
            width,
            height,
            taken: taken(&exif),
            format: "JPEG",
        },
        native: false,
    })
}

/// Builds a thumbnail whose longer edge is at most `tier`, plus metadata.
pub fn thumbnail(data: &[u8], tier: u32, codec: &mut Codec) -> Result<Thumbnail, String> {
    if let Some(raw) = raw_info(data) {
        match raw_preview(data, codec) {
            Some(preview) => {
                let info = Info {
                    orientation: raw.orientation,
                    taken: raw.meta.taken,
                };
                let mut thumb = jpeg_thumbnail(preview, &info, tier, codec, raw.meta.format)?;
                // Report the camera's pixel size, not the preview's.
                thumb.meta = raw.meta;
                return Ok(thumb);
            }
            None if raw.certain => return Err("no embedded preview in this RAW file".into()),
            // A TIFF from a camera maker without a preview is an ordinary TIFF.
            None => {}
        }
    }
    match sniff(data) {
        Some("JPEG") => jpeg_thumbnail(data, &info_of(data), tier, codec, "JPEG"),
        Some(format @ ("HEIF" | "AVIF")) => heif_thumbnail(data, tier, format, codec),
        Some("JPEG XL") => Err("JPEG XL is not supported yet".to_owned()),
        Some(format) => generic_thumbnail(data, tier, format),
        None => Err("unrecognized file format".to_owned()),
    }
}

/// Metadata only, from the file's first bytes (falls back to the whole file for HEIF).
pub fn metadata(data: &[u8]) -> Option<Meta> {
    let format = sniff(data)?;
    let exif = read_exif(data);
    if let Some(raw) = raw_info(data) {
        return Some(raw.meta);
    }
    if is_libraw_format(format) {
        // A RAW file whose details need the whole file (not just its start).
        return None;
    }
    let size = imagesize::blob_size(data).ok()?;
    let orientation = match format {
        // HEIF rotation lives in the container and imagesize reports raw dimensions.
        "HEIF" | "AVIF" => 1,
        _ => exif.as_ref().map_or(1, orientation),
    };
    let (width, height) = oriented(size.width as u32, size.height as u32, orientation);
    Some(Meta {
        width,
        height,
        taken: exif.as_ref().and_then(taken),
        format,
    })
}

/// Orientation and date from a file's EXIF.
struct Info {
    orientation: u32,
    taken: Option<i64>,
}

fn info_of(bytes: &[u8]) -> Info {
    let exif = read_exif(bytes);
    Info {
        orientation: exif.as_ref().map_or(1, orientation),
        taken: exif.as_ref().and_then(taken),
    }
}

/// `data` is the JPEG to decode; orientation and date come from `info`: the
/// JPEG's own EXIF, or the RAW file's around a preview.
fn jpeg_thumbnail(
    data: &[u8],
    info: &Info,
    tier: u32,
    codec: &mut Codec,
    format: &'static str,
) -> Result<Thumbnail, String> {
    let orientation = info.orientation;
    let decoder = codec.jpeg()?;
    let (image, size) = match decoder.header(data) {
        Ok(header) => {
            let image = match decoder.decode_scaled(data, tier) {
                Ok(rgb) => DynamicImage::ImageRgb8(rgb),
                // CMYK and other JPEGs TurboJPEG can't turn into RGB.
                Err(_) => decode_image(data, ImageFormat::Jpeg)?,
            };
            (image, (header.width, header.height))
        }
        Err(_) => {
            let image = decode_image(data, ImageFormat::Jpeg)?;
            let size = (image.width(), image.height());
            (image, size)
        }
    };
    let native = (image.width(), image.height()) == size && size.0.max(size.1) <= tier;
    let (width, height) = oriented(size.0, size.1, orientation);
    Ok(Thumbnail {
        pixels: Pixels::from_image(orient(fit(image, tier), orientation)),
        meta: Meta {
            width,
            height,
            taken: info.taken,
            format,
        },
        native,
    })
}

fn generic_thumbnail(data: &[u8], tier: u32, format: &'static str) -> Result<Thumbnail, String> {
    let image = decode_image(data, image_format(format)?)?;
    let exif = read_exif(data);
    let orientation = exif.as_ref().map_or(1, orientation);
    let native = image.width().max(image.height()) <= tier;
    let (width, height) = oriented(image.width(), image.height(), orientation);
    Ok(Thumbnail {
        pixels: Pixels::from_image(orient(fit(image, tier), orientation)),
        meta: Meta {
            width,
            height,
            taken: exif.as_ref().and_then(taken),
            format,
        },
        native,
    })
}

fn heif_thumbnail(
    data: &[u8],
    tier: u32,
    format: &'static str,
    codec: &mut Codec,
) -> Result<Thumbnail, String> {
    let context = HeifContext::read_from_bytes(data).map_err(|e| e.to_string())?;
    let handle = context.primary_image_handle().map_err(|e| e.to_string())?;
    let (width, height) = (handle.width(), handle.height());

    // iPhone HEICs carry a ~320 px thumbnail item, far cheaper than the photo.
    let mut ids = vec![0; handle.number_of_thumbnails()];
    let count = handle.thumbnail_ids(&mut ids);
    let embedded = ids[..count]
        .iter()
        .filter_map(|&id| handle.thumbnail(id).ok())
        .filter(|t| t.width().max(t.height()) >= tier)
        .min_by_key(|t| t.width().max(t.height()));
    let source = embedded.as_ref().unwrap_or(&handle);
    let image = heif_decode(source, codec)?;

    let native = embedded.is_none() && width.max(height) <= tier;
    // libheif already applied the container's rotation and mirroring.
    Ok(Thumbnail {
        pixels: Pixels::from_image(fit(image, tier)),
        meta: Meta {
            width,
            height,
            taken: heif_exif(&handle).as_ref().and_then(taken),
            format,
        },
        native,
    })
}

/// Decodes a HEIF image (rotation and mirroring applied) to 8-bit RGB(A).
fn heif_decode(
    handle: &libheif_rs::ImageHandle,
    codec: &mut Codec,
) -> Result<DynamicImage, String> {
    let alpha = handle.has_alpha_channel();
    let chroma = if alpha {
        RgbChroma::Rgba
    } else {
        RgbChroma::Rgb
    };
    let decoded = codec
        .heif
        .decode(handle, ColorSpace::Rgb(chroma), None)
        .map_err(|e| e.to_string())?;
    let plane = decoded
        .planes()
        .interleaved
        .ok_or_else(|| "no interleaved plane".to_owned())?;
    let channels = if alpha { 4 } else { 3 };
    if plane.storage_bits_per_pixel != channels * 8 {
        return Err(format!(
            "unexpected {} bits per pixel",
            plane.storage_bits_per_pixel
        ));
    }
    let row_len = plane.width as usize * channels as usize;
    let mut pixels = Vec::with_capacity(row_len * plane.height as usize);
    for row in plane.data.chunks(plane.stride).take(plane.height as usize) {
        pixels.extend_from_slice(row.get(..row_len).ok_or("short HEIF row")?);
    }
    if alpha {
        RgbaImage::from_raw(plane.width, plane.height, pixels).map(DynamicImage::ImageRgba8)
    } else {
        RgbImage::from_raw(plane.width, plane.height, pixels).map(DynamicImage::ImageRgb8)
    }
    .ok_or_else(|| "bad HEIF buffer".to_owned())
}

/// A decoded image for the fullscreen viewer.
pub struct Display {
    pub pixels: Pixels,
    /// The whole image's size as displayed; `pixels` may be a smaller preview.
    pub width: u32,
    pub height: u32,
    /// `pixels` are the image at full resolution.
    pub full: bool,
}

/// Decodes for the fullscreen viewer. Given the screen size (device pixels)
/// it may return a cheaper preview that still covers the image's fit size on
/// screen; without it, the full resolution.
pub fn display(
    data: &[u8],
    screen: Option<(u32, u32)>,
    codec: &mut Codec,
) -> Result<Display, String> {
    if let Some(raw) = raw_info(data) {
        // A screen-sized request gets the camera's own JPEG, which is
        // instant; the viewer then asks for the full development.
        let info = Info {
            orientation: raw.orientation,
            taken: raw.meta.taken,
        };
        if screen.is_some()
            && let Some(preview) = raw_preview(data, codec)
            && let Ok(mut shown) = jpeg_display(preview, &info, screen, codec)
        {
            // Lay it out at the RAW's size so the swap doesn't jump.
            let aspect = |w: u32, h: u32| f64::from(w) / f64::from(h.max(1));
            if (aspect(raw.meta.width, raw.meta.height) / aspect(shown.width, shown.height) - 1.0)
                .abs()
                < 0.03
            {
                (shown.width, shown.height) = (raw.meta.width, raw.meta.height);
            }
            shown.full = false;
            return Ok(shown);
        }
        match crate::libraw::develop(data) {
            Ok(image) => return Ok(finish_display(DynamicImage::ImageRgb8(image), screen)),
            Err(error) if raw.certain => return Err(error),
            // A TIFF from a camera maker that isn't RAW after all.
            Err(_) => {}
        }
    }
    match sniff(data) {
        Some("JPEG") => jpeg_display(data, &info_of(data), screen, codec),
        Some("HEIF" | "AVIF") => {
            let context = HeifContext::read_from_bytes(data).map_err(|e| e.to_string())?;
            let handle = context.primary_image_handle().map_err(|e| e.to_string())?;
            Ok(finish_display(heif_decode(&handle, codec)?, screen))
        }
        Some("JPEG XL") => Err("JPEG XL is not supported yet".to_owned()),
        Some(format) => {
            let image = decode_image(data, image_format(format)?)?;
            let orientation = read_exif(data).as_ref().map_or(1, orientation);
            Ok(finish_display(orient(image, orientation), screen))
        }
        None => Err("unrecognized file format".to_owned()),
    }
}

fn jpeg_display(
    data: &[u8],
    info: &Info,
    screen: Option<(u32, u32)>,
    codec: &mut Codec,
) -> Result<Display, String> {
    let orientation = info.orientation;
    let decoder = codec.jpeg()?;
    let Ok(header) = decoder.header(data) else {
        let image = orient(decode_image(data, ImageFormat::Jpeg)?, orientation);
        return Ok(finish_display(image, None));
    };
    let (width, height) = oriented(header.width, header.height, orientation);
    // DCT scaling makes a screen-sized decode of a 64 MP photo cheap.
    let min_edge = screen.map_or(u32::MAX, |screen| preview_edge(width, height, screen));
    let image = match decoder.decode_scaled(data, min_edge) {
        Ok(rgb) => DynamicImage::ImageRgb8(rgb),
        Err(_) => decode_image(data, ImageFormat::Jpeg)?,
    };
    let full = (image.width(), image.height()) == (header.width, header.height);
    Ok(Display {
        pixels: Pixels::from_image(orient(image, orientation)),
        width,
        height,
        full,
    })
}

/// Formats LibRaw reads that aren't TIFF-with-EXIF: told apart by content.
fn is_libraw_format(sniffed: &str) -> bool {
    matches!(sniffed, "CR3" | "RAW")
}

/// Camera makers whose TIFF files may be RAW (DNG, Pentax PEF, Phase One IIQ, ...).
const RAW_MAKERS: [&str; 14] = [
    "PENTAX",
    "RICOH",
    "LEICA",
    "HASSELBLAD",
    "PHASE ONE",
    "SIGMA",
    "OLYMPUS",
    "OM DIGITAL",
    "PANASONIC",
    "SAMSUNG",
    "KODAK",
    "EPSON",
    "MAMIYA",
    "LEAF",
];

/// A camera RAW file's details.
struct RawInfo {
    /// Size as displayed (camera pixels, orientation applied), date and format.
    meta: Meta,
    orientation: u32,
    /// Surely a RAW file; a TIFF from a camera maker might not be.
    certain: bool,
}

/// Recognizes Canon CR2/CR3, Sony ARW, Nikon NEF and the other RAW formats
/// LibRaw reads, and returns their size, date and orientation. CR2, ARW and
/// NEF are TIFFs, told apart from real TIFFs by the maker in their EXIF and
/// read from it directly; the others ask LibRaw, which needs the whole file.
fn raw_info(data: &[u8]) -> Option<RawInfo> {
    let sniffed = sniff(data)?;
    if matches!(sniffed, "CR2" | "TIFF") {
        let exif = read_exif(data)?;
        let make = match exif.get_field(Tag::Make, In::PRIMARY).map(|f| &f.value) {
            Some(exif::Value::Ascii(values)) => values
                .first()
                .map(|v| String::from_utf8_lossy(v).to_uppercase())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let label = if sniffed == "CR2" {
            Some("CR2")
        } else if make.starts_with("SONY") {
            Some("ARW")
        } else if make.contains("NIKON") {
            Some("NEF")
        } else {
            None
        };
        if let Some(label) = label {
            let dimension = |tag| exif.get_field(tag, In::PRIMARY)?.value.get_uint(0);
            let orientation = orientation(&exif);
            if let (Some(x), Some(y)) = (
                dimension(Tag::PixelXDimension),
                dimension(Tag::PixelYDimension),
            ) {
                let (width, height) = oriented(x, y, orientation);
                return Some(RawInfo {
                    meta: Meta {
                        width,
                        height,
                        taken: taken(&exif),
                        format: label,
                    },
                    orientation,
                    certain: sniffed == "CR2",
                });
            }
            // No pixel size in the EXIF: LibRaw knows it (whole file only).
            let probe = crate::libraw::probe(data)?;
            let (width, height) = oriented(probe.width, probe.height, orientation);
            return Some(RawInfo {
                meta: Meta {
                    width,
                    height,
                    taken: taken(&exif).or(probe.taken),
                    format: label,
                },
                orientation,
                certain: sniffed == "CR2",
            });
        }
        let dng = exif
            .fields()
            .any(|f| f.tag == Tag(Context::Tiff, 0xC612) && f.ifd_num == In::PRIMARY);
        if !dng && !RAW_MAKERS.iter().any(|m| make.contains(m)) {
            return None;
        }
    } else if !is_libraw_format(sniffed) {
        return None;
    }
    let probe = crate::libraw::probe(data)?;
    let orientation = match probe.flip {
        3 => 3,
        5 => 8,
        6 => 6,
        _ => 1,
    };
    let (width, height) = oriented(probe.width, probe.height, orientation);
    let format = match sniffed {
        "CR3" => "CR3",
        _ if probe.dng => "DNG",
        _ => "RAW",
    };
    Some(RawInfo {
        meta: Meta {
            width,
            height,
            taken: probe.taken,
            format,
        },
        orientation,
        certain: true,
    })
}

/// The largest full-color JPEG inside a RAW file (Canon CR2 keeps a
/// camera-made preview, often full size, next to the sensor data). JPEG
/// streams are found by their start marker, which can't occur inside another
/// JPEG's data, and only streams that open with an APPn, DQT or comment segment count (the
/// sensor data is lossless JPEG and opens with a Huffman table), and
/// TurboJPEG's header check has the final say.
fn raw_preview<'a>(data: &'a [u8], codec: &mut Codec) -> Option<&'a [u8]> {
    let decoder = codec.jpeg().ok()?;
    let mut best: Option<(u64, &[u8])> = None;
    let mut tried = 0;
    let mut from = 0;
    while let Some(found) = data[from..].windows(4).position(|w| {
        w[0] == 0xFF && w[1] == 0xD8 && w[2] == 0xFF && matches!(w[3], 0xE0..=0xEF | 0xDB | 0xFE)
    }) {
        let start = from + found;
        from = start + 3;
        tried += 1;
        if tried > 24 {
            break;
        }
        let candidate = &data[start..];
        if let Ok(header) = decoder.header(candidate) {
            let area = u64::from(header.width) * u64::from(header.height);
            if !header.cmyk && best.is_none_or(|(a, _)| area > a) {
                best = Some((area, candidate));
            }
        }
    }
    best.map(|(_, jpeg)| jpeg)
}

/// Downscales to the fit size on `screen` (if given and smaller).
fn finish_display(image: DynamicImage, screen: Option<(u32, u32)>) -> Display {
    let (width, height) = (image.width(), image.height());
    let edge = screen.map_or(u32::MAX, |screen| preview_edge(width, height, screen));
    let full = edge >= width.max(height);
    let image = if full { image } else { fit(image, edge) };
    Display {
        pixels: Pixels::from_image(image),
        width,
        height,
        full,
    }
}

/// The longer edge the image has when shrunk to fit `screen` (never enlarged).
fn preview_edge(width: u32, height: u32, screen: (u32, u32)) -> u32 {
    let fit = (f64::from(screen.0) / f64::from(width))
        .min(f64::from(screen.1) / f64::from(height))
        .min(1.0);
    (f64::from(width.max(height)) * fit).ceil() as u32
}

/// Frames of an animated GIF, composited, with their delays in milliseconds.
/// `None` for stills, and for animations over 512 MB decoded.
pub fn animation(data: &[u8]) -> Option<Vec<(RgbaImage, u32)>> {
    use image::AnimationDecoder;
    if sniff(data) != Some("GIF") {
        return None;
    }
    let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(data)).ok()?;
    let mut frames = Vec::new();
    let mut bytes = 0usize;
    for frame in decoder.into_frames() {
        let frame = frame.ok()?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let delay = numerator.checked_div(denominator).unwrap_or(0);
        // Browsers treat near-zero delays as 100 ms; so do we.
        let delay = if delay < 20 { 100 } else { delay };
        let buffer = frame.into_buffer();
        bytes += buffer.len();
        if bytes > 512 << 20 {
            return None;
        }
        frames.push((buffer, delay));
    }
    (frames.len() > 1).then_some(frames)
}

fn image_format(format: &str) -> Result<ImageFormat, String> {
    match format {
        "PNG" => Ok(ImageFormat::Png),
        "GIF" => Ok(ImageFormat::Gif),
        "WebP" => Ok(ImageFormat::WebP),
        "BMP" => Ok(ImageFormat::Bmp),
        "TIFF" => Ok(ImageFormat::Tiff),
        other => Err(format!("{other} is not supported")),
    }
}

fn heif_exif(handle: &libheif_rs::ImageHandle) -> Option<exif::Exif> {
    let mut ids = [0; 1];
    if handle.metadata_block_ids(&mut ids, b"Exif") == 0 {
        return None;
    }
    let block = handle.metadata(ids[0]).ok()?;
    // The block starts with a 4-byte offset to the TIFF header.
    let offset = u32::from_be_bytes(block.get(..4)?.try_into().ok()?) as usize;
    let tiff = block.get(4 + offset..)?;
    exif::Reader::new().read_raw(tiff.to_vec()).ok()
}

fn decode_image(data: &[u8], format: ImageFormat) -> Result<DynamicImage, String> {
    image::ImageReader::with_format(Cursor::new(data), format)
        .decode()
        .map_err(|e| e.to_string())
}

/// Scales down to fit `tier` x `tier`; never scales up.
fn fit(image: DynamicImage, tier: u32) -> DynamicImage {
    let (width, height) = (image.width(), image.height());
    let longest = width.max(height);
    if longest <= tier {
        return image;
    }
    let scale = f64::from(tier) / f64::from(longest);
    let fit_width = ((f64::from(width) * scale).round() as u32).max(1);
    let fit_height = ((f64::from(height) * scale).round() as u32).max(1);
    image.thumbnail_exact(fit_width, fit_height)
}

fn read_exif(data: &[u8]) -> Option<exif::Exif> {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(data))
        .ok()
}

fn orientation(exif: &exif::Exif) -> u32 {
    exif.get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1)
}

/// Applies EXIF orientation 1-8 so the image displays upright.
pub fn orient(image: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

/// Size after orientation: 5-8 swap width and height.
fn oriented(width: u32, height: u32, orientation: u32) -> (u32, u32) {
    if orientation >= 5 {
        (height, width)
    } else {
        (width, height)
    }
}

fn embedded_thumbnail(exif: &exif::Exif) -> Option<&[u8]> {
    let offset = exif
        .get_field(Tag::JPEGInterchangeFormat, In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let len = exif
        .get_field(Tag::JPEGInterchangeFormatLength, In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    exif.buf().get(offset..offset.checked_add(len)?)
}

/// DateTimeOriginal (or DateTimeDigitized) as Unix time, honoring the
/// EXIF offset tag and assuming local time without one.
fn taken(exif: &exif::Exif) -> Option<i64> {
    let ascii = |tag| match &exif.get_field(tag, In::PRIMARY)?.value {
        exif::Value::Ascii(values) => values.first().cloned(),
        _ => None,
    };
    let (text, offset) = match ascii(Tag::DateTimeOriginal) {
        Some(text) => (text, ascii(Tag::OffsetTimeOriginal)),
        None => (
            ascii(Tag::DateTimeDigitized)?,
            ascii(Tag::OffsetTimeDigitized),
        ),
    };
    let mut time = exif::DateTime::from_ascii(&text).ok()?;
    if let Some(offset) = offset {
        let _ = time.parse_offset(&offset);
    }
    let zone = match time.offset {
        Some(minutes) => glib::TimeZone::from_offset(i32::from(minutes) * 60),
        None => glib::TimeZone::local(),
    };
    glib::DateTime::new(
        &zone,
        i32::from(time.year),
        i32::from(time.month),
        i32::from(time.day),
        i32::from(time.hour),
        i32::from(time.minute),
        f64::from(time.second),
    )
    .ok()
    .map(|t| t.to_unix())
}

/// Cached thumbnail encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Jpeg = 1,
    Png = 2,
}

impl Encoding {
    pub fn from_code(code: i64) -> Option<Self> {
        match code {
            1 => Some(Self::Jpeg),
            2 => Some(Self::Png),
            _ => None,
        }
    }
}

/// Opaque thumbnails are stored as JPEG; ones with transparency as PNG.
pub fn encode(pixels: &Pixels, codec: &mut Codec) -> Result<(Encoding, Vec<u8>), String> {
    match pixels {
        Pixels::Rgb(rgb) => {
            let encoder = codec
                .jpeg_encoder
                .as_mut()
                .ok_or_else(|| "TurboJPEG unavailable".to_owned())?;
            Ok((Encoding::Jpeg, encoder.encode(rgb, CACHE_QUALITY)?))
        }
        Pixels::Rgba(rgba) => {
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new_with_quality(
                &mut png,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Adaptive,
            )
            .write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| e.to_string())?;
            Ok((Encoding::Png, png))
        }
    }
}

pub fn decode_cached(encoding: Encoding, data: &[u8], codec: &mut Codec) -> Result<Pixels, String> {
    match encoding {
        Encoding::Jpeg => Ok(Pixels::Rgb(codec.jpeg()?.decode_scaled(data, u32::MAX)?)),
        Encoding::Png => Ok(Pixels::Rgba(
            decode_image(data, ImageFormat::Png)?.to_rgba8(),
        )),
    }
}

use image::ImageEncoder as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(image: &DynamicImage) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn sniffs_by_content() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE1]), Some("JPEG"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("PNG"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("WebP"));
        assert_eq!(sniff(b"\0\0\0\x1cftypheic\0\0\0\0mif1heic"), Some("HEIF"));
        assert_eq!(sniff(b"\0\0\0\x1cftypavif\0\0\0\0mif1miaf"), Some("AVIF"));
        // An MP4 has an ftyp box too, but isn't an image.
        assert_eq!(sniff(b"\0\0\0\x18ftypmp42\0\0\0\0isommp42"), None);
        assert_eq!(sniff(b"plain text"), None);
    }

    #[test]
    fn thumbnails_fit_without_upscaling() {
        let mut codec = Codec::new();
        let big = DynamicImage::ImageRgb8(RgbImage::new(1000, 500));
        let thumb = thumbnail(&png_bytes(&big), 256, &mut codec).unwrap();
        assert_eq!(thumb.pixels.dimensions(), (256, 128));
        assert_eq!((thumb.meta.width, thumb.meta.height), (1000, 500));
        assert!(!thumb.native && !thumb.pixels.has_alpha());

        let small =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(40, 30, image::Rgba([1, 2, 3, 0])));
        let thumb = thumbnail(&png_bytes(&small), 256, &mut codec).unwrap();
        assert_eq!(thumb.pixels.dimensions(), (40, 30));
        assert!(thumb.native && thumb.pixels.has_alpha());
    }

    #[test]
    fn corrupt_data_is_an_error_not_a_panic() {
        let mut codec = Codec::new();
        assert!(thumbnail(b"\x89PNG\r\n\x1a\ngarbage", 128, &mut codec).is_err());
        assert!(thumbnail(&[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3], 128, &mut codec).is_err());
        assert!(thumbnail(b"", 128, &mut codec).is_err());
    }

    #[test]
    fn orientation_swaps_dimensions() {
        assert_eq!(oriented(4000, 3000, 6), (3000, 4000));
        assert_eq!(oriented(4000, 3000, 3), (4000, 3000));
        let image = DynamicImage::ImageRgb8(RgbImage::new(4, 2));
        assert_eq!(orient(image, 6).width(), 2);
    }

    #[test]
    fn display_previews_cover_the_screen() {
        let mut codec = Codec::new();
        let big = DynamicImage::ImageRgb8(RgbImage::new(4000, 2000));
        let shown = display(&png_bytes(&big), Some((1920, 1080)), &mut codec).unwrap();
        assert_eq!((shown.width, shown.height), (4000, 2000));
        assert_eq!(shown.pixels.dimensions(), (1920, 960));
        assert!(!shown.full);
        let full = display(&png_bytes(&big), None, &mut codec).unwrap();
        assert!(full.full && full.pixels.dimensions() == (4000, 2000));
        let small = DynamicImage::ImageRgb8(RgbImage::new(300, 200));
        assert!(
            display(&png_bytes(&small), Some((1920, 1080)), &mut codec)
                .unwrap()
                .full
        );
    }

    #[test]
    fn cache_encoding_round_trip() {
        let mut codec = Codec::new();
        let rgb = Pixels::Rgb(RgbImage::from_pixel(64, 48, image::Rgb([200, 100, 50])));
        let (encoding, data) = encode(&rgb, &mut codec).unwrap();
        assert_eq!(encoding, Encoding::Jpeg);
        assert_eq!(
            decode_cached(encoding, &data, &mut codec)
                .unwrap()
                .dimensions(),
            (64, 48)
        );
    }
}

/// Measurement behind the cache format choice; run with
/// `cargo test --release cache_format -- --ignored --nocapture` once the cache has data.
#[cfg(test)]
mod cache_format {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn cache_format() {
        let conn = rusqlite::Connection::open(crate::cache::path()).unwrap();
        let mut stmt = conn
            .prepare("SELECT data FROM thumb WHERE tier = 256 AND encoding = 1 LIMIT 300")
            .unwrap();
        let blobs: Vec<Vec<u8>> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(!blobs.is_empty(), "no cached thumbnails to measure");
        let mut codec = Codec::new();
        let images: Vec<RgbImage> = blobs
            .iter()
            .map(
                |b| match decode_cached(Encoding::Jpeg, b, &mut codec).unwrap() {
                    Pixels::Rgb(rgb) => rgb,
                    Pixels::Rgba(_) => unreachable!(),
                },
            )
            .collect();
        let n = blobs.len() as f64;
        let kib = |bytes: usize| bytes as f64 / n / 1024.0;

        let started = Instant::now();
        for blob in &blobs {
            decode_cached(Encoding::Jpeg, blob, &mut codec).unwrap();
        }
        let jpeg_ms = started.elapsed().as_secs_f64() * 1e3 / n;
        let jpeg_size: usize = blobs.iter().map(Vec::len).sum();

        let pngs: Vec<Vec<u8>> = images
            .iter()
            .map(|rgb| {
                let mut png = Vec::new();
                image::codecs::png::PngEncoder::new_with_quality(
                    &mut png,
                    image::codecs::png::CompressionType::Fast,
                    image::codecs::png::FilterType::Adaptive,
                )
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .unwrap();
                png
            })
            .collect();
        let started = Instant::now();
        for png in &pngs {
            decode_image(png, ImageFormat::Png).unwrap();
        }
        let png_ms = started.elapsed().as_secs_f64() * 1e3 / n;
        let png_size: usize = pngs.iter().map(Vec::len).sum();

        let raw_size: usize = images.iter().map(|i| i.as_raw().len()).sum();
        let started = Instant::now();
        for image in &images {
            std::hint::black_box(image.as_raw().to_vec());
        }
        let raw_ms = started.elapsed().as_secs_f64() * 1e3 / n;

        println!("{} thumbnails at the 256 tier:", blobs.len());
        println!(
            "  JPEG q{CACHE_QUALITY}: {:6.1} KiB, load {:.3} ms",
            kib(jpeg_size),
            jpeg_ms
        );
        println!(
            "  PNG fast:  {:6.1} KiB, load {:.3} ms",
            kib(png_size),
            png_ms
        );
        println!(
            "  raw RGB:   {:6.1} KiB, load {:.3} ms (+ reading it from disk)",
            kib(raw_size),
            raw_ms
        );
    }
}

#[cfg(test)]
mod raw_tests {
    use super::*;

    /// `FASTVIZ_RAW=/path/to/file.CR2 cargo test --release raw_file -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn raw_file_decodes() {
        let path = std::env::var("FASTVIZ_RAW").expect("set FASTVIZ_RAW");
        let data = std::fs::read(path).unwrap();
        println!(
            "sniff {:?} label {:?}",
            sniff(&data),
            raw_info(&data).map(|r| r.meta.format)
        );
        let mut codec = Codec::new();
        let started = std::time::Instant::now();
        let thumb = thumbnail(&data, 256, &mut codec).unwrap();
        println!("thumbnail {:?} in {:?}", thumb.meta, started.elapsed());
        let shown = display(&data, Some((2560, 1440)), &mut codec).unwrap();
        println!(
            "display {}x{} full={}",
            shown.width, shown.height, shown.full
        );
        let started = std::time::Instant::now();
        let full = display(&data, None, &mut codec).unwrap();
        println!(
            "developed {}x{} full={} in {:?}",
            full.width,
            full.height,
            full.full,
            started.elapsed()
        );
        if let (Ok(out), Pixels::Rgb(rgb)) = (std::env::var("FASTVIZ_OUT"), &full.pixels) {
            rgb.save(out).unwrap();
        }
        println!(
            "metadata from prefix: {:?}",
            metadata(&data[..PREFIX_LEN.min(data.len())])
        );
    }
}

#[cfg(test)]
mod vendor_raw_tests {
    use super::*;
    use exif::experimental::Writer;
    use exif::{Field, Value};

    /// A TIFF with `make` in its EXIF and a JPEG after it, like a RAW file
    /// reduced to the parts we read.
    fn fake_raw(make: &str, little_endian: bool) -> Vec<u8> {
        let make = Field {
            tag: Tag::Make,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![make.as_bytes().to_vec()]),
        };
        let size = |tag, value| Field {
            tag,
            ifd_num: In::PRIMARY,
            value: Value::Long(vec![value]),
        };
        let (x, y) = (
            size(Tag::PixelXDimension, 96),
            size(Tag::PixelYDimension, 64),
        );
        let mut writer = Writer::new();
        writer.push_field(&make);
        writer.push_field(&x);
        writer.push_field(&y);
        let mut tiff = std::io::Cursor::new(Vec::new());
        writer.write(&mut tiff, little_endian).unwrap();
        let mut data = tiff.into_inner();
        let image = DynamicImage::ImageRgb8(RgbImage::from_fn(96, 64, |x, y| {
            image::Rgb([x as u8 * 2, y as u8 * 3, 90])
        }));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image.write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
        data.extend_from_slice(&jpeg.into_inner());
        data
    }

    #[test]
    fn sony_and_nikon_use_their_embedded_jpeg() {
        let mut codec = Codec::new();
        for (make, little_endian, label) in
            [("SONY", true, "ARW"), ("NIKON CORPORATION", false, "NEF")]
        {
            let data = fake_raw(make, little_endian);
            assert_eq!(raw_info(&data).map(|r| r.meta.format), Some(label));
            let thumb = thumbnail(&data, 256, &mut codec).unwrap();
            assert_eq!(thumb.meta.format, label);
            // A screen-sized request is answered with the embedded JPEG; full
            // resolution goes to LibRaw, which rejects this stand-in file.
            let shown = display(&data, Some((1920, 1080)), &mut codec).unwrap();
            assert_eq!((shown.width, shown.height, shown.full), (96, 64, false));
            assert!(display(&data, None, &mut codec).is_err());
        }
    }

    #[test]
    fn other_makers_stay_plain_tiffs() {
        let data = fake_raw("Canon", true);
        assert!(raw_info(&data).is_none());
    }
}
