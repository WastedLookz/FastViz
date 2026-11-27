//! Everything FFmpeg can tell about a video file, for the fullscreen
//! metadata panel: the container (format, duration, bit rate and its tags,
//! such as the camera, place and date of recording), each stream (codec,
//! size, frame rate, color, HDR, audio channels, language) and chapters.

use std::ffi::{CStr, c_char};
use std::path::Path;

use ffmpeg::codec::Id;
use ffmpeg::codec::packet::side_data::Type as SideData;
use ffmpeg::ffi;
use ffmpeg::format::stream::{Disposition, Stream};
use ffmpeg::media::Type as Medium;
use ffmpeg_next as ffmpeg;
use gtk::glib;

/// A titled group of (name, value) rows.
pub type Section = (String, Vec<(String, String)>);

/// Longer tag values are cut short.
const MAX_VALUE: usize = 300;
/// Most chapters listed.
const MAX_CHAPTERS: usize = 100;

/// The file's details by section ("File", "Video", "Audio", ...), in the
/// file's stream order. Empty if FFmpeg can't open it.
pub fn details(path: &Path) -> Vec<Section> {
    super::init();
    let Ok(input) = ffmpeg::format::input(path) else {
        return Vec::new();
    };
    let container: Vec<(String, String)> = input
        .metadata()
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();

    let mut file = Rows::default();
    file.add("Format", input.format().description());
    if input.duration() > 0 {
        let seconds = input.duration() as f64 / f64::from(ffi::AV_TIME_BASE);
        file.add("Duration", clock(seconds, true));
    }
    if input.bit_rate() > 0 {
        file.add("Overall bit rate", bit_rate(input.bit_rate()));
    }
    file.0.extend(tags(&container, &[]));
    let mut sections = vec![("File".to_owned(), file.0)];

    // "Audio 1", "Audio 2" when there are several of a kind.
    let kinds: Vec<&str> = input.streams().map(|s| kind(&s)).collect();
    let mut numbered: Vec<&str> = Vec::new();
    for (stream, &kind) in input.streams().zip(&kinds) {
        let total = kinds.iter().filter(|&&k| k == kind).count();
        numbered.push(kind);
        let title = if total > 1 {
            let n = numbered.iter().filter(|&&k| k == kind).count();
            format!("{kind} {n}")
        } else {
            kind.to_owned()
        };
        sections.push((title, stream_rows(&stream, &container, total == 1)));
    }
    sections.extend(chapters(&input));
    sections.retain(|(_, rows)| !rows.is_empty());
    sections
}

#[derive(Default)]
struct Rows(Vec<(String, String)>);

impl Rows {
    /// Adds a row unless `value` is empty.
    fn add(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() {
            self.0.push((name.to_owned(), value));
        }
    }
}

fn kind(stream: &Stream) -> &'static str {
    if stream.disposition().contains(Disposition::ATTACHED_PIC) {
        return "Cover picture";
    }
    match stream.parameters().medium() {
        Medium::Video => "Video",
        Medium::Audio => "Audio",
        Medium::Subtitle => "Subtitle",
        Medium::Data => "Data",
        Medium::Attachment => "Attachment",
        Medium::Unknown => "Stream",
    }
}

/// `alone`: the only stream of its kind.
fn stream_rows(
    stream: &Stream,
    container: &[(String, String)],
    alone: bool,
) -> Vec<(String, String)> {
    let params = stream.parameters();
    // SAFETY: plain field reads from the stream's codec parameters, which
    // live as long as the input.
    let p = unsafe { &*params.as_ptr() };
    let mut rows = Rows::default();
    rows.add("Codec", codec_name(p.codec_id));
    rows.add("Codec ID", fourcc(p.codec_tag));
    if p.profile >= 0 {
        // SAFETY: returns a static string or null.
        let profile = unsafe { text(ffi::avcodec_profile_name(p.codec_id, p.profile)) };
        rows.add("Profile", profile.unwrap_or_default());
    }
    rows.add("Level", level(params.id(), p.level));
    match params.medium() {
        Medium::Video => video_rows(&mut rows, stream, p),
        Medium::Audio => audio_rows(&mut rows, p),
        _ => {}
    }
    if p.bit_rate > 0 {
        rows.add("Bit rate", bit_rate(p.bit_rate));
    }
    // "Default" picks the track that plays among several of a kind; data
    // tracks are all marked default and aren't played.
    let choice = !alone
        && matches!(
            params.medium(),
            Medium::Video | Medium::Audio | Medium::Subtitle
        );
    rows.add("Flags", flags(stream.disposition(), choice));
    let own: Vec<(String, String)> = stream
        .metadata()
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    rows.0.extend(tags(&own, container));
    rows.0
}

