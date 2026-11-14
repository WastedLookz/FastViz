//! MP4, MOV and 3GP (ISO base media files): rotation is the display matrix
//! in each video track's header (`moov/trak/tkhd`), which players apply.
//! Turning a clip rewrites those 36 bytes in place, whatever the file's size;
//! the video itself isn't touched.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// 1.0 and -1.0 in the matrix's 16.16 fixed point.
const ONE: i32 = 0x1_0000;
const MINUS_ONE: i32 = -ONE;
/// The movie header is read whole; real ones are a few MB at most.
const MAX_MOOV: u64 = 256 << 20;

/// A box: its type and where its contents are (in the file or a buffer).
struct Atom {
    kind: [u8; 4],
    body: std::ops::Range<u64>,
}

/// The boxes in `range` of a buffer.
fn atoms(data: &[u8], range: std::ops::Range<usize>) -> Vec<Atom> {
    let mut out = Vec::new();
    let mut at = range.start;
    while at + 8 <= range.end {
        let size = u64::from(u32::from_be_bytes(
            data[at..at + 4].try_into().unwrap_or_default(),
        ));
        let kind: [u8; 4] = data[at + 4..at + 8].try_into().unwrap_or_default();
        let (header, size) = match size {
            0 => (8, (range.end - at) as u64),
            1 => match data.get(at + 8..at + 16) {
                Some(large) => (16, u64::from_be_bytes(large.try_into().unwrap_or_default())),
                None => break,
            },
            size => (8, size),
        };
        let end = at as u64 + size;
        if size < header || end > range.end as u64 {
            break;
        }
        out.push(Atom {
            kind,
            body: at as u64 + header..end,
        });
        at = end as usize;
    }
    out
}

fn child<'a>(atoms: &'a [Atom], kind: &[u8; 4]) -> Option<&'a Atom> {
    atoms.iter().find(|a| &a.kind == kind)
}

fn range(atom: &Atom) -> std::ops::Range<usize> {
    atom.body.start as usize..atom.body.end as usize
}

/// Finds the top-level `moov` box by reading box headers only.
fn find_moov(file: &File, length: u64) -> Result<Atom, String> {
    let mut at = 0;
    let mut first = true;
    while at + 8 <= length {
        let mut header = [0u8; 16];
        let n = if at + 16 <= length { 16 } else { 8 };
        file.read_exact_at(&mut header[..n], at)
            .map_err(|e| e.to_string())?;
        let size = u64::from(u32::from_be_bytes(
            header[..4].try_into().unwrap_or_default(),
        ));
        let kind: [u8; 4] = header[4..8].try_into().unwrap_or_default();
        let known = [
            b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot", b"uuid",
        ];
        if first && !known.contains(&&kind) {
            return Err("it isn't an MP4 or MOV file".to_owned());
        }
        first = false;
        let (header_len, size) = match size {
            0 => (8, length - at),
            1 if n == 16 => (
                16,
                u64::from_be_bytes(header[8..16].try_into().unwrap_or_default()),
            ),
            1 => break,
            size => (8, size),
        };
        if size < header_len {
            break;
        }
        if &kind == b"moov" {
            return Ok(Atom {
                kind,
                body: at + header_len..(at + size).min(length),
            });
        }
        at += size;
    }
    Err("the file has no movie header (moov)".to_owned())
}

/// Quarter turns (0-3, clockwise) of a pure rotation matrix (a, b, c, d).
fn turns_of(a: i32, b: i32, c: i32, d: i32) -> Option<u8> {
    match (a, b, c, d) {
        (ONE, 0, 0, ONE) => Some(0),
        (0, ONE, MINUS_ONE, 0) => Some(1),
        (MINUS_ONE, 0, 0, MINUS_ONE) => Some(2),
        (0, MINUS_ONE, ONE, 0) => Some(3),
        _ => None,
    }
}

/// The 3 x 3 matrix as stored (a, b, u, c, d, v, x, y, w) for `turns`
/// clockwise, as FFmpeg writes it: the translation keeps the picture in
/// view for players that apply the whole matrix.
fn matrix(turns: u8, width: i32, height: i32) -> [i32; 9] {
    let w = 0x4000_0000; // 1.0 in 2.30 fixed point
    match turns % 4 {
        1 => [0, ONE, 0, -ONE, 0, 0, height * ONE, 0, w],
        2 => [-ONE, 0, 0, 0, -ONE, 0, width * ONE, height * ONE, w],
        3 => [0, -ONE, 0, ONE, 0, 0, 0, width * ONE, w],
        _ => [ONE, 0, 0, 0, ONE, 0, 0, 0, w],
    }
}

