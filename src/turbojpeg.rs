//! Minimal safe binding to the system TurboJPEG 2.x API.
//!
//! Ubuntu 24.04 ships libjpeg-turbo 2.1, while the `turbojpeg` crate needs
//! 3.x, so we bind the few stable 2.x calls thumbnails need: header parsing,
//! DCT-scaled decoding and encoding.

use std::ffi::{CStr, c_char, c_int, c_uchar, c_ulong, c_void};

use image::RgbImage;

type Handle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct ScalingFactor {
    num: c_int,
    denom: c_int,
}

const TJPF_RGB: c_int = 0;
const TJSAMP_420: c_int = 2;
const TJCS_CMYK: c_int = 3;
const TJCS_YCCK: c_int = 4;
const TJERR_WARNING: c_int = 0;
const TJFLAG_FASTDCT: c_int = 2048;

#[link(name = "turbojpeg")]
unsafe extern "C" {
    fn tjInitDecompress() -> Handle;
    fn tjInitCompress() -> Handle;
    fn tjDestroy(handle: Handle) -> c_int;
    fn tjDecompressHeader3(
        handle: Handle,
        jpeg: *const c_uchar,
        size: c_ulong,
        width: *mut c_int,
        height: *mut c_int,
        subsamp: *mut c_int,
        colorspace: *mut c_int,
    ) -> c_int;
    fn tjGetScalingFactors(count: *mut c_int) -> *mut ScalingFactor;
    fn tjDecompress2(
        handle: Handle,
        jpeg: *const c_uchar,
        size: c_ulong,
        dst: *mut c_uchar,
        width: c_int,
        pitch: c_int,
        height: c_int,
        pixel_format: c_int,
        flags: c_int,
    ) -> c_int;
    fn tjCompress2(
        handle: Handle,
        src: *const c_uchar,
        width: c_int,
        pitch: c_int,
        height: c_int,
        pixel_format: c_int,
        jpeg: *mut *mut c_uchar,
        size: *mut c_ulong,
        subsamp: c_int,
        quality: c_int,
        flags: c_int,
    ) -> c_int;
    fn tjFree(buffer: *mut c_uchar);
    fn tjGetErrorStr2(handle: Handle) -> *mut c_char;
    fn tjGetErrorCode(handle: Handle) -> c_int;
}

fn error_string(handle: Handle) -> String {
    // SAFETY: TurboJPEG returns a pointer to a static or per-handle C string.
    unsafe {
        let msg = tjGetErrorStr2(handle);
        if msg.is_null() {
            "unknown TurboJPEG error".to_owned()
        } else {
            CStr::from_ptr(msg).to_string_lossy().into_owned()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    /// CMYK/YCCK images can't be decoded to RGB by TurboJPEG.
    pub cmyk: bool,
}

/// A TurboJPEG decompressor. Use one per thread.
pub struct Decoder(Handle);

// SAFETY: a TurboJPEG handle may move between threads; it is only used by one at a time.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Option<Self> {
        // SAFETY: plain constructor; null means failure.
        let handle = unsafe { tjInitDecompress() };
        (!handle.is_null()).then_some(Self(handle))
    }

    pub fn header(&mut self, jpeg: &[u8]) -> Result<Header, String> {
        let (mut width, mut height, mut subsamp, mut colorspace) = (0, 0, 0, 0);
        // SAFETY: the buffer pointer and length describe `jpeg`; outputs are valid ints.
        let status = unsafe {
            tjDecompressHeader3(
                self.0,
                jpeg.as_ptr(),
                jpeg.len() as c_ulong,
                &mut width,
                &mut height,
                &mut subsamp,
                &mut colorspace,
            )
        };
        if status != 0 || width <= 0 || height <= 0 {
            return Err(error_string(self.0));
        }
        Ok(Header {
            width: width as u32,
            height: height as u32,
            cmyk: colorspace == TJCS_CMYK || colorspace == TJCS_YCCK,
        })
    }

    /// Decodes to RGB at the smallest DCT scale (1/8, 1/4, 1/2, 1/1) whose longer
    /// edge is still at least `min_edge`. Truncated files decode partially.
    pub fn decode_scaled(&mut self, jpeg: &[u8], min_edge: u32) -> Result<RgbImage, String> {
        let header = self.header(jpeg)?;
        if header.cmyk {
            return Err("CMYK JPEG".to_owned());
        }
        let (width, height) = scaled_size(header.width, header.height, min_edge);
        let mut pixels = vec![0u8; width as usize * height as usize * 3];
        // SAFETY: `pixels` holds exactly width * height RGB pixels with a pitch of width * 3.
        let status = unsafe {
            tjDecompress2(
                self.0,
                jpeg.as_ptr(),
                jpeg.len() as c_ulong,
                pixels.as_mut_ptr(),
                width as c_int,
                (width * 3) as c_int,
                height as c_int,
                TJPF_RGB,
                TJFLAG_FASTDCT,
            )
        };
        // SAFETY: valid handle.
        if status != 0 && unsafe { tjGetErrorCode(self.0) } != TJERR_WARNING {
            return Err(error_string(self.0));
        }
        RgbImage::from_raw(width, height, pixels).ok_or_else(|| "bad buffer size".to_owned())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from tjInitDecompress and is destroyed once.
        unsafe { tjDestroy(self.0) };
    }
}

