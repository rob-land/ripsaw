// End-to-end "identify a disc" orchestration.
//
// Inputs: a path (ISO file today; mounted folder / device later).
// Output: an IdentificationResult that ties together
//   - the makemkvcon scan (titles, streams, MakeMKV version)
//   - the inferred disc type (DVD / BD / UHD / BD-3D)
//   - the content hash (when computable — needs a mount)
//   - any TheDiscDB Identity matches (Vec because hash collisions
//     across regional pressings are documented and supported)
//
// Composition: this is mostly glue. The hash, scan, mount, and lookup
// each have their own modules and tests.

use std::fs::File;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::identify::{
    disc_hash::{content_hash, enumerate_disc_files},
    ffprobe,
    from_scan::{detect_disc_type, detect_disc_type_with_mount},
    thediscdb::TheDiscDbClient,
    DiscType, Identity,
};
use libmvc::ebml::EbmlReader;
use libmvc::mvcc::scan_3d_info;
use crate::rip::{
    iso_mount::MountedIso,
    makemkv::{scan, ScanSource},
    makemkv_parse::{
        DiscAttributes, MakemkvScan, StreamAttributes, TitleAttributes,
    },
};

pub struct IdentificationResult {
    pub scan: MakemkvScan,
    pub mount: Option<MountedIso>,
    pub disc_type: DiscType,
    pub content_hash: Option<String>,
    pub identities: Vec<Identity>,
    /// Outcome of the TheDiscDB lookup. Distinguishes a *service error*
    /// (endpoint down, network failure) from a genuine *no match*, so the
    /// UI doesn't tell the user "not in catalog" when the lookup actually
    /// failed. TheDiscDB's hosted endpoint has been unreliable (observed
    /// 403 "Web App - Unavailable"), which previously surfaced identically
    /// to "disc not catalogued".
    pub lookup_status: LookupStatus,
    /// What `makemkvcon` should be pointed at to extract titles from
    /// this source. `Disc(N)` for physical drives, `Iso(path)` for ISO
    /// images and (vacuously) for already-extracted MKVs where the
    /// rip-button path is never taken.
    pub source: ScanSource,
    /// For non-disc sources (MKVs already on disk), the original file
    /// path so downstream actions (conversion, transcode) know where to
    /// find the input bytes.
    pub source_file: Option<PathBuf>,
    /// True when MakeMKV-style mvcC was detected in the source — i.e.
    /// the file or disc carries an MVC dependent view track.
    pub has_mvc: bool,
    /// `BDMV/META/DL/bdmt_eng.xml` content when present. Used as
    /// pre-fill data on the title list page when TheDiscDB has no
    /// match for the disc.
    pub bdmt: Option<crate::identify::bdmt::BdmtMetadata>,
    /// TheDiscDB-style region code derived from the DVD VMG when the
    /// source is a DVD-Video disc. `"0"` for region-free, `"1"`..`"8"`
    /// for region-locked discs. `None` for Blu-ray / UHD (region info
    /// lives in AACS, which is stripped on rip) and when the disc isn't
    /// mounted.
    pub dvd_region_code: Option<String>,
}

/// Result of attempting a TheDiscDB content-hash lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LookupStatus {
    /// No lookup was attempted (disc not mounted / no content hash).
    #[default]
    NotAttempted,
    /// Lookup completed. An empty `identities` here means the disc is
    /// genuinely not in the catalogue.
    Ok,
    /// Lookup failed (service down, network error, bad response). The
    /// disc may well be catalogued — we just couldn't reach the data.
    Failed(String),
}

impl IdentificationResult {
    /// `true` when at least one TheDiscDB match was returned.
    pub fn is_identified(&self) -> bool {
        !self.identities.is_empty()
    }
}

