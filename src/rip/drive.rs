// Optical drive detection. Parses /proc/mounts for /dev/srN entries that
// the desktop's udisks2 auto-mount has placed under /run/media/<user>/.
// A future revision will hook udisks2 D-Bus for live insert/eject events;
// for now we just snapshot on demand.

use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedDisc {
    /// `/dev/sr0` etc.
    pub device: PathBuf,
    /// Mount point — typically `/run/media/<user>/<volume-label>`.
    pub mount_path: PathBuf,
    /// MakeMKV-style disc index, derived from the trailing digits of the
    /// device path (e.g. `/dev/sr2` → `2`). MakeMKV's `disc:N` enumeration
    /// follows `/dev/sr<N>` in our experience.
    pub disc_index: u32,
    /// Volume label, taken from the mount path's basename.
    pub label: Option<String>,
}

pub fn detect_mounted_optical_discs() -> Result<Vec<DetectedDisc>> {
    let text = std::fs::read_to_string("/proc/mounts")?;
    Ok(parse_proc_mounts(&text))
}

pub fn parse_proc_mounts(mounts_text: &str) -> Vec<DetectedDisc> {
    let mut out = Vec::new();
    for line in mounts_text.lines() {
        let mut parts = line.split_whitespace();
        let Some(dev) = parts.next() else { continue };
        let Some(mount) = parts.next() else { continue };
        if let Some(index) = sr_device_index(dev) {
            let mount_path = PathBuf::from(decode_octal_escapes(mount));
            let label = mount_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned());
            out.push(DetectedDisc {
                device: PathBuf::from(dev),
                mount_path,
                disc_index: index,
                label,
            });
        }
    }
    out
}

fn sr_device_index(dev: &str) -> Option<u32> {
    let tail = dev.strip_prefix("/dev/sr")?;
    tail.parse::<u32>().ok()
}

/// `/proc/mounts` octal-escapes spaces (`\040`) and other non-printable
/// characters in the mount-point column.
fn decode_octal_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let d1 = chars.next();
            let d2 = chars.next();
            let d3 = chars.next();
            match (d1, d2, d3) {
                (Some(a), Some(b), Some(c)) if a.is_digit(8) && b.is_digit(8) && c.is_digit(8) => {
                    let n = (a.to_digit(8).unwrap() * 64)
                        + (b.to_digit(8).unwrap() * 8)
                        + c.to_digit(8).unwrap();
                    if let Some(decoded) = char::from_u32(n) {
                        out.push(decoded);
                        continue;
                    }
                }
                _ => {}
            }
            out.push('\\');
            if let Some(a) = d1 { out.push(a); }
            if let Some(b) = d2 { out.push(b); }
            if let Some(c) = d3 { out.push(c); }
        } else {
            out.push(c);
        }
    }
    out
}

#[allow(dead_code)]
pub fn is_optical_device(path: &Path) -> bool {
    path.to_str()
        .and_then(|s| s.strip_prefix("/dev/sr"))
        .and_then(|t| t.parse::<u32>().ok())
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_optical_mount() {
        let text =
            "/dev/sr0 /run/media/rob/DOBIEGILLIS_S1D1 udf ro,nosuid,nodev,relatime 0 0\n";
        let discs = parse_proc_mounts(text);
        assert_eq!(discs.len(), 1);
        assert_eq!(discs[0].device, PathBuf::from("/dev/sr0"));
        assert_eq!(discs[0].disc_index, 0);
        assert_eq!(
            discs[0].mount_path,
            PathBuf::from("/run/media/rob/DOBIEGILLIS_S1D1")
        );
        assert_eq!(discs[0].label.as_deref(), Some("DOBIEGILLIS_S1D1"));
    }

    #[test]
    fn skips_non_optical_mounts() {
        let text = "\
/dev/sda1 / ext4 rw 0 0
proc /proc proc rw,relatime 0 0
/dev/sr0 /run/media/rob/MY_DVD udf ro 0 0
tmpfs /tmp tmpfs rw 0 0
";
        let discs = parse_proc_mounts(text);
        assert_eq!(discs.len(), 1);
        assert_eq!(discs[0].device, PathBuf::from("/dev/sr0"));
    }

    #[test]
    fn handles_multiple_drives_in_order_seen() {
        let text = "\
/dev/sr0 /run/media/rob/DISC_A udf ro 0 0
/dev/sr1 /run/media/rob/DISC_B udf ro 0 0
";
        let discs = parse_proc_mounts(text);
        assert_eq!(discs.len(), 2);
        assert_eq!(discs[0].disc_index, 0);
        assert_eq!(discs[1].disc_index, 1);
    }

    #[test]
    fn decodes_octal_escapes_in_mount_paths() {
        let text =
            "/dev/sr0 /run/media/rob/Some\\040Disc udf ro,nosuid 0 0\n";
        let discs = parse_proc_mounts(text);
        assert_eq!(discs.len(), 1);
        assert_eq!(
            discs[0].mount_path,
            PathBuf::from("/run/media/rob/Some Disc")
        );
    }

    #[test]
    fn ignores_devices_without_a_trailing_digit() {
        let text = "/dev/srbroken /run/media/x udf ro 0 0\n";
        assert!(parse_proc_mounts(text).is_empty());
    }
}
