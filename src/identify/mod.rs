// Disc identification. See docs/identify.md.

pub mod bdmt;
pub mod composite;
pub mod disc_hash;
pub mod dvd;
pub mod ffprobe;
pub mod from_scan;
pub mod pipeline;
pub mod submission;
pub mod thediscdb;
pub mod tmdb;
pub mod submit;
pub mod submit_pr;
pub mod upc;

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// DiscFingerprint: the full scan-time signal collection.
// See docs/identify.md § "Disc fingerprint".
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscFingerprint {
    pub content_hash: String,
    pub disc_type: DiscType,
    pub volume_label: Option<String>,
    pub upc: Option<String>,
    pub makemkv_disc_info: MakeMkvDiscInfo,
    pub aacs: Option<AacsInfo>,
    pub on_disc_metadata: Option<OnDiscMetadata>,
    pub bdmv_index_summary: Option<BdmvIndexSummary>,
    pub titles: Vec<TitleFingerprint>,
    pub drive: DriveInfo,
    pub makemkv_version: String,
    pub structural_hash: String,
    pub scan_timestamp: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscType {
    Dvd,
    BluRay,
    UltraHdBluRay,
    BluRay3D,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MakeMkvDiscInfo {
    pub name: Option<String>,
    pub comment: Option<String>,
    pub language_code: Option<String>,
    pub content_type: Option<String>,
    pub year: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AacsInfo {
    pub volume_id_hex: String,
    pub mkb_version: u32,
    pub libredrive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnDiscMetadata {
    pub title: Option<String>,
    pub description: Option<String>,
    pub language_code: Option<String>,
    pub source_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BdmvIndexSummary {
    pub playlist_count: u32,
    pub bdj_object_count: u32,
    pub first_play_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleFingerprint {
    pub index: u32,
    pub duration_seconds: u64,
    pub size_bytes: u64,
    pub source_file: String,
    pub segment_map: String,
    pub chapter_count: u32,
    pub streams: Vec<StreamFingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamFingerprint {
    pub index: u32,
    pub kind: StreamKind,
    pub codec: String,
    pub language_code: Option<String>,
    pub channels: Option<u8>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveInfo {
    pub device: PathBuf,
    pub vendor: String,
    pub model: String,
    pub firmware: String,
    pub libredrive_mode: bool,
}

// ---------------------------------------------------------------------------
// Identity: post-lookup result. Populated only when a catalog (currently
// TheDiscDB) returns a hit; carries the title-role information the lookup
// chain in docs/identify.md surfaces.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub media_item_id: String,
    pub release_slug: String,
    pub disc_index: u32,
    pub titles: Vec<TitleIdentity>,
    /// Display title of the matched media item (e.g. "Skyfall").
    /// Pulled from TheDiscDB's `MediaItem.title`. May be empty when
    /// missing upstream.
    #[serde(default)]
    pub item_title: String,
    /// Release year of the media item (e.g. 2012). `None` when
    /// TheDiscDB has no year on the matched record.
    #[serde(default)]
    pub year: Option<u32>,
    /// TheDiscDB external IDs (TMDb, IMDb, TVDb) when present on
    /// the matched MediaItem.
    #[serde(default)]
    pub tmdb_id: Option<u64>,
    #[serde(default)]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub tvdb_id: Option<String>,
    /// Cover-art reference from TheDiscDB (`MediaItem.imageUrl`, falling
    /// back to the release's). May be a relative path
    /// (`Movie/<slug>/cover.jpg`) or an absolute URL; resolve with
    /// `cover_art_url`. `None` when the record has no image.
    #[serde(default)]
    pub image_url: Option<String>,
}

impl Identity {
    /// Resolve `image_url` to a downloadable URL. TheDiscDB stores it as a
    /// slug-based path (`Movie/<slug>/cover.jpg`) served from its static
    /// image host at `thediscdb.com/images/`. Notably that static path
    /// stays up even when the GraphQL API is down (verified 2026-06), so a
    /// successfully-identified disc can still show its cover. Absolute
    /// URLs pass through unchanged.
    pub fn cover_art_url(&self) -> Option<String> {
        let raw = self.image_url.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        if raw.starts_with("http://") || raw.starts_with("https://") {
            Some(raw.to_string())
        } else {
            Some(format!("https://thediscdb.com/images/{}", raw.trim_start_matches('/')))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleIdentity {
    pub index: u32,
    pub role: TitleRole,
    pub display_title: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// MakeMKV's source-file name for this title (e.g. "00342.m2ts"
    /// or "00500.mpls"). Used as the join key against
    /// `TitleAttributes.source_file` since TheDiscDB's per-title
    /// `index` is its own ordering and does NOT line up with
    /// MakeMKV's title index.
    #[serde(default)]
    pub source_file: Option<String>,
    /// Per-chapter titles (1-based by `index`). Empty when TheDiscDB
    /// has no chapters submitted for the title.
    #[serde(default)]
    pub chapters: Vec<ChapterIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterIdentity {
    pub index: u32,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TitleRole {
    Main,
    Trailer,
    BehindTheScenes,
    DeletedScene,
    Featurette,
    Interview,
    Scene,
    Short,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_with_image(image: Option<&str>) -> Identity {
        Identity {
            media_item_id: "1".into(),
            release_slug: "r".into(),
            disc_index: 0,
            titles: Vec::new(),
            item_title: "X".into(),
            year: None,
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            image_url: image.map(str::to_string),
        }
    }

    #[test]
    fn cover_art_url_resolves_relative_path_to_image_host() {
        let id = identity_with_image(Some("Movie/friday-the-13th-part-iii-1982/cover.jpg"));
        assert_eq!(
            id.cover_art_url().as_deref(),
            Some("https://thediscdb.com/images/Movie/friday-the-13th-part-iii-1982/cover.jpg")
        );
    }

    #[test]
    fn cover_art_url_passes_absolute_url_through_and_handles_empty() {
        assert_eq!(
            identity_with_image(Some("https://cdn.example/poster.jpg")).cover_art_url().as_deref(),
            Some("https://cdn.example/poster.jpg")
        );
        // Leading slash is trimmed so we don't get a double slash.
        assert_eq!(
            identity_with_image(Some("/Movie/x/cover.jpg")).cover_art_url().as_deref(),
            Some("https://thediscdb.com/images/Movie/x/cover.jpg")
        );
        assert_eq!(identity_with_image(Some("  ")).cover_art_url(), None);
        assert_eq!(identity_with_image(None).cover_art_url(), None);
    }
}
