// Local TheDiscDB lookup against a mirror of the `TheDiscDb/data` GitHub
// repo. See docs/thediscdb-local.md.
//
// The hosted GraphQL endpoint has proven unreliable (observed fully down),
// but the catalogue data is open and complete on GitHub. A user who syncs
// the JSON subset (scripts/sync-thediscdb.sh) gets offline, outage-proof,
// instant lookups. This module:
//
//   1. builds a contentHash -> discNN.json index by walking the mirror,
//   2. resolves a hash to our `Identity` shape by reading the matched
//      disc's JSON plus its sibling release.json and the title-level
//      metadata.json (external IDs + cover art).
//
// The on-disk JSON is PascalCase and disc-centric, so it needs its own
// deserialisation types and mapper — distinct from the camelCase GraphQL
// ones in `thediscdb.rs`. The file *is* the matched disc, so there's no
// multi-disc hash filtering to do.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{ChapterIdentity, Identity, TitleIdentity, TitleRole};

/// A local mirror of `TheDiscDb/data`. `root` is the directory that
/// contains the repo's `data/` tree (i.e. `root/data/movie/...`).
pub struct LocalDiscDb {
    root: PathBuf,
}

impl LocalDiscDb {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `true` when the mirror looks populated (`<root>/data` exists).
    pub fn is_present(&self) -> bool {
        self.root.join("data").is_dir()
    }

    /// Look up a disc by its TheDiscDB content hash. Returns every
    /// catalogued disc whose `ContentHash` matches (normally one, but a
    /// hash can recur across regional pressings — mirroring the live
    /// client's one-Identity-per-matching-release behaviour). An empty
    /// vec means the hash isn't in the mirror.
    pub fn lookup_by_hash(&self, hash: &str) -> Result<Vec<Identity>> {
        let index = self.build_index().context("building local TheDiscDB index")?;
        let mut out = Vec::new();
        if let Some(paths) = index.get(&hash.to_ascii_uppercase()) {
            for disc_path in paths {
                match identity_from_disc_file(disc_path) {
                    Ok(id) => out.push(id),
                    Err(e) => tracing::warn!(
                        "local TheDiscDB: failed to map {}: {e:#}",
                        disc_path.display()
                    ),
                }
            }
        }
        Ok(out)
    }

    /// Walk the mirror and build `contentHash (upper) -> [discNN.json]`.
    /// Reads each `disc*.json` for its `ContentHash`. Cheap enough to do
    /// per lookup for now (a few thousand small files); persisting the
    /// index is a future optimisation (docs/thediscdb-local.md).
    pub fn build_index(&self) -> Result<HashMap<String, Vec<PathBuf>>> {
        let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let data_dir = self.root.join("data");
        if !data_dir.is_dir() {
            return Ok(index);
        }
        for kind in ["movie", "series", "sets"] {
            let kind_dir = data_dir.join(kind);
            if kind_dir.is_dir() {
                index_dir_recursive(&kind_dir, &mut index)?;
            }
        }
        Ok(index)
    }
}

/// Count the disc records currently in the mirror (`disc*.json` files).
/// 0 when the mirror is absent.
pub fn disc_count(root: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let p = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => walk(&p, n),
                Ok(_) if is_disc_json(&p) => *n += 1,
                _ => {}
            }
        }
    }
    let mut n = 0;
    walk(&root.join("data"), &mut n);
    n
}

/// Download or refresh the local mirror by syncing the JSON subset of
/// `TheDiscDb/data` into `root` (blobless sparse checkout — JSON only, no
/// images/summaries). Blocking; call from a worker thread. Returns the
/// disc-record count afterwards. Mirrors `scripts/sync-thediscdb.sh` so
/// the app doesn't depend on locating that script at runtime.
pub fn sync_mirror(root: &Path) -> Result<usize> {
    use std::process::Command;
    const REPO: &str = "https://github.com/TheDiscDb/data.git";
    let git = |args: &[&str]| -> Result<()> {
        let status = Command::new("git")
            .args(args)
            .status()
            .context("running git (is it installed?)")?;
        if !status.success() {
            anyhow::bail!("git {:?} failed with {status}", args);
        }
        Ok(())
    };
    let root_s = root.to_string_lossy().into_owned();

    if root.join(".git").is_dir() {
        git(&["-C", &root_s, "sparse-checkout", "reapply"])?;
        git(&["-C", &root_s, "pull", "--ff-only"])?;
    } else {
        std::fs::create_dir_all(root)
            .with_context(|| format!("creating mirror dir {}", root.display()))?;
        git(&["clone", "--filter=blob:none", "--no-checkout", REPO, &root_s])?;
        git(&["-C", &root_s, "sparse-checkout", "init", "--no-cone"])?;
        git(&[
            "-C", &root_s, "sparse-checkout", "set", "--no-cone",
            "/**/*.json", "!/**/*.txt", "!/**/*.jpg", "!/**/*.jpeg", "!/**/*.png", "!/**/*.webp",
        ])?;
        git(&["-C", &root_s, "checkout"])?;
    }
    Ok(disc_count(root))
}