/// Run a TheDiscDB lookup for the given optional content hash, capturing
/// whether it succeeded, returned nothing, or errored. Tries the local
/// mirror first (offline, outage-proof) and falls back to the live
/// GraphQL endpoint. Centralises the "service error vs no match"
/// distinction both disc paths need.
async fn lookup_with_status(hash: Option<&str>) -> (Vec<Identity>, LookupStatus) {
    let Some(h) = hash else {
        return (Vec::new(), LookupStatus::NotAttempted);
    };

    // 1. Local mirror first. A hit is authoritative (offline + instant);
    //    a miss falls through to live in case the mirror is stale.
    let mirror = crate::settings::thediscdb_mirror_root();
    if mirror.join("data").is_dir() {
        let hh = h.to_string();
        let m = mirror.clone();
        match tokio::task::spawn_blocking(move || {
            crate::identify::thediscdb_local::LocalDiscDb::new(m).lookup_by_hash(&hh)
        })
        .await
        {
            Ok(Ok(ids)) if !ids.is_empty() => {
                tracing::info!("identified via local TheDiscDB mirror");
                return (ids, LookupStatus::Ok);
            }
            Ok(Ok(_)) => tracing::debug!("local TheDiscDB mirror: no match, trying live"),
            Ok(Err(e)) => tracing::warn!("local TheDiscDB lookup error: {e:#}"),
            Err(e) => tracing::warn!("local TheDiscDB task panicked: {e}"),
        }
    }

    // 2. Live GraphQL fallback.
    let client = match TheDiscDbClient::with_default_endpoint() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("TheDiscDB client init failed: {e:#}");
            return (Vec::new(), LookupStatus::Failed(format!("{e:#}")));
        }
    };
    match client.lookup_by_hash(h).await {
        Ok(ids) => (ids, LookupStatus::Ok),
        Err(e) => {
            tracing::warn!("TheDiscDB lookup failed for hash {h}: {e:#}");
            (Vec::new(), LookupStatus::Failed(format!("{e:#}")))
        }
    }
}

/// Drive a physical optical disc end-to-end: scan via `disc:N` + walk an
/// already-mounted path for hashing + TheDiscDB lookup. The caller passes
/// the mount path that udisks2 (or the desktop's auto-mount) has placed
/// the disc at; we never set it up or tear it down ourselves — the
/// desktop owns the mount lifecycle for inserted physical media.
pub async fn identify_physical_disc(
    disc_index: u32,
    mount_path: PathBuf,
) -> Result<IdentificationResult> {
    let source = ScanSource::Disc(disc_index);
    let scan_data = scan(&source).await.context("running makemkvcon scan")?;

    let hash = enumerate_disc_files(&mount_path)
        .map(|files| content_hash(&files))
        .ok();
    let (identities, lookup_status) = lookup_with_status(hash.as_deref()).await;
    let disc_type = detect_disc_type_with_mount(&scan_data, &mount_path);
    let has_mvc = scan_has_mvc(&scan_data);
    let bdmt = crate::identify::bdmt::read_from_mount(&mount_path)
        .ok()
        .flatten();
    let dvd_region_code = crate::identify::dvd::read_region_code(&mount_path)
        .ok()
        .flatten()
        .map(|s| s.to_string());

    Ok(IdentificationResult {
        scan: scan_data,
        mount: None,
        disc_type,
        content_hash: hash,
        identities,
        lookup_status,
        source,
        source_file: None,
        has_mvc,
        bdmt,
        dvd_region_code,
    })
}

/// True when any title on the disc carries a stream MakeMKV identifies
/// as MVC. The two codec strings we care about are SINFO code 6
/// (`Mpeg4-MVC-3D`) and SINFO code 7 (`Mpeg4 MVC High@L4.1/...`); a
/// match on either is sufficient.
fn scan_has_mvc(scan: &MakemkvScan) -> bool {
    scan.titles.iter().any(|t| {
        t.streams.iter().any(|s| {
            let short = s.codec_short.as_deref().unwrap_or("");
            let long = s.codec_long.as_deref().unwrap_or("");
            short.to_ascii_uppercase().contains("MVC")
                || long.to_ascii_uppercase().contains("MVC")
        })
    })
}

