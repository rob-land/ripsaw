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

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::identify::{
    disc_hash::{content_hash, enumerate_disc_files},
    from_scan::{detect_disc_type, detect_disc_type_with_mount},
    thediscdb::TheDiscDbClient,
    DiscType, Identity,
};
use crate::rip::{
    iso_mount::MountedIso,
    makemkv::{scan, ScanSource},
    makemkv_parse::MakemkvScan,
};

pub struct IdentificationResult {
    pub scan: MakemkvScan,
    pub mount: Option<MountedIso>,
    pub disc_type: DiscType,
    pub content_hash: Option<String>,
    pub identities: Vec<Identity>,
}

impl IdentificationResult {
    /// `true` when at least one TheDiscDB match was returned.
    pub fn is_identified(&self) -> bool {
        !self.identities.is_empty()
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
    let identities = match (&hash, TheDiscDbClient::with_default_endpoint()) {
        (Some(h), Ok(client)) => client.lookup_by_hash(h).await.unwrap_or_default(),
        _ => Vec::new(),
    };
    let disc_type = detect_disc_type_with_mount(&scan_data, &mount_path);

    Ok(IdentificationResult {
        scan: scan_data,
        mount: None,
        disc_type,
        content_hash: hash,
        identities,
    })
}

/// Drive an ISO end-to-end: scan + mount + hash + TheDiscDB lookup. The
/// mount is the precondition for hashing (we need filesystem access to
/// the disc's payload directory). If mounting fails, the function still
/// returns successfully with `mount: None` and `content_hash: None`; the
/// caller can show partial results.
pub async fn identify_iso(iso_path: PathBuf) -> Result<IdentificationResult> {
    let source = ScanSource::Iso(iso_path.clone());

    // Scan and mount in parallel — they're independent network/IO and
    // typically take similar wall-time (a few seconds each).
    let (scan_res, mount_res) = tokio::join!(scan(&source), MountedIso::mount(&iso_path));
    let scan_data = scan_res.context("running makemkvcon scan")?;
    let mount = mount_res.ok();

    let (disc_type, content_hash_value, identities) = match &mount {
        Some(m) => {
            let hash = enumerate_disc_files(&m.mount_point)
                .map(|files| content_hash(&files))
                .ok();
            let identities = match (&hash, TheDiscDbClient::with_default_endpoint()) {
                (Some(h), Ok(client)) => {
                    client.lookup_by_hash(h).await.unwrap_or_default()
                }
                _ => Vec::new(),
            };
            let disc_type = detect_disc_type_with_mount(&scan_data, &m.mount_point);
            (disc_type, hash, identities)
        }
        None => (detect_disc_type(&scan_data), None, Vec::new()),
    };

    Ok(IdentificationResult {
        scan: scan_data,
        mount,
        disc_type,
        content_hash: content_hash_value,
        identities,
    })
}