/// Recursively find `disc*.json` files and add their hash -> path entry.
fn index_dir_recursive(dir: &Path, index: &mut HashMap<String, Vec<PathBuf>>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            index_dir_recursive(&path, index)?;
        } else if is_disc_json(&path) {
            if let Some(hash) = read_disc_hash(&path) {
                index.entry(hash.to_ascii_uppercase()).or_default().push(path);
            }
        }
    }
    Ok(())
}

fn is_disc_json(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    // `disc01.json`, `disc1.json`, ... but not `disc01-summary.txt`.
    name.starts_with("disc") && name.ends_with(".json")
}

/// Read only the `ContentHash` from a disc file (cheap; avoids fully
/// deserialising the Titles array just to index).
fn read_disc_hash(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct HashOnly {
        #[serde(rename = "ContentHash")]
        content_hash: Option<String>,
    }
    let bytes = std::fs::read(path).ok()?;
    let parsed: HashOnly = serde_json::from_slice(&bytes).ok()?;
    parsed.content_hash.filter(|h| !h.trim().is_empty())
}

/// Build an `Identity` from a matched `discNN.json`, reading its sibling
/// `release.json` (one level up) and the title's `metadata.json` (two
/// levels up) for the release slug, external IDs, and cover art.
fn identity_from_disc_file(disc_path: &Path) -> Result<Identity> {
    let disc: DiscFile = read_json(disc_path)?;

    let release_dir = disc_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("disc file has no parent dir"))?;
    let release: Option<ReleaseFile> = read_json(&release_dir.join("release.json")).ok();
    let title_dir = release_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("release dir has no parent dir"))?;
    let metadata: Option<MetadataFile> = read_json(&title_dir.join("metadata.json")).ok();

    // Release slug: prefer release.json's, fall back to the directory name.
    let release_slug = release
        .as_ref()
        .and_then(|r| r.slug.clone())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| release_dir.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let ext = metadata.as_ref().and_then(|m| m.external_ids.as_ref());
    let tmdb_id = ext
        .and_then(|e| e.tmdb.as_deref())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let imdb_id = ext
        .and_then(|e| e.imdb.clone())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tvdb_id = ext
        .and_then(|e| e.tvdb.clone())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Cover art: media-item image, falling back to the release's.
    let image_url = metadata
        .as_ref()
        .and_then(|m| m.image_url.clone())
        .or_else(|| release.as_ref().and_then(|r| r.image_url.clone()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // media_item_id: the files carry no numeric id, so use the metadata
    // slug (stable identifier), falling back to the title directory name.
    let media_item_id = metadata
        .as_ref()
        .and_then(|m| m.slug.clone())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| title_dir.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let item_title = metadata
        .as_ref()
        .and_then(|m| m.title.clone())
        .unwrap_or_default();
    let year = metadata
        .as_ref()
        .and_then(|m| m.year)
        .and_then(|y| if y > 0 { Some(y as u32) } else { None });

    Ok(Identity {
        media_item_id,
        release_slug,
        disc_index: disc.index.unwrap_or(0),
        titles: disc.titles.iter().map(title_identity_from).collect(),
        item_title,
        year,
        tmdb_id,
        imdb_id,
        tvdb_id,
        image_url,
    })
}

fn title_identity_from(t: &TitleFile) -> TitleIdentity {
    let item = t.item.as_ref();
    let chapters = item
        .map(|i| {
            i.chapters
                .iter()
                .filter_map(|c| {
                    c.title
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(|s| ChapterIdentity { index: c.index, title: s.to_string() })
                })
                .collect()
        })
        .unwrap_or_default();
    TitleIdentity {
        index: t.index.unwrap_or(0),
        role: parse_role(item.and_then(|i| i.item_type.as_deref())),
        display_title: item.and_then(|i| i.title.clone()).unwrap_or_default(),
        source_file: t.source_file.clone(),
        chapters,
        season: item.and_then(|i| i.season),
        episode: item.and_then(|i| i.episode),
    }
}

/// Map TheDiscDB's `Item.Type` strings to our role. Same vocabulary as
/// the GraphQL path's `parse_role` (kept in sync deliberately).
fn parse_role(s: Option<&str>) -> TitleRole {
    match s {
        Some("MainMovie") | Some("MainFeature") | Some("Main") | Some("Movie") => TitleRole::Main,
        Some("Trailer") => TitleRole::Trailer,
        Some("BehindTheScenes") | Some("BehindThe Scenes") => TitleRole::BehindTheScenes,
        Some("DeletedScene") | Some("DeletedScenes") => TitleRole::DeletedScene,
        Some("Featurette") => TitleRole::Featurette,
        Some("Interview") => TitleRole::Interview,
        Some("Scene") => TitleRole::Scene,
        Some("Short") => TitleRole::Short,
        _ => TitleRole::Other,
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

// ---------------------------------------------------------------------------
// On-disk JSON types (PascalCase). Private to this module.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DiscFile {
    index: Option<u32>,
    #[serde(default)]
    titles: Vec<TitleFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TitleFile {
    index: Option<u32>,
    #[serde(default)]
    source_file: Option<String>,
    #[serde(default)]
    item: Option<ItemFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemFile {
    title: Option<String>,
    season: Option<u32>,
    episode: Option<u32>,
    #[serde(rename = "Type")]
    item_type: Option<String>,
    #[serde(default)]
    chapters: Vec<ChapterFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ChapterFile {
    index: u32,
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReleaseFile {
    slug: Option<String>,
    image_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MetadataFile {
    title: Option<String>,
    slug: Option<String>,
    year: Option<i32>,
    image_url: Option<String>,
    external_ids: Option<ExternalIdsFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ExternalIdsFile {
    tmdb: Option<String>,
    imdb: Option<String>,
    tvdb: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/thediscdb")
    }

    #[test]
    fn indexes_and_resolves_real_friday_the_13th_disc() {
        let db = LocalDiscDb::new(fixture_root());
        assert!(db.is_present(), "fixture mirror should be present");

        let index = db.build_index().unwrap();
        let hash = "0F7341A7F10CC1B5B9FFE2D220245509";
        assert!(index.contains_key(hash), "hash should be indexed");

        let ids = db.lookup_by_hash(hash).unwrap();
        assert_eq!(ids.len(), 1, "one matching disc");
        let id = &ids[0];
        assert_eq!(id.item_title, "Friday the 13th Part III");
        assert_eq!(id.year, Some(1982));
        assert_eq!(id.tmdb_id, Some(9728));
        assert_eq!(id.imdb_id.as_deref(), Some("tt0083972"));
        assert_eq!(
            id.cover_art_url().as_deref(),
            Some("https://thediscdb.com/images/Movie/friday-the-13th-part-iii-1982/cover.jpg")
        );
        assert_eq!(id.release_slug, "2020-shout-factory-deluxe-edition-collection");
        // The MainMovie title maps to the Main role.
        assert!(
            id.titles.iter().any(|t| t.role == TitleRole::Main),
            "the feature should be tagged Main"
        );
        assert!(!id.titles.is_empty());
    }

    #[test]
    fn hash_lookup_is_case_insensitive_and_misses_cleanly() {
        let db = LocalDiscDb::new(fixture_root());
        assert_eq!(
            db.lookup_by_hash("0f7341a7f10cc1b5b9ffe2d220245509").unwrap().len(),
            1,
            "lowercase hash still hits"
        );
        assert!(
            db.lookup_by_hash("DEADBEEFDEADBEEFDEADBEEFDEADBEEF").unwrap().is_empty(),
            "unknown hash misses with an empty vec, not an error"
        );
    }

    #[test]
    fn absent_mirror_yields_empty_not_error() {
        let db = LocalDiscDb::new(Path::new("/nonexistent/ripsaw/mirror"));
        assert!(!db.is_present());
        assert!(db.lookup_by_hash("0F7341A7F10CC1B5B9FFE2D220245509").unwrap().is_empty());
    }
}