fn i32_at(data: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Turns every video track `turns` quarter turns clockwise.
pub fn rotate(path: &Path, turns: u8) -> Result<(), String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let moov = find_moov(&file, length)?;
    let size = moov.body.end - moov.body.start;
    if size > MAX_MOOV {
        return Err("its movie header is too large".to_owned());
    }
    let mut data = vec![0u8; size as usize];
    file.read_exact_at(&mut data, moov.body.start)
        .map_err(|e| e.to_string())?;
    let patches = patches(&data, turns)?;
    for (at, bytes) in patches {
        file.write_all_at(&bytes, moov.body.start + at as u64)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The new matrices for the video tracks in a `moov` box's contents, and
/// where they go.
fn patches(moov: &[u8], turns: u8) -> Result<Vec<(usize, Vec<u8>)>, String> {
    let mut out = Vec::new();
    for trak in atoms(moov, 0..moov.len())
        .iter()
        .filter(|a| &a.kind == b"trak")
    {
        let parts = atoms(moov, range(trak));
        let video = child(&parts, b"mdia")
            .map(|mdia| atoms(moov, range(mdia)))
            .and_then(|mdia| {
                let hdlr = child(&mdia, b"hdlr")?;
                // Version and flags, pre_defined, then the handler type.
                let at = hdlr.body.start as usize + 8;
                Some(moov.get(at..at + 4)? == b"vide")
            })
            .unwrap_or(false);
        let Some(tkhd) = child(&parts, b"tkhd").filter(|_| video) else {
            continue;
        };
        let body = range(tkhd);
        let version = *moov.get(body.start).ok_or("bad track header")?;
        // After version/flags: creation and modification times, track id,
        // reserved, duration (32 or 64-bit), 8 reserved, layer, group,
        // volume, reserved; then the matrix, width and height.
        let matrix_at = body.start + if version == 1 { 52 } else { 40 };
        if matrix_at + 44 > body.end {
            return Err("bad track header".to_owned());
        }
        let get = |i: usize| i32_at(moov, matrix_at + i * 4).unwrap_or_default();
        let current = turns_of(get(0), get(1), get(3), get(4))
            .ok_or("the video has an unusual transform (mirrored or scaled)")?;
        let width = (get(9) >> 16).clamp(0, 0x7FFF);
        let height = (get(10) >> 16).clamp(0, 0x7FFF);
        let new = matrix((current + turns) % 4, width, height);
        let bytes = new.iter().flat_map(|v| v.to_be_bytes()).collect();
        out.push((matrix_at, bytes));
    }
    if out.is_empty() {
        return Err("the file has no video track".to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn track(handler: &[u8; 4], matrix: [i32; 9]) -> Vec<u8> {
        let mut tkhd = vec![0u8; 40];
        tkhd.extend(matrix.iter().flat_map(|v| v.to_be_bytes()));
        tkhd.extend_from_slice(&(1920 * ONE).to_be_bytes());
        tkhd.extend_from_slice(&(1080 * ONE).to_be_bytes());
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0; 12]);
        let mdia = atom(b"mdia", &atom(b"hdlr", &hdlr));
        atom(b"trak", &[atom(b"tkhd", &tkhd), mdia].concat())
    }

    #[test]
    fn turns_video_tracks_only() {
        let moov = atom(
            b"moov",
            &[
                atom(b"mvhd", &[0; 100]),
                track(b"vide", matrix(1, 1920, 1080)),
                track(b"soun", matrix(0, 0, 0)),
            ]
            .concat(),
        );
        let mut file = atom(b"ftyp", b"isom\0\0\0\0");
        file.extend(atom(b"mdat", &[1, 2, 3]));
        file.extend_from_slice(&moov);
        let path = std::env::temp_dir().join(format!("fastviz-mp4-{}.mp4", std::process::id()));
        std::fs::write(&path, &file).unwrap();

        rotate(&path, 1).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), file.len());
        let start = file.len() - moov.len() + 8;
        let found = patches(&data[start..], 0).unwrap();
        assert_eq!(found.len(), 1, "only the video track");
        // 90° + 90° = 180°.
        let (at, _) = found[0];
        let get = |i: usize| i32_at(&data[start..], at + i * 4).unwrap();
        assert_eq!(turns_of(get(0), get(1), get(3), get(4)), Some(2));
        assert_eq!((get(6), get(7)), (1920 * ONE, 1080 * ONE));

        rotate(&path, 2).unwrap();
        let data = std::fs::read(&path).unwrap();
        let get = |i: usize| i32_at(&data[start..], at + i * 4).unwrap();
        assert_eq!(turns_of(get(0), get(1), get(3), get(4)), Some(0));
        std::fs::remove_file(&path).unwrap();

        let path = std::env::temp_dir().join(format!("fastviz-notmp4-{}", std::process::id()));
        std::fs::write(&path, b"RIFF....AVI LIST").unwrap();
        assert!(rotate(&path, 1).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
