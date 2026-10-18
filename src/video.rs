//! Video thumbnails and metadata via FFmpeg: one frame from about 10% in.
//! Colors follow the clip's YUV matrix like mpv's playback, and HDR (PQ or
//! HLG) frames are tone-mapped to SDR.

use std::path::Path;
use std::sync::Once;

use ffmpeg::codec::packet::side_data::Type as SideData;
use ffmpeg::color::{Range, Space, TransferCharacteristic};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::video::Video as Frame;
use ffmpeg_next as ffmpeg;
use gtk::glib;
use image::{DynamicImage, RgbImage};

use crate::decode::{Pixels, Thumbnail};
use crate::media::Meta;

/// Packets to read looking for a decodable frame before giving up.
const MAX_PACKETS: usize = 600;
/// Clips shorter than this are thumbnailed from their first frame.
const MIN_SEEK_US: i64 = 2_000_000;
/// SDR reference white in nits (BT.2408): HDR light is measured against it.
const SDR_WHITE: f32 = 203.0;
/// HDR light up to this fraction of SDR white passes through unchanged.
const KNEE: f32 = 0.7;

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = ffmpeg::init();
        // Broken files would otherwise spam stderr; failures show as placeholders.
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Quiet);
    });
}

struct Info {
    /// Display size before rotation (pixel aspect ratio applied).
    width: u32,
    height: u32,
    /// Clockwise rotation to display upright: 0, 90, 180 or 270.
    rotation: u32,
    duration_us: i64,
    taken: Option<i64>,
}

fn info(
    input: &ffmpeg::format::context::Input,
    stream: &ffmpeg::format::stream::Stream,
) -> Option<Info> {
    let params = stream.parameters();
    // SAFETY: plain field reads from valid codec parameters.
    let (width, height, sar) = unsafe {
        let p = &*params.as_ptr();
        (p.width, p.height, p.sample_aspect_ratio)
    };
    if width <= 0 || height <= 0 {
        return None;
    }
    let width = if sar.num > 0 && sar.den > 0 && sar.num != sar.den {
        (i64::from(width) * i64::from(sar.num) / i64::from(sar.den)) as u32
    } else {
        width as u32
    };
    Some(Info {
        width,
        height: height as u32,
        rotation: rotation(stream),
        duration_us: input.duration(),
        taken: creation_time(input, stream),
    })
}

/// The display-matrix rotation (iPhones record portrait clips sideways).
fn rotation(stream: &ffmpeg::format::stream::Stream) -> u32 {
    for side_data in stream.side_data() {
        if side_data.kind() != SideData::DisplayMatrix || side_data.data().len() < 36 {
            continue;
        }
        // SAFETY: a display matrix is nine i32s in suitably aligned side data.
        let ccw = unsafe { ffmpeg::ffi::av_display_rotation_get(side_data.data().as_ptr().cast()) };
        if ccw.is_nan() {
            return 0;
        }
        let clockwise = (-ccw.round() as i64).rem_euclid(360);
        return ((clockwise + 45) / 90 % 4 * 90) as u32;
    }
    0
}

/// When the clip was recorded: Apple's local-time tag first, else the UTC one.
fn creation_time(
    input: &ffmpeg::format::context::Input,
    stream: &ffmpeg::format::stream::Stream,
) -> Option<i64> {
    let (format_tags, stream_tags) = (input.metadata(), stream.metadata());
    ["com.apple.quicktime.creationdate", "creation_time"]
        .iter()
        .filter_map(|key| format_tags.get(key).or_else(|| stream_tags.get(key)))
        .find_map(|text| glib::DateTime::from_iso8601(text, Some(&glib::TimeZone::utc())).ok())
        .map(|time| time.to_unix())
        // Cameras without a clock write 1970 or 1904.
        .filter(|&time| time > 0)
}

fn meta(info: &Info, format: &'static str) -> Meta {
    let (width, height) = if info.rotation % 180 == 90 {
        (info.height, info.width)
    } else {
        (info.width, info.height)
    };
    Meta {
        width,
        height,
        taken: info.taken,
        format,
    }
}

/// Size, rotation and date without decoding anything.
pub fn probe(path: &Path, format: &'static str) -> Option<Meta> {
    init();
    let input = ffmpeg::format::input(path).ok()?;
    let stream = input.streams().best(ffmpeg::media::Type::Video)?;
    let info = info(&input, &stream)?;
    Some(meta(&info, format))
}

/// A frame from about 10% into the clip (the keyframe before that point),
/// fitted within `tier` and rotated upright.
pub fn thumbnail(path: &Path, tier: u32, format: &'static str) -> Result<Thumbnail, String> {
    grab(path, tier, format, true)
}

