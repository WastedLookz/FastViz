//! Video thumbnails and metadata via FFmpeg: one frame from about 10% in.

use std::path::Path;
use std::sync::Once;

use ffmpeg::codec::packet::side_data::Type as SideData;
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

    let seeked = info.duration_us > MIN_SEEK_US && {
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
    let mut scaler = scaling::Context::get(
        frame.format(),
        frame.width(),
        frame.height(),
        Pixel::RGB24,
        out_width,
        out_height,
        scaling::Flags::AREA,
    )
    .map_err(|e| e.to_string())?;
    let mut rgb = Frame::empty();
    scaler.run(&frame, &mut rgb).map_err(|e| e.to_string())?;

    let stride = rgb.stride(0);
    let row_len = out_width as usize * 3;
    let mut pixels = Vec::with_capacity(row_len * out_height as usize);
    for row in rgb.data(0).chunks(stride).take(out_height as usize) {
        pixels.extend_from_slice(row.get(..row_len).ok_or("short video row")?);
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