/// The smallest supported scaled size whose longer edge is at least `min_edge`.
fn scaled_size(width: u32, height: u32, min_edge: u32) -> (u32, u32) {
    let mut count = 0;
    // SAFETY: returns a static array of `count` factors (or null).
    let factors = unsafe { tjGetScalingFactors(&mut count) };
    let factors: &[ScalingFactor] = if factors.is_null() || count <= 0 {
        &[]
    } else {
        // SAFETY: TurboJPEG guarantees `count` valid entries.
        unsafe { std::slice::from_raw_parts(factors, count as usize) }
    };
    let scale =
        |dim: u32, f: &ScalingFactor| (dim as u64 * f.num as u64).div_ceil(f.denom as u64) as u32;
    // Only 1/1, 1/2, 1/4 and 1/8 have SIMD inverse DCTs; 3/8 and friends are slower.
    factors
        .iter()
        .filter(|f| f.num == 1 || f.num == f.denom)
        .map(|f| (scale(width, f), scale(height, f)))
        .filter(|&(w, h)| w.max(h) >= min_edge)
        .min_by_key(|&(w, h)| w.max(h))
        .unwrap_or((width, height))
}

/// A TurboJPEG compressor. Use one per thread.
pub struct Encoder(Handle);

// SAFETY: as for `Decoder`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Option<Self> {
        // SAFETY: plain constructor; null means failure.
        let handle = unsafe { tjInitCompress() };
        (!handle.is_null()).then_some(Self(handle))
    }

    pub fn encode(&mut self, image: &RgbImage, quality: i32) -> Result<Vec<u8>, String> {
        let mut out: *mut c_uchar = std::ptr::null_mut();
        let mut size: c_ulong = 0;
        // SAFETY: `image` is a packed RGB buffer of width * height pixels; TurboJPEG
        // allocates `out`, which we copy and free below.
        let status = unsafe {
            tjCompress2(
                self.0,
                image.as_raw().as_ptr(),
                image.width() as c_int,
                (image.width() * 3) as c_int,
                image.height() as c_int,
                TJPF_RGB,
                &mut out,
                &mut size,
                TJSAMP_420,
                quality,
                0,
            )
        };
        let result = if status == 0 && !out.is_null() {
            // SAFETY: TurboJPEG wrote `size` bytes to `out`.
            Ok(unsafe { std::slice::from_raw_parts(out, size as usize) }.to_vec())
        } else {
            Err(error_string(self.0))
        };
        if !out.is_null() {
            // SAFETY: `out` was allocated by TurboJPEG.
            unsafe { tjFree(out) };
        }
        result
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from tjInitCompress and is destroyed once.
        unsafe { tjDestroy(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_scaled_decode() {
        let image = RgbImage::from_fn(800, 600, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let jpeg = Encoder::new().unwrap().encode(&image, 85).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let header = decoder.header(&jpeg).unwrap();
        assert_eq!((header.width, header.height), (800, 600));
        // 1/2 is the smallest scale with a longer edge >= 256 (1/4 would give 200).
        let small = decoder.decode_scaled(&jpeg, 256).unwrap();
        assert_eq!(small.dimensions(), (400, 300));
        let full = decoder.decode_scaled(&jpeg, 2000).unwrap();
        assert_eq!(full.dimensions(), (800, 600));
    }

    #[test]
    fn rejects_garbage() {
        let mut decoder = Decoder::new().unwrap();
        assert!(decoder.header(b"not a jpeg").is_err());
        assert!(
            decoder
                .decode_scaled(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0], 128)
                .is_err()
        );
    }
}