fn video_rows(rows: &mut Rows, stream: &Stream, p: &ffi::AVCodecParameters) {
    if p.width > 0 && p.height > 0 {
        rows.add("Size", format!("{} × {}", p.width, p.height));
        let sar = p.sample_aspect_ratio;
        let (mut width, mut height) = (i64::from(p.width), i64::from(p.height));
        if sar.num > 0 && sar.den > 0 && sar.num != sar.den {
            rows.add("Pixel aspect ratio", format!("{}:{}", sar.num, sar.den));
            width *= i64::from(sar.num);
            height *= i64::from(sar.den);
        }
        rows.add("Display aspect ratio", aspect(width, height));
    }
    rows.add(
        "Rotation",
        match super::rotation(stream) {
            90 => "90° clockwise",
            180 => "180°",
            270 => "90° counterclockwise",
            _ => "",
        },
    );
    rows.add("Frame rate", frame_rate(stream));
    if stream.frames() > 0 {
        rows.add("Frame count", stream.frames().to_string());
    }
    use ffi::AVFieldOrder::*;
    rows.add(
        "Scan type",
        match p.field_order {
            AV_FIELD_PROGRESSIVE => "Progressive",
            AV_FIELD_TT | AV_FIELD_TB => "Interlaced, top field first",
            AV_FIELD_BB | AV_FIELD_BT => "Interlaced, bottom field first",
            _ => "",
        },
    );
    if let Some(format) = pixel_format(p.format) {
        rows.add("Pixel format", format.name);
        rows.add("Bit depth", format!("{} bits", format.depth));
        rows.add("Chroma subsampling", format.chroma);
    }
    rows.add(
        "Color range",
        match p.color_range {
            ffi::AVColorRange::AVCOL_RANGE_MPEG => "Limited",
            ffi::AVColorRange::AVCOL_RANGE_JPEG => "Full",
            _ => "",
        },
    );
    // SAFETY: these return static strings or null.
    let (primaries, transfer, matrix) = unsafe {
        (
            text(ffi::av_color_primaries_name(p.color_primaries)),
            text(ffi::av_color_transfer_name(p.color_trc)),
            text(ffi::av_color_space_name(p.color_space)),
        )
    };
    rows.add("Color primaries", color_name(primaries));
    rows.add("Transfer", color_name(transfer));
    rows.add("Matrix", color_name(matrix));
    for side in stream.side_data() {
        let data = side.data();
        match side.kind() {
            SideData::DOVI_CONF => rows.add("Dolby Vision", dolby_vision(data)),
            SideData::MasteringDisplayMetadata => {
                rows.add("Mastering display", mastering_display(data));
            }
            SideData::ContentLightLevel => {
                let level = |i| {
                    u32_at(data, i)
                        .filter(|&nits| nits > 0)
                        .map_or_else(String::new, |nits| format!("{nits} cd/m²"))
                };
                rows.add("Max content light", level(0));
                rows.add("Max frame-average light", level(1));
            }
            SideData::DataSpherical => rows.add("Projection", projection(data)),
            SideData::Stereo3d => rows.add("Stereo 3D", stereo(data)),
            _ => {}
        }
    }
}

