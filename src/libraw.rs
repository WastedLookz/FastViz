//! RAW development through LibRaw (see `libraw_shim.c`).

use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use image::RgbImage;

#[repr(C)]
#[derive(Default)]
struct FvProbe {
    width: c_int,
    height: c_int,
    flip: c_int,
    dng: c_int,
    taken: i64,
}

/// What LibRaw reads from a RAW file's headers.
pub struct Probe {
    /// Visible sensor area, before the orientation is applied.
    pub width: u32,
    pub height: u32,
    /// LibRaw's flip: 0 none, 3 = 180°, 5 = 90° counter-clockwise, 6 = 90° clockwise.
    pub flip: i32,
    pub dng: bool,
    pub taken: Option<i64>,
}

#[repr(C)]
struct FvDetails {
    make: [c_char; 64],
    model: [c_char; 64],
    lens: [c_char; 128],
    software: [c_char; 64],
    artist: [c_char; 64],
    serial: [c_char; 64],
    iso: f32,
    shutter: f32,
    aperture: f32,
    focal: f32,
    flash: f32,
    focal35: c_int,
    raw_width: c_int,
    raw_height: c_int,
    taken: i64,
    latitude: [f32; 3],
    longitude: [f32; 3],
    altitude: f32,
    latref: c_char,
    longref: c_char,
    gps: c_char,
}

/// Shooting details from a RAW file's headers; empty strings and zeros
/// where the file doesn't say.
pub struct Details {
    pub make: String,
    pub model: String,
    pub lens: String,
    pub software: String,
    pub artist: String,
    pub serial: String,
    pub iso: f32,
    /// Seconds.
    pub shutter: f32,
    /// f-number.
    pub aperture: f32,
    /// Millimeters.
    pub focal: f32,
    pub focal35: u32,
    pub flash: bool,
    /// The sensor's full size in pixels.
    pub raw_size: (u32, u32),
    pub taken: Option<i64>,
    /// Signed decimal degrees and meters.
    pub gps: Option<(f64, f64, f64)>,
}

unsafe extern "C" {
    fn fv_raw_details(path: *const c_char, out: *mut FvDetails) -> c_int;
    fn fv_raw_probe(data: *const c_void, len: usize, out: *mut FvProbe) -> c_int;
    fn fv_raw_decode(data: *const c_void, len: usize, error: *mut c_int) -> *mut c_void;
    fn fv_raw_width(image: *const c_void) -> c_int;
    fn fv_raw_height(image: *const c_void) -> c_int;
    fn fv_raw_colors(image: *const c_void) -> c_int;
    fn fv_raw_bits(image: *const c_void) -> c_int;
    fn fv_raw_pixels(image: *const c_void) -> *const c_uchar;
    fn fv_raw_size(image: *const c_void) -> usize;
    fn fv_raw_free(image: *mut c_void);
    fn fv_raw_error(code: c_int) -> *const c_char;
}

/// Develops a RAW file (in memory) to an upright 8-bit sRGB image.
pub fn develop(data: &[u8]) -> Result<RgbImage, String> {
    let mut error = 0;
    // SAFETY: the pointer and length describe `data`; `error` is a valid int.
    let image = unsafe { fv_raw_decode(data.as_ptr().cast(), data.len(), &mut error) };
    if image.is_null() {
        // SAFETY: LibRaw returns a static, NUL-terminated message.
        let message = unsafe { CStr::from_ptr(fv_raw_error(error)) };
        return Err(format!("LibRaw: {}", message.to_string_lossy()));
    }
    // SAFETY: `image` is a live LibRaw result until `fv_raw_free`; its pixel
    // pointer is valid for `fv_raw_size` bytes.
    let result = unsafe {
        let (width, height) = (fv_raw_width(image), fv_raw_height(image));
        if fv_raw_colors(image) == 3 && fv_raw_bits(image) == 8 && width > 0 && height > 0 {
            let pixels = std::slice::from_raw_parts(fv_raw_pixels(image), fv_raw_size(image));
            RgbImage::from_raw(width as u32, height as u32, pixels.to_vec())
        } else {
            None
        }
    };
    // SAFETY: freed once, after the last use above.
    unsafe { fv_raw_free(image) };
    result.ok_or_else(|| "LibRaw returned an unexpected pixel format".to_owned())
}

/// Reads a RAW file's size, orientation and date without developing it.
/// `None` if LibRaw doesn't recognize it as a RAW file (or `data` is cut off).
pub fn probe(data: &[u8]) -> Option<Probe> {
    let mut out = FvProbe::default();
    // SAFETY: the pointer and length describe `data`; `out` is a valid struct
    // matching the C side's layout.
    let rc = unsafe { fv_raw_probe(data.as_ptr().cast(), data.len(), &mut out) };
    (rc == 0 && out.width > 0 && out.height > 0).then(|| Probe {
        width: out.width as u32,
        height: out.height as u32,
        flip: out.flip,
        dng: out.dng != 0,
        taken: (out.taken > 0).then_some(out.taken),
    })
}

/// Reads a RAW file's shooting details (LibRaw reads only the headers).
/// `None` if LibRaw doesn't recognize the file.
pub fn details(path: &Path) -> Option<Details> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero FvDetails is valid (arrays of zero bytes, zero numbers).
    let mut out: FvDetails = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated; `out` matches the C side's layout.
    if unsafe { fv_raw_details(path.as_ptr(), &mut out) } != 0 {
        return None;
    }
    let text = |chars: &[c_char]| {
        let bytes: Vec<u8> = chars
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).trim().to_owned()
    };
    let degrees = |dms: [f32; 3], negative: bool| {
        let value = f64::from(dms[0]) + f64::from(dms[1]) / 60.0 + f64::from(dms[2]) / 3600.0;
        if negative { -value } else { value }
    };
    let gps = (out.gps != 0 && out.latitude[0] + out.longitude[0] != 0.0).then(|| {
        (
            degrees(out.latitude, out.latref as u8 == b'S'),
            degrees(out.longitude, out.longref as u8 == b'W'),
            f64::from(out.altitude),
        )
    });
    Some(Details {
        make: text(&out.make),
        model: text(&out.model),
        lens: text(&out.lens),
        software: text(&out.software),
        artist: text(&out.artist),
        serial: text(&out.serial),
        iso: out.iso,
        shutter: out.shutter,
        aperture: out.aperture,
        focal: out.focal,
        focal35: out.focal35.max(0) as u32,
        flash: out.flash > 0.0,
        raw_size: (out.raw_width.max(0) as u32, out.raw_height.max(0) as u32),
        taken: (out.taken > 0).then_some(out.taken),
        gps,
    })
}