/// The first frame, where playback starts, fitted within `edge` and upright:
/// the viewer shows it while mpv opens the clip.
pub fn poster(path: &Path, edge: u32, format: &'static str) -> Result<Thumbnail, String> {
    grab(path, edge, format, false)
}

fn grab(path: &Path, tier: u32, format: &'static str, skip_in: bool) -> Result<Thumbnail, String> {
    init();
    let mut input = ffmpeg::format::input(path).map_err(|e| e.to_string())?;
    let (index, info, params) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or("no video stream")?;
        let info = info(&input, &stream).ok_or("video has no size")?;
        (stream.index(), info, stream.parameters())
    };
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(params)
        .and_then(|context| context.decoder().video())
        .map_err(|e| e.to_string())?;

    let seeked = skip_in && info.duration_us > MIN_SEEK_US && {
        let target = info.duration_us / 10;
        input.seek(target, ..target).is_ok()
    };
    let frame = match first_frame(&mut input, &mut decoder, index) {
        Some(frame) => frame,
        // Some files don't seek well; try again from the start.
        None if seeked => {
            input.seek(0, ..0).map_err(|e| e.to_string())?;
            decoder.flush();
            first_frame(&mut input, &mut decoder, index).ok_or("no decodable frame")?
        }
        None => return Err("no decodable frame".to_owned()),
    };

    let scale = (f64::from(tier) / f64::from(info.width.max(info.height))).min(1.0);
    let out_width = ((f64::from(info.width) * scale).round() as u32).max(2);
    let out_height = ((f64::from(info.height) * scale).round() as u32).max(2);
    let hdr = Hdr::of(&frame);
    let mut scaler = scaling::Context::get(
        frame.format(),
        frame.width(),
        frame.height(),
        if hdr.is_some() {
            Pixel::RGB48LE
        } else {
            Pixel::RGB24
        },
        out_width,
        out_height,
        scaling::Flags::AREA,
    )
    .map_err(|e| e.to_string())?;
    set_colorspace(&mut scaler, &frame);
    let mut rgb = Frame::empty();
    scaler.run(&frame, &mut rgb).map_err(|e| e.to_string())?;

    let channels = if hdr.is_some() { 6 } else { 3 };
    let row_len = out_width as usize * channels;
    let mut pixels = Vec::with_capacity(row_len * out_height as usize);
    for row in rgb.data(0).chunks(rgb.stride(0)).take(out_height as usize) {
        pixels.extend_from_slice(row.get(..row_len).ok_or("short video row")?);
    }
    if let Some(hdr) = hdr {
        pixels = hdr.tone_map(&pixels);
    }
    let image = RgbImage::from_raw(out_width, out_height, pixels).ok_or("bad video buffer")?;
    let image = match info.rotation {
        90 => DynamicImage::ImageRgb8(image).rotate90(),
        180 => DynamicImage::ImageRgb8(image).rotate180(),
        270 => DynamicImage::ImageRgb8(image).rotate270(),
        _ => DynamicImage::ImageRgb8(image),
    };
    Ok(Thumbnail {
        pixels: Pixels::Rgb(image.to_rgb8()),
        meta: meta(&info, format),
        native: false,
    })
}

/// Uses the frame's YUV matrix and range, as mpv does (swscale would
/// assume BT.601), so thumbnails and posters match playback.
fn set_colorspace(scaler: &mut scaling::Context, frame: &Frame) {
    use ffmpeg::ffi;
    let matrix = match frame.color_space() {
        Space::BT709 => ffi::SWS_CS_ITU709,
        Space::FCC => ffi::SWS_CS_FCC,
        Space::BT470BG | Space::SMPTE170M => ffi::SWS_CS_ITU601,
        Space::SMPTE240M => ffi::SWS_CS_SMPTE240M,
        Space::BT2020NCL | Space::BT2020CL => ffi::SWS_CS_BT2020,
        // Untagged: HD is BT.709, SD is BT.601 (mpv guesses the same way).
        _ if frame.width() >= 1280 || frame.height() > 576 => ffi::SWS_CS_ITU709,
        _ => ffi::SWS_CS_ITU601,
    };
    let full_range = frame.color_range() == Range::JPEG
        || matches!(
            frame.format(),
            Pixel::YUVJ420P | Pixel::YUVJ422P | Pixel::YUVJ444P | Pixel::YUVJ440P
        );
    // SAFETY: a live scaler; the coefficient tables are static. Fails
    // harmlessly (keeping the defaults) for RGB sources.
    unsafe {
        ffi::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            ffi::sws_getCoefficients(matrix),
            i32::from(full_range),
            ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT),
            1,
            0,
            1 << 16,
            1 << 16,
        );
    }
}

/// HDR transfer functions (BT.2100).
#[derive(Clone, Copy)]
enum Hdr {
    Pq,
    Hlg,
}

