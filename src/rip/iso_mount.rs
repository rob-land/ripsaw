// Loopback-mount an ISO via `udisksctl` so the filesystem can be walked
// for content-hash computation. Shell-based for now; a future revision
// could go straight to UDisks2 over D-Bus via the `zbus` crate.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::process::Command;

#[derive(Debug)]
pub struct MountedIso {
    pub iso_path: PathBuf,
    pub loop_device: PathBuf,
    pub mount_point: PathBuf,
}

impl MountedIso {
    /// Loop-set-up and mount the ISO. The returned guard does NOT
    /// auto-unmount on drop — call `unmount` explicitly when finished.
    pub async fn mount(iso: &Path) -> Result<Self> {
        let setup_out = Command::new("udisksctl")
            .args(["loop-setup", "-r", "-f"])
            .arg(iso)
            .output()
            .await
            .context("spawn udisksctl loop-setup")?;
        if !setup_out.status.success() {
            return Err(anyhow!(
                "udisksctl loop-setup failed: {}",
                String::from_utf8_lossy(&setup_out.stderr).trim(),
            ));
        }
        let setup_text = String::from_utf8_lossy(&setup_out.stdout);
        let loop_device = parse_loop_device(&setup_text).ok_or_else(|| {
            anyhow!(
                "could not parse loop device from udisksctl output: {}",
                setup_text.trim()
            )
        })?;

        // udisks2 sometimes auto-mounts the loop device. `mount` then either
        // succeeds with the new mount point, or fails saying "already mounted
        // at <path>". We grep the mount point out of either case.
        let mount_out = Command::new("udisksctl")
            .args(["mount", "-b"])
            .arg(&loop_device)
            .output()
            .await
            .context("spawn udisksctl mount")?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&mount_out.stdout),
            String::from_utf8_lossy(&mount_out.stderr),
        );
        let mount_point = parse_mount_point(&combined).ok_or_else(|| {
            anyhow!("could not parse mount point from udisksctl output: {}", combined.trim())
        })?;

        Ok(Self {
            iso_path: iso.to_path_buf(),
            loop_device,
            mount_point,
        })
    }

    /// Best-effort unmount. Tolerates "not mounted" / "no such device" since
    /// udisks2's bookkeeping may already have cleaned up.
    pub async fn unmount(&self) -> Result<()> {
        let _ = Command::new("udisksctl")
            .args(["unmount", "-b"])
            .arg(&self.loop_device)
            .output()
            .await;
        let _ = Command::new("udisksctl")
            .args(["loop-delete", "-b"])
            .arg(&self.loop_device)
            .output()
            .await;
        Ok(())
    }
}

static LOOP_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(/dev/loop\d+)").expect("static regex"));
static MOUNT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:at|on)\s+[`']?(/[^`'\s]+)").expect("static regex"));

pub fn parse_loop_device(text: &str) -> Option<PathBuf> {
    LOOP_RE.captures(text).map(|c| PathBuf::from(&c[1]))
}

pub fn parse_mount_point(text: &str) -> Option<PathBuf> {
    MOUNT_RE.captures(text).map(|c| PathBuf::from(c[1].trim_end_matches(['.', '`', '\''])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loop_device_from_setup_output() {
        let out = "Mapped file /home/rob/foo.iso as /dev/loop3.";
        assert_eq!(parse_loop_device(out), Some(PathBuf::from("/dev/loop3")));
    }

    #[test]
    fn parses_mount_point_from_success_output() {
        let out = "Mounted /dev/loop3 at /run/media/rob/MY_DISC";
        assert_eq!(parse_mount_point(out), Some(PathBuf::from("/run/media/rob/MY_DISC")));
    }

    #[test]
    fn parses_mount_point_from_already_mounted_error() {
        let out = "Error mounting /dev/loop3: GDBus.Error:org.freedesktop.UDisks2.Error.AlreadyMounted: Device /dev/loop3 is already mounted at `/run/media/rob/BD201502010526'.";
        assert_eq!(
            parse_mount_point(out),
            Some(PathBuf::from("/run/media/rob/BD201502010526"))
        );
    }

    #[test]
    fn returns_none_when_no_match() {
        assert_eq!(parse_loop_device("nothing relevant here"), None);
        assert_eq!(parse_mount_point("nothing relevant here"), None);
    }
}
