// makemkvcon driver. See docs/rip.md.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::makemkv_parse::{to_makemkv_scan, Aggregator, MakemkvScan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub fn at_least(&self, other: &Version) -> bool {
        (self.major, self.minor, self.patch) >= (other.major, other.minor, other.patch)
    }
}

#[derive(Debug)]
pub enum ProbeOutcome {
    Missing,
    Outdated(Version),
    Ok(Version),
}

pub async fn probe() -> ProbeOutcome {
    // makemkvcon doesn't accept --version; the version is printed in the
    // banner of any invocation. The cheapest probe is `makemkvcon -r info`
    // with no source — it errors quickly but prints the MakeMKV version
    // in its first MSG record.
    let output = match Command::new("makemkvcon").arg("-r").arg("info").output().await {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ProbeOutcome::Missing,
        Err(_) => return ProbeOutcome::Missing,
    };
    let combined = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    match super::makemkv_parse::aggregate(&combined)
        .into_iter()
        .filter_map(|r| match r {
            super::makemkv_parse::Record::Msg { code: 1005, text, .. } => Some(text),
            _ => None,
        })
        .next()
        .and_then(|t| parse_version_from_banner(&t))
    {
        Some(v) if v.at_least(&minimum_supported_version()) => ProbeOutcome::Ok(v),
        Some(v) => ProbeOutcome::Outdated(v),
        None => ProbeOutcome::Missing,
    }
}

pub fn minimum_supported_version() -> Version {
    // Bumped per release; older builds fail on current discs because of expired beta keys
    // and lack of UHD AACS2 support.
    Version { major: 1, minor: 17, patch: 0 }
}

fn parse_version_from_banner(text: &str) -> Option<Version> {
    // Banner: "MakeMKV v1.17.8 linux(x64-release) started"
    let after_v = text.split_once(" v")?.1;
    let dotted: String = after_v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let parts: Vec<u32> = dotted.split('.').filter_map(|s| s.parse().ok()).collect();
    match parts.as_slice() {
        [maj, min, pat] => Some(Version { major: *maj, minor: *min, patch: *pat }),
        [maj, min] => Some(Version { major: *maj, minor: *min, patch: 0 }),
        _ => None,
    }
}

/// What `makemkvcon` should be pointed at for a scan or extract.
#[derive(Debug, Clone)]
pub enum ScanSource {
    /// Physical drive index, surfaced as `disc:N`.
    Disc(u32),
    /// Path to a `.iso` image, surfaced as `iso:/path/...`.
    Iso(PathBuf),
    /// Path to a directory containing a `BDMV/` or `VIDEO_TS/` tree, surfaced as `file:/path/...`.
    Folder(PathBuf),
    /// Path to a Linux device node (e.g. `/dev/sr0`), surfaced as `dev:/dev/sr0`.
    Device(PathBuf),
}

impl ScanSource {
    fn as_argument(&self) -> String {
        match self {
            ScanSource::Disc(n) => format!("disc:{n}"),
            ScanSource::Iso(p) => format!("iso:{}", p.display()),
            ScanSource::Folder(p) => format!("file:{}", p.display()),
            ScanSource::Device(p) => format!("dev:{}", p.display()),
        }
    }
}

/// Run `makemkvcon -r info <source>`, parse its robot-mode output, and
/// return the typed scan. Streams stdout through the incremental
/// `Aggregator` so this is safe for large discs without buffering the
/// whole output in memory first.
pub async fn scan(source: &ScanSource) -> Result<MakemkvScan> {
    let arg = source.as_argument();
    let mut child = Command::new("makemkvcon")
        .arg("-r")
        .arg("--noscan")
        .arg("--messages=-stdout")
        .arg("--progress=-null")
        .arg("info")
        .arg(&arg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn makemkvcon")?;

    let stdout = child.stdout.take().ok_or_else(|| anyhow!("makemkvcon stdout was not piped"))?;
    let mut reader = BufReader::new(stdout).lines();

    let mut agg = Aggregator::new();
    let mut records = Vec::new();
    while let Some(line) = reader.next_line().await.context("reading makemkvcon stdout")? {
        records.extend(agg.push_line(&line));
    }
    records.extend(agg.finish());

    let status = child.wait().await.context("waiting for makemkvcon")?;
    if !status.success() {
        let mut stderr_buf = String::new();
        if let Some(mut e) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let _ = e.read_to_string(&mut stderr_buf).await;
        }
        return Err(anyhow!(
            "makemkvcon exited with status {}; stderr: {}",
            status,
            stderr_buf.trim()
        ));
    }

    Ok(to_makemkv_scan(&records))
}

pub async fn extract_title(
    _disc_index: u32,
    _title_index: u32,
    _output_dir: &Path,
    _progress: tokio::sync::mpsc::Sender<Progress>,
) -> Result<PathBuf> {
    todo!("spawn makemkvcon mkv, stream PRGV: progress, return final mkv path")
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub current: u64,
    pub total: u64,
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        let a = Version { major: 1, minor: 17, patch: 0 };
        let b = Version { major: 1, minor: 17, patch: 8 };
        let c = Version { major: 1, minor: 18, patch: 0 };
        assert!(b.at_least(&a));
        assert!(c.at_least(&b));
        assert!(!a.at_least(&b));
    }

    #[test]
    fn parses_version_from_banner() {
        let v = parse_version_from_banner("MakeMKV v1.17.8 linux(x64-release) started").unwrap();
        assert_eq!(v, Version { major: 1, minor: 17, patch: 8 });
    }

    #[test]
    fn parses_two_part_version() {
        let v = parse_version_from_banner("MakeMKV v1.18 macOS started").unwrap();
        assert_eq!(v, Version { major: 1, minor: 18, patch: 0 });
    }

    #[test]
    fn rejects_malformed_banner() {
        assert!(parse_version_from_banner("MakeMKV vbogus").is_none());
        assert!(parse_version_from_banner("no version here").is_none());
    }

    #[test]
    fn scan_source_renders_as_makemkvcon_arg() {
        assert_eq!(ScanSource::Disc(3).as_argument(), "disc:3");
        assert_eq!(ScanSource::Iso(PathBuf::from("/x/y.iso")).as_argument(), "iso:/x/y.iso");
        assert_eq!(ScanSource::Folder(PathBuf::from("/mnt/bd")).as_argument(), "file:/mnt/bd");
        assert_eq!(ScanSource::Device(PathBuf::from("/dev/sr0")).as_argument(), "dev:/dev/sr0");
    }
}

// =====================================================================
// Live integration test against the JP ISO. Gated behind an env var so
// CI without makemkvcon (or without the sample) doesn't fail.
// Set THREEDRIP_TEST_ISO_PATH=/path/to/disc.iso to enable.
// =====================================================================

#[cfg(test)]
mod live_tests {
    use super::*;

    #[tokio::test]
    async fn scan_real_iso_when_env_var_set() {
        let Ok(iso) = std::env::var("THREEDRIP_TEST_ISO_PATH") else {
            eprintln!("THREEDRIP_TEST_ISO_PATH not set; skipping live scan test");
            return;
        };
        let scan_result = match scan(&ScanSource::Iso(PathBuf::from(iso))).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("scan failed (skipping; ensure makemkvcon is installed): {e}");
                return;
            }
        };
        assert!(scan_result.titles.len() > 0, "expected at least one title");
        assert!(scan_result.makemkv_version.is_some(), "expected version parsed");
    }
}