fn audio_rows(rows: &mut Rows, p: &ffi::AVCodecParameters) {
    let channels = p.ch_layout.nb_channels;
    if channels > 0 {
        rows.add("Channels", channels.to_string());
        let mut buffer: [c_char; 64] = [0; 64];
        // SAFETY: writes a NUL-terminated (possibly cut short) description.
        let written = unsafe {
            ffi::av_channel_layout_describe(&p.ch_layout, buffer.as_mut_ptr(), buffer.len())
        };
        // SAFETY: NUL-terminated by the call above when it succeeded.
        let layout = (written > 0)
            .then(|| unsafe { text(buffer.as_ptr()) })
            .flatten()
            .unwrap_or_default();
        if !layout.ends_with(" channels") {
            rows.add("Channel layout", layout);
        }
    }
    if p.sample_rate > 0 {
        let khz = decimals(f64::from(p.sample_rate) / 1000.0, 3);
        rows.add("Sample rate", format!("{khz} kHz"));
    }
    if p.bits_per_raw_sample > 0 {
        rows.add("Bit depth", format!("{} bits", p.bits_per_raw_sample));
    }
}

/// A string from FFmpeg, if any.
///
/// # Safety
/// `ptr` is null or points to a NUL-terminated string.
unsafe fn text(ptr: *const c_char) -> Option<String> {
    (!ptr.is_null()).then(|| {
        // SAFETY: per the contract above.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    })
}

fn codec_name(id: ffi::AVCodecID) -> String {
    if id == ffi::AVCodecID::AV_CODEC_ID_NONE {
        return String::new();
    }
    // SAFETY: descriptors are static (null for unknown ids).
    unsafe {
        let descriptor = ffi::avcodec_descriptor_get(id);
        if descriptor.is_null() {
            return String::new();
        }
        text((*descriptor).long_name)
            .or_else(|| text((*descriptor).name))
            .unwrap_or_default()
    }
}

/// A codec tag ("avc1", "hvc1", "mp4a"), if it is printable text.
fn fourcc(tag: u32) -> String {
    let bytes = tag.to_le_bytes();
    if tag == 0 || !bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// The codec's level as people write it ("4.1"); empty where unknown.
fn level(id: Id, level: i32) -> String {
    if level <= 0 {
        return String::new();
    }
    let tenths = |l: i32| {
        if l % 10 == 0 {
            (l / 10).to_string()
        } else {
            format!("{}.{}", l / 10, l % 10)
        }
    };
    match id {
        Id::H264 if level == 9 => "1b".to_owned(),
        Id::H264 | Id::VP9 => tenths(level),
        // HEVC's general_level_idc is 30 × the level.
        Id::HEVC => tenths(level / 3),
        // AV1's seq_level_idx: 2.0 is 0, 2.1 is 1, ..., 7.3 is 23.
        Id::AV1 if level < 24 => format!("{}.{}", 2 + level / 4, level % 4),
        _ => String::new(),
    }
}

/// "16:9", or "2.39:1" when the ratio doesn't reduce to small numbers.
fn aspect(width: i64, height: i64) -> String {
    fn gcd(a: i64, b: i64) -> i64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    if width <= 0 || height <= 0 {
        return String::new();
    }
    let divisor = gcd(width, height);
    let (w, h) = (width / divisor, height / divisor);
    if w <= 32 && h <= 32 {
        format!("{w}:{h}")
    } else {
        format!("{}:1", decimals(width as f64 / height as f64, 2))
    }
}

/// The average frame rate (the nominal one if the average is unknown).
fn frame_rate(stream: &Stream) -> String {
    [stream.avg_frame_rate(), stream.rate()]
        .into_iter()
        .filter(|r| r.numerator() > 0 && r.denominator() > 0)
        .map(|r| f64::from(r.numerator()) / f64::from(r.denominator()))
        .find(|&fps| fps < 1000.0)
        .map_or_else(String::new, |fps| format!("{} fps", decimals(fps, 3)))
}

struct PixelFormat {
    name: String,
    depth: i32,
    chroma: &'static str,
}

fn pixel_format(format: i32) -> Option<PixelFormat> {
    if format < 0 {
        return None;
    }
    // Found by walking the descriptor list, so a raw number never has to
    // be turned into the enum.
    // SAFETY: the descriptors are static and walked with FFmpeg's iterator.
    unsafe {
        let mut descriptor = std::ptr::null();
        loop {
            descriptor = ffi::av_pix_fmt_desc_next(descriptor);
            if descriptor.is_null() {
                return None;
            }
            if ffi::av_pix_fmt_desc_get_id(descriptor) as i32 == format {
                break;
            }
        }
        let d = &*descriptor;
        let rgb = (d.flags & ffi::AV_PIX_FMT_FLAG_RGB as u64) != 0;
        let chroma = if rgb {
            "None (RGB)"
        } else if d.nb_components < 3 {
            "None (monochrome)"
        } else {
            match (d.log2_chroma_w, d.log2_chroma_h) {
                (0, 0) => "4:4:4",
                (1, 0) => "4:2:2",
                (1, 1) => "4:2:0",
                (0, 1) => "4:4:0",
                (2, 0) => "4:1:1",
                (2, 2) => "4:1:0",
                _ => "",
            }
        };
        Some(PixelFormat {
            name: text(d.name)?,
            depth: d.comp[0].depth,
            chroma,
        })
    }
}

/// FFmpeg's color names as they are usually written; unset ones are empty.
fn color_name(name: Option<String>) -> String {
    let Some(name) = name else {
        return String::new();
    };
    match name.as_str() {
        "unknown" | "reserved" | "unspecified" => "",
        "bt709" => "BT.709",
        "bt470m" => "BT.470 M",
        "bt470bg" => "BT.601 (625 lines)",
        "smpte170m" => "BT.601 (525 lines)",
        "smpte240m" => "SMPTE 240M",
        "bt2020" | "bt2020nc" | "bt2020-10" | "bt2020-12" => "BT.2020",
        "bt2020c" => "BT.2020 (constant luminance)",
        "smpte2084" => "PQ (SMPTE ST 2084)",
        "arib-std-b67" => "HLG (ARIB STD-B67)",
        "smpte431" => "DCI-P3",
        "smpte432" => "Display P3",
        "iec61966-2-1" => "sRGB",
        "linear" => "Linear",
        "gbr" => "RGB",
        "ycgco" => "YCgCo",
        other => other,
    }
    .to_owned()
}

fn u32_at(data: &[u8], index: usize) -> Option<u32> {
    let bytes = data.get(index * 4..index * 4 + 4)?;
    Some(u32::from_ne_bytes(bytes.try_into().ok()?))
}

fn i32_at(data: &[u8], index: usize) -> Option<i32> {
    u32_at(data, index).map(|v| v as i32)
}

/// An AVDOVIDecoderConfigurationRecord: "Profile 8.4, level 6, HLG compatible".
fn dolby_vision(data: &[u8]) -> String {
    let [_, _, profile, level, _, enhancement, _, compatibility, ..] = *data else {
        return String::new();
    };
    // Profile 8's variants are named by their compatibility (8.1, 8.2, 8.4).
    let mut text = if profile == 8 && compatibility > 0 {
        format!("Profile 8.{compatibility}")
    } else {
        format!("Profile {profile}")
    };
    if level > 0 {
        text += &format!(", level {level}");
    }
    let compatible = match compatibility {
        1 => "HDR10",
        2 => "SDR",
        4 => "HLG",
        6 => "Blu-ray HDR10",
        _ => "",
    };
    if !compatible.is_empty() {
        text += &format!(", {compatible} compatible");
    }
    if enhancement != 0 {
        text += ", enhancement layer";
    }
    text
}

/// An AVMasteringDisplayMetadata: "Display P3, 0.0001–1000 cd/m²".
fn mastering_display(data: &[u8]) -> String {
    // Ten rationals (R, G, B and white x/y, min and max luminance), then
    // has_primaries and has_luminance.
    let rational = |i: usize| {
        let (num, den) = (i32_at(data, 2 * i)?, i32_at(data, 2 * i + 1)?);
        (den > 0).then(|| f64::from(num) / f64::from(den))
    };
    let (Some(has_primaries), Some(has_luminance)) = (i32_at(data, 20), i32_at(data, 21)) else {
        return String::new();
    };
    let mut parts = Vec::new();
    if has_primaries != 0 {
        let xy: Option<Vec<f64>> = (0..8).map(rational).collect();
        if let Some(xy) = xy {
            let known = [
                (
                    "Display P3",
                    [0.680, 0.320, 0.265, 0.690, 0.150, 0.060, 0.3127, 0.3290],
                ),
                (
                    "BT.2020",
                    [0.708, 0.292, 0.170, 0.797, 0.131, 0.046, 0.3127, 0.3290],
                ),
                (
                    "BT.709",
                    [0.640, 0.330, 0.300, 0.600, 0.150, 0.060, 0.3127, 0.3290],
                ),
                (
                    "DCI-P3",
                    [0.680, 0.320, 0.265, 0.690, 0.150, 0.060, 0.314, 0.351],
                ),
            ];
            let named = known.iter().find(|(_, reference)| {
                reference
                    .iter()
                    .zip(&xy)
                    .all(|(r, v)| (r - v).abs() < 0.002)
            });
            parts.push(match named {
                Some((name, _)) => (*name).to_owned(),
                None => format!(
                    "R {:.3},{:.3} G {:.3},{:.3} B {:.3},{:.3} white {:.4},{:.4}",
                    xy[0], xy[1], xy[2], xy[3], xy[4], xy[5], xy[6], xy[7]
                ),
            });
        }
    }
    if has_luminance != 0
        && let (Some(min), Some(max)) = (rational(8), rational(9))
    {
        parts.push(format!("{}–{} cd/m²", decimals(min, 4), decimals(max, 0)));
    }
    parts.join(", ")
}

/// An AVSphericalMapping's projection (360° video).
fn projection(data: &[u8]) -> String {
    match u32_at(data, 0) {
        Some(0) => "Equirectangular (360°)",
        Some(1) => "Cubemap (360°)",
        Some(2) => "Equirectangular, tiled (360°)",
        Some(3) => "Half equirectangular (180°)",
        Some(4) => "Rectilinear",
        Some(5) => "Fisheye",
        _ => "",
    }
    .to_owned()
}

/// An AVStereo3D's packing.
fn stereo(data: &[u8]) -> String {
    match u32_at(data, 0) {
        Some(1) => "Side by side",
        Some(2) => "Top and bottom",
        Some(3) => "Frame sequence",
        Some(4) => "Checkerboard",
        Some(5) => "Side by side (quincunx)",
        Some(6) => "Interleaved lines",
        Some(7) => "Interleaved columns",
        _ => "",
    }
    .to_owned()
}

/// The stream's disposition: "Default, forced"; "default" only where it
/// is a `choice` between tracks.
fn flags(disposition: Disposition, choice: bool) -> String {
    let names = [
        (Disposition::DEFAULT, "default"),
        (Disposition::FORCED, "forced"),
        (Disposition::ORIGINAL, "original"),
        (Disposition::DUB, "dub"),
        (Disposition::COMMENT, "commentary"),
        (Disposition::LYRICS, "lyrics"),
        (Disposition::KARAOKE, "karaoke"),
        (Disposition::HEARING_IMPAIRED, "hearing impaired"),
        (Disposition::VISUAL_IMPAIRED, "visually impaired"),
        (Disposition::CLEAN_EFFECTS, "clean effects"),
        (Disposition::CAPTIONS, "captions"),
        (Disposition::DESCRIPTIONS, "descriptions"),
        (Disposition::METADATA, "metadata"),
    ];
    let set: Vec<&str> = names
        .iter()
        .filter(|(flag, _)| disposition.contains(*flag))
        .filter(|(flag, _)| choice || *flag != Disposition::DEFAULT)
        .map(|(_, name)| *name)
        .collect();
    sentence(&set.join(", "))
}

/// Tags as rows, named for people and with dates, places and numbers
/// written out. Tags `skip` has too (a stream repeating the container's
/// creation time) are left out.
fn tags(pairs: &[(String, String)], skip: &[(String, String)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .filter(|(key, value)| {
            let value = value.trim();
            // vendor_id "[0][0][0][0]", language "und": nothing to say.
            let empty = value.is_empty()
                || value.chars().all(|c| matches!(c, '[' | ']' | '0'))
                || (key == "language" && value == "und");
            // QuickTime gives "location-eng" next to "location" with the same value.
            let translation = key.rsplit_once('-').is_some_and(|(base, language)| {
                language.len() == 3
                    && language.bytes().all(|b| b.is_ascii_lowercase())
                    && pairs.iter().any(|(k, v)| k == base && v.trim() == value)
            });
            !empty && !translation && !skip.iter().any(|(k, v)| k == key && v == value)
        })
        .map(|(key, value)| (tag_name(key), tag_value(key, value.trim())))
        .collect()
}

/// "com.apple.quicktime.location.ISO6709" → "Location", "major_brand" →
/// "Major brand".
fn tag_name(key: &str) -> String {
    let short = [
        "com.apple.quicktime.",
        "com.apple.",
        "com.samsung.android.",
        "com.android.",
    ]
    .iter()
    .find_map(|prefix| key.strip_prefix(prefix))
    .unwrap_or(key);
    // Matroska writes its tags in capitals ("ENCODER", "HANDLER_NAME").
    let lowered;
    let short = if short.chars().any(char::is_lowercase) {
        short
    } else {
        lowered = short.to_lowercase();
        &lowered
    };
    let known = match short {
        "creationdate" => "Creation date",
        "location.ISO6709" => "Location",
        "capture.fps" => "Capture frame rate",
        "version" if key.starts_with("com.android.") => "Android version",
        "vendor_id" => "Vendor ID",
        "handler_name" => "Handler",
        // Matroska's statistics tag (mkvmerge).
        "bps" => "Bits per second",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_owned();
    }
    // Words split at separators and lower-to-upper case changes; words in
    // capitals (ISO, GPS) stay so.
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for c in short.chars() {
        if matches!(c, '.' | '_' | '-' | ' ') || (previous_lower && c.is_uppercase()) {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            previous_lower = false;
            if !c.is_alphanumeric() {
                continue;
            }
        }
        previous_lower = c.is_lowercase();
        word.push(c);
    }
    if !word.is_empty() {
        words.push(word);
    }
    let words: Vec<String> = words
        .into_iter()
        .map(|w| {
            let capitals = w
                .chars()
                .filter(|c| c.is_alphabetic())
                .all(char::is_uppercase);
            if capitals && w.len() > 1 {
                w
            } else {
                w.to_lowercase()
            }
        })
        .collect();
    sentence(&words.join(" "))
}

fn tag_value(key: &str, value: &str) -> String {
    let lower = key.to_lowercase();
    if lower == "compatible_brands" && value.len().is_multiple_of(4) && value.is_ascii() {
        // "isommp42" → "isom, mp42"
        let brands: Vec<&str> = (0..value.len() / 4)
            .map(|i| value[i * 4..i * 4 + 4].trim())
            .collect();
        return brands.join(", ");
    }
    if (lower.contains("location") || lower.contains("iso6709"))
        && let Some(place) = iso6709(value)
    {
        return place;
    }
    if (lower.contains("date") || lower.contains("time"))
        && let Some(local) = glib::DateTime::from_iso8601(value, Some(&glib::TimeZone::utc()))
            .ok()
            .and_then(|t| t.to_local().ok())
            .and_then(|t| t.format("%Y-%m-%d %H:%M:%S").ok())
    {
        return local.to_string();
    }
    // "240.000000" → "240"
    if value.contains('.')
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
        && let Ok(number) = value.parse::<f64>()
    {
        return decimals(number, 6);
    }
    if value.chars().count() > MAX_VALUE {
        let cut: String = value.chars().take(MAX_VALUE).collect();
        return format!("{cut}…");
    }
    value.to_owned()
}

/// An ISO 6709 position in decimal degrees ("+37.3349-122.0090+012.345/")
/// as "37.33490, -122.00900, 12 m".
fn iso6709(value: &str) -> Option<String> {
    let body = value.split(['/', 'C']).next()?;
    let mut numbers = Vec::new();
    let mut start = None;
    for (i, c) in body.char_indices() {
        if c == '+' || c == '-' {
            if let Some(s) = start {
                numbers.push(body[s..i].parse::<f64>().ok()?);
            }
            start = Some(i);
        } else if !(c.is_ascii_digit() || c == '.') {
            return None;
        }
    }
    numbers.push(body[start?..].parse::<f64>().ok()?);
    let (&latitude, &longitude) = (numbers.first()?, numbers.get(1)?);
    // Degrees-and-minutes forms (+4012.22) aren't decimal degrees.
    if numbers.len() > 3 || latitude.abs() > 90.0 || longitude.abs() > 180.0 {
        return None;
    }
    let mut text = format!("{latitude:.5}, {longitude:.5}");
    if let Some(altitude) = numbers.get(2) {
        text += &format!(", {} m", altitude.round());
    }
    Some(text)
}

fn chapters(input: &ffmpeg::format::context::Input) -> Option<Section> {
    let mut rows: Vec<(String, String)> = input
        .chapters()
        .take(MAX_CHAPTERS)
        .enumerate()
        .map(|(i, chapter)| {
            let base = chapter.time_base();
            let start = chapter.start() as f64 * f64::from(base.numerator())
                / f64::from(base.denominator().max(1));
            let title = chapter
                .metadata()
                .get("title")
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Chapter {}", i + 1));
            (clock(start, false), title)
        })
        .collect();
    let more = input.nb_chapters() as usize - rows.len();
    if more > 0 {
        rows.push(("…".to_owned(), format!("{more} more")));
    }
    (!rows.is_empty()).then(|| ("Chapters".to_owned(), rows))
}

/// "1:02:03" or "2:03", with hundredths of a second when `fraction`.
fn clock(seconds: f64, fraction: bool) -> String {
    let hundredths = if fraction {
        (seconds.max(0.0) * 100.0).round() as u64
    } else {
        seconds.max(0.0).floor() as u64 * 100
    };
    let (hours, rest) = (hundredths / 360_000, hundredths % 360_000);
    let (minutes, rest) = (rest / 6000, rest % 6000);
    let (whole, part) = (rest / 100, rest % 100);
    let mut text = if hours > 0 {
        format!("{hours}:{minutes:02}:{whole:02}")
    } else {
        format!("{minutes}:{whole:02}")
    };
    if fraction {
        text += &format!(".{part:02}");
    }
    text
}

fn bit_rate(bits_per_second: i64) -> String {
    let bits = bits_per_second as f64;
    if bits >= 1e6 {
        format!("{} Mb/s", decimals(bits / 1e6, 1))
    } else if bits >= 1e3 {
        format!("{} kb/s", (bits / 1e3).round())
    } else {
        format!("{bits} b/s")
    }
}

/// `value` with at most `places` decimals and no trailing zeros ("29.97", "30").
fn decimals(value: f64, places: usize) -> String {
    let text = format!("{value:.places$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

/// The first letter in capitals.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_tags() {
        assert_eq!(tag_name("major_brand"), "Major brand");
        assert_eq!(tag_name("com.apple.quicktime.make"), "Make");
        assert_eq!(tag_name("com.apple.quicktime.location.ISO6709"), "Location");
        assert_eq!(
            tag_name("com.apple.quicktime.live-photo.vitality-score"),
            "Live photo vitality score"
        );
        assert_eq!(tag_name("com.android.version"), "Android version");
        assert_eq!(tag_name("com.android.capture.fps"), "Capture frame rate");
        assert_eq!(tag_name("creation_time"), "Creation time");
        assert_eq!(tag_name("handler_name"), "Handler");
        assert_eq!(tag_name("ENCODER"), "Encoder");
        assert_eq!(tag_name("HANDLER_NAME"), "Handler");
        assert_eq!(
            tag_name("_STATISTICS_WRITING_APP"),
            "Statistics writing app"
        );
        assert_eq!(tag_name("NUMBER_OF_FRAMES"), "Number of frames");
    }

    #[test]
    fn writes_values() {
        assert_eq!(tag_value("compatible_brands", "isommp42"), "isom, mp42");
        assert_eq!(
            tag_value("location", "+37.3349-122.0090+012.345/"),
            "37.33490, -122.00900, 12 m"
        );
        assert_eq!(
            tag_value("location", "+48.8584+002.2945/"),
            "48.85840, 2.29450"
        );
        assert_eq!(tag_value("com.android.capture.fps", "240.000000"), "240");
        assert_eq!(tag_value("encoder", "Lavf60.16.100"), "Lavf60.16.100");
        assert_eq!(tag_value("timecode", "01:00:00:00"), "01:00:00:00");
        // Degrees and minutes stay as written.
        assert_eq!(iso6709("+4012.22-07500.25/"), None);
    }

    #[test]
    fn skips_repeats() {
        let pairs = [
            ("location".to_owned(), "+48.8584+002.2945/".to_owned()),
            ("location-eng".to_owned(), "+48.8584+002.2945/".to_owned()),
            ("vendor_id".to_owned(), "[0][0][0][0]".to_owned()),
            ("language".to_owned(), "und".to_owned()),
            ("creation_time".to_owned(), "2024".to_owned()),
        ];
        let container = [("creation_time".to_owned(), "2024".to_owned())];
        assert_eq!(
            tags(&pairs, &container),
            [("Location".to_owned(), "48.85840, 2.29450".to_owned())]
        );
    }

    #[test]
    fn writes_numbers() {
        assert_eq!(level(Id::H264, 41), "4.1");
        assert_eq!(level(Id::H264, 40), "4");
        assert_eq!(level(Id::HEVC, 153), "5.1");
        assert_eq!(level(Id::HEVC, 120), "4");
        assert_eq!(level(Id::AV1, 9), "4.1");
        assert_eq!(level(Id::MPEG2VIDEO, 8), "");
        assert_eq!(aspect(1920, 1080), "16:9");
        assert_eq!(aspect(1080, 1920), "9:16");
        assert_eq!(aspect(720 * 8, 480 * 9), "4:3");
        assert_eq!(aspect(3840, 1608), "2.39:1");
        assert_eq!(clock(65.27, true), "1:05.27");
        assert_eq!(clock(3723.9, false), "1:02:03");
        assert_eq!(bit_rate(17_340_000), "17.3 Mb/s");
        assert_eq!(bit_rate(128_000), "128 kb/s");
        assert_eq!(bit_rate(160), "160 b/s");
        assert_eq!(decimals(30000.0 / 1001.0, 3), "29.97");
        assert_eq!(fourcc(u32::from_le_bytes(*b"hvc1")), "hvc1");
        assert_eq!(fourcc(0x1b), "");
    }

    #[test]
    fn reads_hdr_side_data() {
        let dovi = [1, 0, 8, 6, 1, 0, 1, 4];
        assert_eq!(dolby_vision(&dovi), "Profile 8.4, level 6, HLG compatible");
        let mut mastering = Vec::new();
        let rationals = [
            (34000, 50000),
            (16000, 50000),
            (13250, 50000),
            (34500, 50000),
            (7500, 50000),
            (3000, 50000),
            (15635, 50000),
            (16450, 50000),
            (1, 10000),
            (1000, 1),
        ];
        for (num, den) in rationals {
            mastering.extend_from_slice(&i32::to_ne_bytes(num));
            mastering.extend_from_slice(&i32::to_ne_bytes(den));
        }
        mastering.extend_from_slice(&1i32.to_ne_bytes());
        mastering.extend_from_slice(&1i32.to_ne_bytes());
        assert_eq!(
            mastering_display(&mastering),
            "Display P3, 0.0001–1000 cd/m²"
        );
    }

    /// `FASTVIZ_VIDEO=/path/to/clip.mp4 cargo test --release video_details -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn video_details() {
        let path = std::env::var("FASTVIZ_VIDEO").expect("set FASTVIZ_VIDEO");
        let started = std::time::Instant::now();
        let sections = details(Path::new(&path));
        println!("({:.1} ms)", started.elapsed().as_secs_f64() * 1e3);
        for (title, rows) in sections {
            println!("== {title}");
            for (name, value) in rows {
                println!("{name}: {value}");
            }
        }
    }
}
