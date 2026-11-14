//! Just enough TIFF (EXIF) parsing to read and change a few tags in place:
//! the orientation, the pixel size, and the link to the embedded thumbnail.
//! Offsets are relative to the start of the TIFF data (the "II"/"MM" header).

pub const ORIENTATION: u16 = 0x0112;
const EXIF_IFD: u16 = 0x8769;
const PIXEL_X: u16 = 0xA002;
const PIXEL_Y: u16 = 0xA003;
const SHORT: u16 = 3;
const LONG: u16 = 4;

/// The byte order of a TIFF block.
#[derive(Clone, Copy)]
pub struct Tiff {
    big_endian: bool,
}

/// A directory entry: where it starts, its tag, type and count.
#[derive(Clone, Copy, Debug)]
struct Field {
    at: usize,
    tag: u16,
    kind: u16,
    count: u32,
}

impl Field {
    /// Where its value (or the offset of its value) is.
    fn value_at(&self) -> usize {
        self.at + 8
    }
}

impl Tiff {
    pub fn new(data: &[u8]) -> Option<Self> {
        match data.get(..4)? {
            [b'I', b'I', 42, 0] => Some(Self { big_endian: false }),
            [b'M', b'M', 0, 42] => Some(Self { big_endian: true }),
            _ => None,
        }
    }