impl Hdr {
    fn of(frame: &Frame) -> Option<Self> {
        match frame.color_transfer_characteristic() {
            TransferCharacteristic::SMPTE2084 => Some(Self::Pq),
            TransferCharacteristic::ARIB_STD_B67 => Some(Self::Hlg),
            _ => None,
        }
    }

    /// A code value (0..1) to linear light, relative to SDR white. For HLG
    /// this is scene light; `display` applies its system gamma.
    fn linear(self, code: f32) -> f32 {
        match self {
            Self::Pq => {
                let (m1, m2) = (0.159_301_76, 78.843_75);
                let (c1, c2, c3) = (0.835_937_5, 18.851_563, 18.687_5);
                let p = code.powf(1.0 / m2);
                ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1) * 10_000.0 / SDR_WHITE
            }
            Self::Hlg => {
                let (a, b, c) = (0.178_832_77, 0.284_668_92, 0.559_910_7);
                if code <= 0.5 {
                    code * code / 3.0
                } else {
                    (((code - c) / a).exp() + b) / 12.0
                }
            }
        }
    }

    /// Display light relative to SDR white, in BT.2020 primaries.
    fn display(self, [r, g, b]: [f32; 3]) -> [f32; 3] {
        match self {
            Self::Pq => [r, g, b],
            Self::Hlg => {
                // OOTF for a 1000-nit display: system gamma 1.2.
                let y = 0.2627 * r + 0.6780 * g + 0.0593 * b;
                let gain = 1000.0 / SDR_WHITE * y.max(1e-6).powf(0.2);
                [r * gain, g * gain, b * gain]
            }
        }
    }

    /// 16-bit RGB (little-endian, BT.2020, PQ or HLG) to 8-bit SDR RGB:
    /// light up to the knee passes through, brighter light is compressed
    /// into the remaining headroom (scaling R, G and B together keeps hues).
    fn tone_map(self, pixels: &[u8]) -> Vec<u8> {
        // Lookup tables: 12-bit code values in; gamma 2.2 out (what mpv
        // renders for SDR screens), indexed on a square-root scale, which
        // keeps shadow precision.
        let linear: Vec<f32> = (0..4096u16)
            .map(|i| self.linear(f32::from(i) / 4095.0))
            .collect();
        let gamma: Vec<u8> = (0..4096u16)
            .map(|i| ((f32::from(i) / 4095.0).powf(2.0 / 2.2) * 255.0).round() as u8)
            .collect();
        let light: Vec<[f32; 3]> = pixels
            .as_chunks::<6>()
            .0
            .iter()
            .map(|p| {
                let code =
                    |i: usize| linear[usize::from(u16::from_le_bytes([p[i], p[i + 1]]) >> 4)];
                let [r, g, b] = self.display([code(0), code(2), code(4)]);
                // BT.2020 to BT.709 primaries.
                [
                    (1.6605 * r - 0.5876 * g - 0.0728 * b).max(0.0),
                    (-0.1246 * r + 1.1329 * g - 0.0083 * b).max(0.0),
                    (-0.0182 * r - 0.1006 * g + 1.1187 * b).max(0.0),
                ]
            })
            .collect();
        // The frame's own peak, so dim clips aren't darkened needlessly.
        let peak = light
            .iter()
            .map(|p| p[0].max(p[1]).max(p[2]))
            .fold(1.0f32, f32::max);
        // Past the knee, x = knee + t becomes knee + t / (1 + t / a): slope 1
        // at the knee, and the peak lands on 1.0.
        let (room, span) = (1.0 - KNEE, peak - KNEE);
        let a = if span > room {
            room * span / (span - room)
        } else {
            f32::INFINITY
        };
        let mut out = Vec::with_capacity(light.len() * 3);
        for [r, g, b] in light {
            let max = r.max(g).max(b);
            let scale = if max > KNEE {
                let t = max - KNEE;
                (KNEE + t / (1.0 + t / a)) / max
            } else {
                1.0
            };
            for channel in [r, g, b] {
                let value = (channel * scale).min(1.0);
                out.push(gamma[(value.sqrt() * 4095.0).round() as usize]);
            }
        }
        out
    }
}

fn first_frame(
    input: &mut ffmpeg::format::context::Input,
    decoder: &mut ffmpeg::decoder::Video,
    index: usize,
) -> Option<Frame> {
    let mut frame = Frame::empty();
    let mut packets = 0;
    for (stream, packet) in input.packets() {
        if stream.index() != index {
            continue;
        }
        packets += 1;
        if packets > MAX_PACKETS {
            return None;
        }
        if decoder.send_packet(&packet).is_ok() && decoder.receive_frame(&mut frame).is_ok() {
            return Some(frame);
        }
    }
    let _ = decoder.send_eof();
    decoder.receive_frame(&mut frame).ok().map(|_| frame)
}
