// makemkvcon driver. See docs/rip.md.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};

use super::makemkv_parse::{to_makemkv_scan, Aggregator, MakemkvScan, MsgRecord, Record};

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
    let output = match crate::hostcmd::host_command("makemkvcon").arg("-r").arg("info").output().await {
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

/// Below this version, MakeMKV silently drops the MVC dependent-view
/// track on 3D Blu-rays even when our keep-mvc profile says to keep
/// it -- the binary just doesn't write mvcC BlockAdditions or
/// stereo_mode flags. Diagnosed against Jurassic Park 3D on Linux
/// v1.17.8 (drops MVC) vs Windows v1.18.2 (writes mvcC) using the
/// same `+sel:mvcvideo` selector. v1.18 is the first build where the
/// CLI rip produces a 3D-capable output on the discs we have.
pub fn minimum_mvc_capable_version() -> Version {
    Version { major: 1, minor: 18, patch: 0 }
}

impl Version {
    pub fn supports_mvc(&self) -> bool {
        self.at_least(&minimum_mvc_capable_version())
    }
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
    let mut child = crate::hostcmd::host_command("makemkvcon")
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

/// Current progress state aggregated from PRGT (overall label) /
/// PRGC (sub-operation label) / PRGV (numeric scale) records.
///
/// `current` / `total` are out of `max` (typically 65536).
#[derive(Debug, Clone, Default)]
pub struct ExtractProgress {
    pub current: u32,
    pub total: u32,
    pub max: u32,
    pub current_label: Option<String>,
    pub total_label: Option<String>,
}

impl ExtractProgress {
    pub fn current_fraction(&self) -> f32 {
        if self.max == 0 { 0.0 } else { self.current as f32 / self.max as f32 }
    }

    pub fn total_fraction(&self) -> f32 {
        if self.max == 0 { 0.0 } else { self.total as f32 / self.max as f32 }
    }
}

/// Live event from a running extraction. Sent on the channel the caller
/// passes to `extract_title`; the caller (typically the UI) reads them
/// and updates its progress bar / log expander.
#[derive(Debug, Clone)]
pub enum ExtractEvent {
    Progress(ExtractProgress),
    Message(MsgRecord),
}

/// Apply a single `Record` to a running `ExtractProgress`, returning an
/// `ExtractEvent` to emit if one is warranted. Pure function — exposed
/// here so its behaviour can be unit-tested without spawning subprocesses.
pub fn apply_record(state: &mut ExtractProgress, record: &Record) -> Option<ExtractEvent> {
    match record {
        Record::Prgt { name, .. } => {
            state.total_label = Some(name.clone());
            None
        }
        Record::Prgc { name, .. } => {
            state.current_label = Some(name.clone());
            None
        }
        Record::Prgv { current, total, max } => {
            state.current = *current;
            state.total = *total;
            state.max = *max;
            Some(ExtractEvent::Progress(state.clone()))
        }
        Record::Msg { code, priority, text, .. } => Some(ExtractEvent::Message(MsgRecord {
            code: *code,
            priority: *priority,
            text: text.clone(),
        })),
        _ => None,
    }
}

/// Run `makemkvcon -r mkv <source> <title_index> <output_dir>` and stream
/// progress events through the optional `event_tx` channel. Returns the
/// path of the produced `.mkv` file on success.
///
/// `expected_output_filename` should match the title's MakeMKV-assigned
/// output filename (CINFO/TINFO code 27 in a prior scan); we use it to
/// resolve the produced file. If MakeMKV's actual output name differs,
/// the function falls back to scanning `output_dir` for the most recently
/// created `.mkv`.
///
/// The subprocess is set up with `kill_on_drop(true)`, so a cancelled
/// task tears down the running makemkvcon cleanly.
/// MakeMKV's compiled-in default selector ends with `-sel:mvcvideo`,
/// which silently drops the dependent-view track on a 3D Blu-ray. The
/// MKV that comes out then has only the base view -- 1920x1080
/// flat -- and the downstream 3D convert pipeline has nothing to pack
/// into FSBS. We ship our own profile that flips that one selector to
/// `+sel:mvcvideo`; everything else mirrors MakeMKV's default. The
/// XML is embedded so the dev-from-cargo build doesn't depend on
/// $datadir being set up.
const KEEP_MVC_PROFILE_XML: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/makemkv/keep-mvc.mmcp.xml"));

/// A keep-MVC profile file on disk that removes itself when dropped.
///
/// It must be written somewhere `makemkvcon` can actually read it. In a Flatpak
/// sandbox `makemkvcon` runs on the *host* (via `flatpak-spawn --host`) and
/// cannot see our private `/tmp`, so a `tempfile::TempDir` path would be handed
/// over as `--profile=` and silently fail — makemkvcon then falls back to its
/// built-in default, which DROPS the MVC dependent view (flat 2D output). We
/// instead drop it into `output_dir`, which is granted to us AND visible to the
/// host at the identical real path (xdg-videos et al. map through unchanged).
struct KeepMvcProfile {
    path: PathBuf,
}

impl Drop for KeepMvcProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Write our keep-MVC profile into `output_dir` and return a guard that removes
/// it on drop. The caller keeps the guard alive for the duration of the
/// makemkvcon invocation.
fn write_keep_mvc_profile(output_dir: &Path) -> Result<KeepMvcProfile> {
    let path = output_dir.join(".ripsaw-keep-mvc.mmcp.xml");
    std::fs::write(&path, KEEP_MVC_PROFILE_XML)
        .with_context(|| format!("writing MakeMKV keep-MVC profile to {}", path.display()))?;
    Ok(KeepMvcProfile { path })
}

pub async fn extract_title(
    source: &ScanSource,
    title_index: u32,
    output_dir: &Path,
    expected_output_filename: &str,
    event_tx: Option<tokio::sync::mpsc::Sender<ExtractEvent>>,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(output_dir)
        .await
        .with_context(|| format!("ensuring output dir {} exists", output_dir.display()))?;

    // Hold the guard for the lifetime of this fn so the profile XML file we
    // hand makemkvcon stays on disk until the rip finishes (and is removed
    // afterwards). It lives in output_dir so host makemkvcon can read it.
    let profile = write_keep_mvc_profile(output_dir)?;

    let arg = source.as_argument();
    let mut child = crate::hostcmd::host_command("makemkvcon")
        .arg("-r")
        .arg("--noscan")
        .arg(format!("--profile={}", profile.path.display()))
        .arg("--messages=-stdout")
        .arg("--progress=-stdout")
        .arg("mkv")
        .arg(&arg)
        .arg(title_index.to_string())
        .arg(output_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn makemkvcon mkv")?;

    let stdout = child.stdout.take().ok_or_else(|| anyhow!("makemkvcon stdout was not piped"))?;
    let mut reader = BufReader::new(stdout).lines();

    let mut agg = Aggregator::new();
    let mut state = ExtractProgress::default();
    // makemkvcon writes its real error reason as MSG records on stdout
    // (we asked for `--messages=-stdout`), so stderr is almost always
    // empty on failure. Collect MSG text here so we can surface it in
    // the error path.
    let mut messages: Vec<String> = Vec::new();

    while let Some(line) = reader.next_line().await.context("reading makemkvcon stdout")? {
        for rec in agg.push_line(&line) {
            if let Record::Msg { text, .. } = &rec {
                messages.push(text.clone());
            }
            if let Some(event) = apply_record(&mut state, &rec) {
                if let Some(tx) = &event_tx {
                    // Drop events when the consumer is full rather than
                    // blocking parsing. Progress is coalescing-safe.
                    let _ = tx.try_send(event);
                }
            }
        }
    }
    for rec in agg.finish() {
        if let Record::Msg { text, .. } = &rec {
            messages.push(text.clone());
        }
        if let Some(event) = apply_record(&mut state, &rec) {
            if let Some(tx) = &event_tx {
                let _ = tx.try_send(event);
            }
        }
    }

    let status = child.wait().await.context("waiting for makemkvcon")?;
    if !status.success() {
        let mut stderr_buf = String::new();
        if let Some(mut e) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let _ = e.read_to_string(&mut stderr_buf).await;
        }
        return Err(anyhow!(
            "makemkvcon mkv exited with status {}; stderr: {}; messages: {}",
            status,
            stderr_buf.trim(),
            if messages.is_empty() { "(none)".into() } else { messages.join(" | ") },
        ));
    }

    resolve_output_file(output_dir, expected_output_filename).await
}

async fn resolve_output_file(dir: &Path, expected: &str) -> Result<PathBuf> {
    let primary = dir.join(expected);
    if tokio::fs::try_exists(&primary).await.unwrap_or(false) {
        return Ok(primary);
    }
    // Fall-back: pick the newest *.mkv in the output directory.
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut entries = tokio::fs::read_dir(dir).await.context("reading output dir")?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("mkv") {
            continue;
        }
        let modified = entry.metadata().await?.modified()?;
        if latest.as_ref().is_none_or(|(t, _)| modified > *t) {
            latest = Some((modified, path));
        }
    }
    latest.map(|(_, p)| p).ok_or_else(|| {
        anyhow!(
            "makemkvcon exited successfully but no .mkv file was produced in {}",
            dir.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_mvc_profile_is_well_formed_and_keeps_mvc() {
        // Regression guard. An earlier revision of this profile
        // referenced an outputSettings ("copy") it never defined and put
        // the selection string in a nested `<default selected=..>`
        // element instead of the `defaultSelection` attribute. MakeMKV
        // rejected it ("Profile parsing error: output config invalid")
        // and silently fell back to its built-in default -- which DROPS
        // the MVC dependent-view track, so every 3D rip came out flat 2D.
        let xml = KEEP_MVC_PROFILE_XML;

        // 1. It must be valid XML.
        let mut reader = quick_xml::Reader::from_str(xml);
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("keep-mvc profile is not valid XML: {e}"),
            }
        }

        // 2. The active track-selection strings must retain MVC, not drop
        //    it. Inspect the `defaultSelection` attribute values only --
        //    the surrounding comment documents the stock `-sel:mvcvideo`
        //    for contrast, so a naive whole-file substring check would
        //    give a false positive.
        let selections: Vec<&str> = xml
            .match_indices("defaultSelection=\"")
            .map(|(i, m)| {
                let start = i + m.len();
                let end = xml[start..].find('"').map(|e| start + e).unwrap_or(xml.len());
                &xml[start..end]
            })
            .collect();
        assert!(!selections.is_empty(), "profile defines no track selection");
        assert!(
            selections.iter().any(|s| s.contains("+sel:mvcvideo")),
            "the default track selection must keep the MVC track"
        );
        assert!(
            selections.iter().all(|s| !s.contains("-sel:mvcvideo")),
            "no track selection may drop the MVC track"
        );

        // 3. The outputSettings referenced by trackSettings must be
        //    defined in-file, and the selection must live in the
        //    `defaultSelection` attribute -- the two things whose absence
        //    made MakeMKV reject the old profile.
        assert!(
            xml.contains(r#"<outputSettings name="copy""#),
            "the \"copy\" outputSettings must be defined or MakeMKV errors"
        );
        assert!(xml.contains(r#"outputSettingsName="copy""#));
        assert!(xml.contains("defaultSelection="));
    }

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

    #[test]
    fn extract_progress_fractions_are_safe_when_max_is_zero() {
        let p = ExtractProgress::default();
        assert_eq!(p.current_fraction(), 0.0);
        assert_eq!(p.total_fraction(), 0.0);
    }

    #[test]
    fn extract_progress_fractions_scale_to_max() {
        let p = ExtractProgress { current: 16384, total: 32768, max: 65536, ..Default::default() };
        assert!((p.current_fraction() - 0.25).abs() < 1e-6);
        assert!((p.total_fraction() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn apply_record_updates_progress_and_emits_event() {
        let mut state = ExtractProgress::default();

        // PRGT sets the overall label without emitting.
        let r = Record::Prgt { code: 5018, id: 0, name: "Saving title to MKV file".into() };
        assert!(matches!(apply_record(&mut state, &r), None));
        assert_eq!(state.total_label.as_deref(), Some("Saving title to MKV file"));

        // PRGC sets the sub-operation label without emitting.
        let r = Record::Prgc { code: 5017, id: 0, name: "Analyzing seamless segments".into() };
        assert!(matches!(apply_record(&mut state, &r), None));
        assert_eq!(state.current_label.as_deref(), Some("Analyzing seamless segments"));

        // PRGV sets numbers AND emits a Progress event.
        let r = Record::Prgv { current: 100, total: 200, max: 65536 };
        let event = apply_record(&mut state, &r).expect("PRGV must emit");
        match event {
            ExtractEvent::Progress(p) => {
                assert_eq!(p.current, 100);
                assert_eq!(p.total, 200);
                assert_eq!(p.max, 65536);
                assert_eq!(p.current_label.as_deref(), Some("Analyzing seamless segments"));
                assert_eq!(p.total_label.as_deref(), Some("Saving title to MKV file"));
            }
            other => panic!("expected Progress, got {other:?}"),
        }

        // MSG emits a Message event without touching progress numbers.
        let r = Record::Msg { code: 5036, flags: 0, priority: 2, text: "Copy complete.".into() };
        let event = apply_record(&mut state, &r).expect("MSG must emit");
        match event {
            ExtractEvent::Message(m) => {
                assert_eq!(m.code, 5036);
                assert_eq!(m.text, "Copy complete.");
            }
            other => panic!("expected Message, got {other:?}"),
        }
        // PRGV state should be unchanged by the MSG.
        assert_eq!(state.current, 100);
    }

    #[tokio::test]
    async fn resolve_output_file_finds_named_output() {
        let dir = tempfile::tempdir().unwrap();
        let expected = "title_t00.mkv";
        tokio::fs::write(dir.path().join(expected), b"fake").await.unwrap();
        let resolved = resolve_output_file(dir.path(), expected).await.unwrap();
        assert_eq!(resolved.file_name().unwrap().to_str().unwrap(), expected);
    }

    #[tokio::test]
    async fn resolve_output_file_falls_back_to_newest_mkv() {
        let dir = tempfile::tempdir().unwrap();
        // We expected something MakeMKV chose not to write; instead, find the newest .mkv.
        tokio::fs::write(dir.path().join("older.mkv"), b"a").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::fs::write(dir.path().join("newer.mkv"), b"b").await.unwrap();
        // Plus an unrelated file to be skipped:
        tokio::fs::write(dir.path().join("notes.txt"), b"x").await.unwrap();
        let resolved = resolve_output_file(dir.path(), "missing.mkv").await.unwrap();
        assert_eq!(resolved.file_name().unwrap().to_str().unwrap(), "newer.mkv");
    }

    #[tokio::test]
    async fn resolve_output_file_errors_when_no_mkv_exists() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("notes.txt"), b"x").await.unwrap();
        let err = resolve_output_file(dir.path(), "expected.mkv").await.unwrap_err();
        assert!(format!("{err}").contains("no .mkv file was produced"));
    }
}

// =====================================================================
// Live integration test against the JP ISO. Gated behind an env var so
// CI without makemkvcon (or without the sample) doesn't fail.
// Set RIPSAW_TEST_ISO_PATH=/path/to/disc.iso to enable.
// =====================================================================

#[cfg(test)]
mod live_tests {
    use super::*;

    #[tokio::test]
    async fn scan_real_iso_when_env_var_set() {
        let Ok(iso) = std::env::var("RIPSAW_TEST_ISO_PATH") else {
            eprintln!("RIPSAW_TEST_ISO_PATH not set; skipping live scan test");
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_smallest_title_when_env_var_set() {
        let Ok(iso) = std::env::var("RIPSAW_TEST_ISO_PATH") else {
            eprintln!("RIPSAW_TEST_ISO_PATH not set; skipping live extract test");
            return;
        };
        let iso_path = PathBuf::from(iso);
        let scan_result = match scan(&ScanSource::Iso(iso_path.clone())).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("preliminary scan failed: {e}");
                return;
            }
        };
        let smallest = scan_result
            .titles
            .iter()
            .filter(|t| t.size_bytes.is_some())
            .min_by_key(|t| t.size_bytes.unwrap())
            .expect("at least one title with a size");
        let expected_filename = smallest
            .output_file
            .clone()
            .unwrap_or_else(|| format!("title_t{:02}.mkv", smallest.index));

        eprintln!(
            "extracting title {} (dur={}s, size={} bytes) -> {}",
            smallest.index,
            smallest.duration_seconds.unwrap_or(0),
            smallest.size_bytes.unwrap_or(0),
            expected_filename,
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ExtractEvent>(64);

        let extract_task = {
            let iso_path = iso_path.clone();
            let out_path = dir.path().to_path_buf();
            let title_index = smallest.index;
            let filename = expected_filename.clone();
            tokio::spawn(async move {
                extract_title(
                    &ScanSource::Iso(iso_path),
                    title_index,
                    &out_path,
                    &filename,
                    Some(tx),
                )
                .await
            })
        };

        let mut last_logged = 0.0f32;
        while let Some(event) = rx.recv().await {
            if let ExtractEvent::Progress(p) = &event {
                let frac = p.total_fraction();
                if frac - last_logged > 0.10 {
                    eprintln!(
                        "  total {:.0}%  current {:.0}%  ({})",
                        frac * 100.0,
                        p.current_fraction() * 100.0,
                        p.current_label.as_deref().unwrap_or(""),
                    );
                    last_logged = frac;
                }
            }
        }

        let produced = extract_task
            .await
            .expect("extract task join")
            .expect("extract result");
        let meta = tokio::fs::metadata(&produced).await.expect("metadata");
        eprintln!("extracted -> {} ({} bytes)", produced.display(), meta.len());
        assert!(meta.len() > 1_000_000, "MKV must be larger than 1 MB, got {}", meta.len());
    }
}