/// Open an existing MKV file (typically a MakeMKV-extracted 3D rip) and
/// produce an `IdentificationResult` suitable for the UI: a synthetic
/// scan with one title representing the file, `has_mvc = true` when the
/// MKV carries an mvcC BlockAdditionMapping. No disc scan is performed
/// because the file is already extracted.
pub async fn identify_mkv(mkv_path: PathBuf) -> Result<IdentificationResult> {
    let report = ffprobe::probe(&mkv_path)
        .await
        .with_context(|| format!("probing {}", mkv_path.display()))?;

    let (has_mvc, stereo_mode) = detect_3d(&mkv_path);
    let scan_data = synthesise_scan(&mkv_path, &report, has_mvc);
    let _ = stereo_mode; // surfaced through has_mvc; detailed mode UX is a TODO
    let _has_mvcc = has_mvc; // used below to discriminate the StereoSource variant
    let disc_type = if has_mvc { DiscType::BluRay3D } else { DiscType::BluRay };

    Ok(IdentificationResult {
        scan: scan_data,
        mount: None,
        disc_type,
        content_hash: None,
        identities: Vec::new(),
        lookup_status: LookupStatus::NotAttempted,
        source: ScanSource::Iso(mkv_path.clone()),
        source_file: Some(mkv_path),
        has_mvc,
        bdmt: None,
        dvd_region_code: None,
    })
}

/// Returns (has_mvc, stereo_mode). `has_mvc` is true when either the
/// mvcC BlockAddition is present or the Matroska StereoMode element
/// indicates MVC-style "both eyes laced" packing (modes 13, 14).
fn detect_3d(mkv_path: &std::path::Path) -> (bool, Option<u64>) {
    let Ok(file) = File::open(mkv_path) else {
        return (false, None);
    };
    let mut reader = EbmlReader::new(file);
    match scan_3d_info(&mut reader) {
        Ok(info) => (info.has_mvc(), info.stereo_mode),
        Err(_) => (false, None),
    }
}

fn synthesise_scan(
    mkv_path: &std::path::Path,
    report: &ffprobe::FfprobeReport,
    has_mvc: bool,
) -> MakemkvScan {
    let name = mkv_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned());
    let display_name = name.clone().unwrap_or_else(|| "MKV file".to_string());

    let mut streams: Vec<StreamAttributes> =
        report.streams.iter().map(stream_attributes_from).collect();
    if has_mvc {
        // ffprobe sees the MVC dependent view as mvcC BlockAdditions on
        // the base track, not as a separate stream, so nothing above is
        // tagged "MVC". Append a synthetic marker so per-title 3D
        // detection (`TitleAttributes::has_mvc_stream`) treats this MKV
        // title the same as a makemkvcon disc scan's 3D title — which is
        // what drives the "3D" badge + per-title format dropdown.
        streams.push(StreamAttributes {
            stream: streams.len() as u32,
            kind: Some("Video".to_string()),
            codec_short: Some("Mpeg4-MVC-3D".to_string()),
            codec_long: Some("Mpeg4 MVC".to_string()),
            ..Default::default()
        });
    }

    let title = TitleAttributes {
        index: 0,
        name: Some(display_name.clone()),
        chapter_count: Some(report.chapters.len() as u32),
        duration_seconds: report.duration_seconds(),
        duration_text: report
            .duration_seconds()
            .map(|d| format_duration(d)),
        display_size: report.size_bytes().map(|b| format_size(b)),
        size_bytes: report.size_bytes(),
        source_file: mkv_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned()),
        segment_count: Some(1),
        segment_map: Some("1".to_string()),
        output_file: Some(format!("{display_name}.mkv")),
        language_code: None,
        streams,
    };

    MakemkvScan {
        disc: DiscAttributes {
            content_type: Some("MKV file".into()),
            name: Some(display_name),
            language_code: None,
            language_name: None,
            volume_label: name,
        },
        titles: vec![title],
        makemkv_version: None,
        messages: Vec::new(),
    }
}