    fn u16(self, data: &[u8], at: usize) -> Option<u16> {
        let bytes: [u8; 2] = data.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    fn u32(self, data: &[u8], at: usize) -> Option<u32> {
        let bytes: [u8; 4] = data.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    }

    pub fn u16_bytes(self, value: u16) -> [u8; 2] {
        if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }

    fn u32_bytes(self, value: u32) -> [u8; 4] {
        if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }

    fn put(data: &mut [u8], at: usize, bytes: &[u8]) -> Option<()> {
        data.get_mut(at..at.checked_add(bytes.len())?)?
            .copy_from_slice(bytes);
        Some(())
    }

    fn ifd0(self, data: &[u8]) -> Option<usize> {
        self.u32(data, 4).map(|offset| offset as usize)
    }

    /// The entries of the directory at `offset`, and where its link to the
    /// next directory is.
    fn entries(self, data: &[u8], offset: usize) -> Option<(Vec<Field>, usize)> {
        let count = self.u16(data, offset)? as usize;
        let mut fields = Vec::with_capacity(count);
        for i in 0..count {
            let at = offset + 2 + i * 12;
            fields.push(Field {
                at,
                tag: self.u16(data, at)?,
                kind: self.u16(data, at + 2)?,
                count: self.u32(data, at + 4)?,
            });
        }
        let next = offset + 2 + count * 12;
        data.get(next..next + 4)?;
        Some((fields, next))
    }

    fn find(self, data: &[u8], offset: usize, tag: u16) -> Option<Field> {
        self.entries(data, offset)?
            .0
            .into_iter()
            .find(|field| field.tag == tag)
    }
}

/// The orientation value (1-8) in IFD0 and where its two bytes are, if the
/// tag is there as the single SHORT it should be.
pub fn orientation(data: &[u8]) -> Option<(Tiff, usize, u16)> {
    let tiff = Tiff::new(data)?;
    let field = tiff.find(data, tiff.ifd0(data)?, ORIENTATION)?;
    if field.kind != SHORT || field.count != 1 {
        return None;
    }
    let value = tiff.u16(data, field.value_at())?;
    Some((tiff, field.value_at(), value))
}

/// Sets the orientation in place; false if the tag isn't there.
pub fn set_orientation(data: &mut [u8], value: u16) -> bool {
    let Some((tiff, at, _)) = orientation(data) else {
        return false;
    };
    Tiff::put(data, at, &tiff.u16_bytes(value)).is_some()
}

/// A copy of `data` with an orientation tag added to IFD0 (replacing a
/// malformed one). The new IFD0 goes at the end, so every offset into the
/// block (values, the EXIF directory, maker notes) stays valid; the old one
/// is left unused.
pub fn with_orientation(data: &[u8], value: u16) -> Option<Vec<u8>> {
    let tiff = Tiff::new(data)?;
    let (fields, next_at) = tiff.entries(data, tiff.ifd0(data)?)?;
    let next = tiff.u32(data, next_at)?;
    let mut entry = Vec::with_capacity(12);
    entry.extend_from_slice(&tiff.u16_bytes(ORIENTATION));
    entry.extend_from_slice(&tiff.u16_bytes(SHORT));
    entry.extend_from_slice(&tiff.u32_bytes(1));
    // Values shorter than four bytes are left-justified.
    entry.extend_from_slice(&tiff.u16_bytes(value));
    entry.extend_from_slice(&[0, 0]);

    let mut out = data.to_vec();
    if out.len() % 2 == 1 {
        out.push(0);
    }
    let start = out.len();
    let kept: Vec<&Field> = fields.iter().filter(|f| f.tag != ORIENTATION).collect();
    out.extend_from_slice(&tiff.u16_bytes(u16::try_from(kept.len() + 1).ok()?));
    let mut added = false;
    for field in kept {
        if !added && field.tag > ORIENTATION {
            out.extend_from_slice(&entry);
            added = true;
        }
        out.extend_from_slice(data.get(field.at..field.at + 12)?);
    }
    if !added {
        out.extend_from_slice(&entry);
    }
    out.extend_from_slice(&tiff.u32_bytes(next));
    Tiff::put(&mut out, 4, &tiff.u32_bytes(u32::try_from(start).ok()?))?;
    Some(out)
}

/// A TIFF block holding only an orientation tag (big-endian).
pub fn minimal(value: u16) -> Vec<u8> {
    let mut out = b"MM\0\x2a".to_vec();
    out.extend_from_slice(&8u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&ORIENTATION.to_be_bytes());
    out.extend_from_slice(&SHORT.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&value.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

/// Unlinks IFD1, the embedded thumbnail (stale after a crop); its bytes
/// stay but nothing points at them.
pub fn drop_thumbnail(data: &mut [u8]) -> Option<()> {
    let tiff = Tiff::new(data)?;
    let (_, next_at) = tiff.entries(data, tiff.ifd0(data)?)?;
    Tiff::put(data, next_at, &[0; 4])
}

/// Updates the EXIF pixel size (PixelXDimension/PixelYDimension) if present.
pub fn set_pixel_size(data: &mut [u8], width: u32, height: u32) -> Option<()> {
    let tiff = Tiff::new(data)?;
    let pointer = tiff.find(data, tiff.ifd0(data)?, EXIF_IFD)?;
    let exif = tiff.u32(data, pointer.value_at())? as usize;
    for (tag, value) in [(PIXEL_X, width), (PIXEL_Y, height)] {
        let Some(field) = tiff.find(data, exif, tag) else {
            continue;
        };
        match field.kind {
            SHORT => {
                if let Ok(value) = u16::try_from(value) {
                    Tiff::put(data, field.value_at(), &tiff.u16_bytes(value))?;
                }
            }
            LONG => Tiff::put(data, field.value_at(), &tiff.u32_bytes(value))?,
            _ => {}
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian TIFF with IFD0 = {Make (offset value), ExifIFD} and an
    /// EXIF directory with the pixel size, plus a link to an IFD1.
    fn sample() -> Vec<u8> {
        let mut d = b"II\x2a\0".to_vec();
        d.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 at 8: two entries.
        d.extend_from_slice(&2u16.to_le_bytes());
        // Make: ASCII, 6 bytes at offset 50.
        d.extend_from_slice(&0x010Fu16.to_le_bytes());
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&6u32.to_le_bytes());
        d.extend_from_slice(&50u32.to_le_bytes());
        // ExifIFD pointer → 56.
        d.extend_from_slice(&EXIF_IFD.to_le_bytes());
        d.extend_from_slice(&LONG.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&56u32.to_le_bytes());
        // Next IFD (IFD1) at 90.
        d.extend_from_slice(&90u32.to_le_bytes());
        assert_eq!(d.len(), 38);
        d.resize(50, 0);
        d.extend_from_slice(b"Canon\0");
        // EXIF IFD at 56: PixelX (SHORT), PixelY (LONG).
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&PIXEL_X.to_le_bytes());
        d.extend_from_slice(&SHORT.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&[0x40, 0x01, 0, 0]);
        d.extend_from_slice(&PIXEL_Y.to_le_bytes());
        d.extend_from_slice(&LONG.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&240u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        d.resize(100, 0);
        d
    }

    fn exif(data: &[u8]) -> exif::Exif {
        exif::Reader::new().read_raw(data.to_vec()).unwrap()
    }

    #[test]
    fn adds_and_changes_the_orientation() {
        let data = sample();
        assert!(orientation(&data).is_none());
        let mut added = with_orientation(&data, 6).unwrap();
        assert_eq!(orientation(&added).unwrap().2, 6);
        let parsed = exif(&added);
        let get = |e: &exif::Exif, tag| {
            e.get_field(tag, exif::In::PRIMARY)
                .map(|f| f.display_value().to_string())
        };
        // Everything else still reads the same.
        assert_eq!(get(&parsed, exif::Tag::Make).unwrap(), "\"Canon\"");
        assert_eq!(get(&parsed, exif::Tag::PixelXDimension).unwrap(), "320");
        assert!(set_orientation(&mut added, 3));
        assert_eq!(orientation(&added).unwrap().2, 3);
        // Adding again replaces instead of repeating the tag.
        let again = with_orientation(&added, 8).unwrap();
        assert_eq!(orientation(&again).unwrap().2, 8);
        let count = Tiff::new(&again)
            .unwrap()
            .entries(&again, Tiff::new(&again).unwrap().ifd0(&again).unwrap())
            .unwrap()
            .0
            .iter()
            .filter(|f| f.tag == ORIENTATION)
            .count();
        assert_eq!(count, 1);
        assert_eq!(orientation(&minimal(5)).unwrap().2, 5);
    }

    #[test]
    fn updates_pixel_size_and_drops_the_thumbnail() {
        let mut data = sample();
        set_pixel_size(&mut data, 100, 70_000).unwrap();
        drop_thumbnail(&mut data).unwrap();
        let tiff = Tiff::new(&data).unwrap();
        let (_, next_at) = tiff.entries(&data, 8).unwrap();
        assert_eq!(tiff.u32(&data, next_at), Some(0));
        let parsed = exif(&data);
        let size = |tag| {
            parsed
                .get_field(tag, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        };
        assert_eq!(size(exif::Tag::PixelXDimension), Some(100));
        assert_eq!(size(exif::Tag::PixelYDimension), Some(70_000));
    }
}
