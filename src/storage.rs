//! What kind of storage a folder lives on, to pick how many parallel reads
//! it tolerates. Only reads /proc and /sys (kernel memory, not disk).

use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// Whether `path` is on a spinning disk. `None` if unknown (network, FUSE
/// without a block device, ...).
pub fn is_rotational(path: &Path) -> Option<bool> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let (dev, source) = mount_for(&mountinfo, path)?;
    // fuseblk (NTFS-3G) and friends report an anonymous 0:N device; use the source.
    let block = if dev.starts_with("0:") {
        let name = Path::new(source).file_name()?;
        fs::canonicalize(Path::new("/sys/class/block").join(name)).ok()?
    } else {
        fs::canonicalize(format!("/sys/dev/block/{dev}")).ok()?
    };
    // A partition's queue settings live on its parent disk.
    let disk: PathBuf = if block.join("partition").exists() {
        block.parent()?.to_path_buf()
    } else {
        block
    };
    let flag = fs::read_to_string(disk.join("queue/rotational")).ok()?;
    Some(flag.trim() == "1")
}

/// The (major:minor, source) of the deepest mount containing `path`.
fn mount_for<'a>(mountinfo: &'a str, path: &Path) -> Option<(&'a str, &'a str)> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let mount_point = unescape(fields.get(4)?);
            let dash = fields.iter().position(|f| *f == "-")?;
            let source = fields.get(dash + 2)?;
            path.starts_with(&mount_point).then_some((
                mount_point.as_os_str().len(),
                *fields.get(2)?,
                *source,
            ))
        })
        .max_by_key(|(len, _, _)| *len)
        .map(|(_, dev, source)| (dev, source))
}

/// mountinfo escapes spaces and a few other bytes as \ooo octal.
fn unescape(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let code = (bytes[i] == b'\\')
            .then(|| bytes.get(i + 1..i + 4))
            .flatten()
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match code {
            Some(code) => {
                out.push(code);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
24 1 259:3 / / rw,relatime shared:1 - ext4 /dev/nvme1n1p3 rw
90 24 0:52 / /media/user/Photos rw,nosuid shared:50 - fuseblk /dev/sdd1 rw
91 24 0:53 / /media/user/Backup\\040Drive rw shared:51 - fuseblk /dev/sdb1 rw";

    #[test]
    fn finds_deepest_mount() {
        assert_eq!(
            mount_for(SAMPLE, Path::new("/media/user/Photos/Scratch/18")),
            Some(("0:52", "/dev/sdd1"))
        );
        assert_eq!(
            mount_for(SAMPLE, Path::new("/home/user")),
            Some(("259:3", "/dev/nvme1n1p3"))
        );
        assert_eq!(
            mount_for(SAMPLE, Path::new("/media/user/Backup Drive/x")),
            Some(("0:53", "/dev/sdb1"))
        );
    }
}