fn stream_attributes_from(s: &ffprobe::FfprobeStream) -> StreamAttributes {
    let kind = match s.codec_type.as_str() {
        "video" => Some("Video".to_string()),
        "audio" => Some("Audio".to_string()),
        "subtitle" => Some("Subtitles".to_string()),
        other => Some(other.to_string()),
    };
    let video_size = match (s.width, s.height) {
        (Some(w), Some(h)) => Some(format!("{w}x{h}")),
        _ => None,
    };
    let language_code = s
        .tags
        .as_ref()
        .and_then(|t| t.get("language"))
        .cloned();
    StreamAttributes {
        stream: s.index,
        kind,
        name: s.tags.as_ref().and_then(|t| t.get("title")).cloned(),
        language_code,
        language_name: None,
        codec_id: s.codec_name.clone(),
        codec_short: s.codec_name.clone(),
        codec_long: s.codec_long_name.clone(),
        bitrate: None,
        channels: s.channels,
        sample_rate: s
            .sample_rate
            .as_ref()
            .and_then(|s| s.parse::<u32>().ok()),
        sample_size: None,
        video_size,
        aspect_ratio: None,
        frame_rate: s.r_frame_rate.clone(),
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h}:{m:02}:{s:02}")
}

fn format_size(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    }
}

/// Drive an ISO end-to-end: scan + mount + hash + TheDiscDB lookup. The
/// mount is the precondition for hashing (we need filesystem access to
/// the disc's payload directory). If mounting fails, the function still
/// returns successfully with `mount: None` and `content_hash: None`; the
/// caller can show partial results.
pub async fn identify_iso(iso_path: PathBuf) -> Result<IdentificationResult> {
    // makemkvcon's built-in `iso:` UDF reader chokes on some images — notably
    // the UDF 2.x that many 3D Blu-rays ship — that the kernel loop-mounts
    // fine. So mount the ISO first (unprivileged, via udisksctl/UDisks2) and
    // point makemkvcon at the mounted directory (`file:`), which is robust and
    // is the same mount content-hashing needs. Fall back to `iso:` only when
    // mounting is unavailable (no udisks2 / headless). The mount is tracked by
    // the caller and stays alive through the later rip, which reuses `source`.
    let mount = MountedIso::mount(&iso_path).await.ok();
    let source = match &mount {
        Some(m) => ScanSource::Folder(m.mount_point.clone()),
        None => ScanSource::Iso(iso_path.clone()),
    };
    let scan_data = scan(&source).await.context("running makemkvcon scan")?;

    let (disc_type, content_hash_value, identities, lookup_status) = match &mount {
        Some(m) => {
            let hash = enumerate_disc_files(&m.mount_point)
                .map(|files| content_hash(&files))
                .ok();
            let (identities, lookup_status) = lookup_with_status(hash.as_deref()).await;
            let disc_type = detect_disc_type_with_mount(&scan_data, &m.mount_point);
            (disc_type, hash, identities, lookup_status)
        }
        None => (
            detect_disc_type(&scan_data),
            None,
            Vec::new(),
            LookupStatus::NotAttempted,
        ),
    };

    let has_mvc = scan_has_mvc(&scan_data);
    let bdmt = mount
        .as_ref()
        .and_then(|m| crate::identify::bdmt::read_from_mount(&m.mount_point).ok().flatten());
    let dvd_region_code = mount
        .as_ref()
        .and_then(|m| crate::identify::dvd::read_region_code(&m.mount_point).ok().flatten())
        .map(|s| s.to_string());
    Ok(IdentificationResult {
        scan: scan_data,
        mount,
        disc_type,
        content_hash: content_hash_value,
        identities,
        lookup_status,
        source,
        source_file: Some(iso_path),
        has_mvc,
        bdmt,
        dvd_region_code,
    })
}
