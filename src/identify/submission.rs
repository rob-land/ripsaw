// Render a `disc0N.json` per TheDiscDB/data's schema, using the
// MakeMKV-side attributes from our scan + the user's per-title
// overrides from the title-detail page. Pure: no I/O, no side
// effects, no clock; the dateAdded field is the caller's
// responsibility (so unit tests are deterministic).
//
// `stage_submission` writes the rendered JSON to a per-disc staging
// directory under XDG_DATA_HOME/ripsaw/discdb-submissions/ so the
// user can copy or PR it into TheDiscDB/data manually. The
// browse-to-github helper opens the relevant repo path so the user
// has a single click from edit to "open a PR / file an issue."
//
// The shape we render matches what TheDiscDB/data already accepts.
// Reference example pulled from
// https://github.com/TheDiscDb/data/blob/main/data/movie/10%20Cloverfield%20Lane%20%282016%29/2016-blu-ray/disc01.json
//
//   {
//     "Index": 1,
//     "Slug": "blu-ray",
//     "Name": "Blu-ray",
//     "Format": "Blu-Ray",
//     "ContentHash": "57B059114B517DF43BE4D05FCA0869FA",
//     "Titles": [
//       {
//         "Index": 0,
//         "Comment": "Skyfall.mkv",
//         "SourceFile": "00500.mpls",
//         "SegmentMap": "1",
//         "Duration": "2:23:09",
//         "Size": 35830161408,
//         "DisplaySize": "33.3 GB",
//         "Item": { "Title": "Skyfall", "Type": "MainMovie", "Chapters": [...] },
//         "Tracks": [ { "Index": 0, "Name": "Mpeg4 AVC High@L4.1",
//                       "Type": "Video", "Resolution": "1920x1080", ... }, ... ]
//       }, ...
//     ]
//   }

use std::collections::HashMap;

use serde_json::json;

use crate::identify::{Identity, TitleIdentity, TitleRole};
use crate::rip::makemkv_parse::{MakemkvScan, TitleAttributes};
use crate::ui::title_detail_page::TitleEdit;

#[derive(Debug, Clone)]
pub struct DiscSubmission {
    pub disc_index: u32,
    pub disc_slug: String,
    pub disc_name: String,
    pub format: String,
    pub content_hash: String,
    /// Comment/note rendered as the disc's `Comment` field. Optional.
    pub comment: Option<String>,
}

/// Render the JSON. `edits` is keyed by MakeMKV title index; entries
/// are optional and used only to override what the scan / identity
/// supplied. `identity` is the matched TheDiscDB record, if any --
/// when provided, its per-title display titles, roles, and chapter
/// lists are used as the defaults, on top of which `edits` are
/// applied.
pub fn render_disc_json(
    disc: &DiscSubmission,
    scan: &MakemkvScan,
    identity: Option<&Identity>,
    edits: &HashMap<u32, TitleEdit>,
) -> String {
    let identity_titles: &[TitleIdentity] = identity
        .map(|i| i.titles.as_slice())
        .unwrap_or(&[]);

    let titles_json: Vec<serde_json::Value> = scan
        .titles
        .iter()
        .map(|t| render_title(t, identity_titles, edits.get(&t.index)))
        .collect();

    let mut disc_json = json!({
        "Index": disc.disc_index,
        "Slug": disc.disc_slug,
        "Name": disc.disc_name,
        "Format": disc.format,
        "ContentHash": disc.content_hash,
        "Titles": titles_json,
    });
    if let Some(c) = &disc.comment {
        disc_json["Comment"] = serde_json::Value::String(c.clone());
    }
    serde_json::to_string_pretty(&disc_json).expect("serde_json never fails on owned data")
}

fn render_title(
    t: &TitleAttributes,
    identity_titles: &[TitleIdentity],
    edit: Option<&TitleEdit>,
) -> serde_json::Value {
    // Match identity by sourceFile first (the join key TheDiscDB and
    // MakeMKV agree on), falling back to index.
    let identity = match_identity(identity_titles, t);

    // Display title precedence: edit override -> identity ->
    // MakeMKV name -> "Title N".
    let display_title = edit
        .and_then(|e| e.display_title.clone())
        .or_else(|| identity.map(|i| i.display_title.clone()).filter(|s| !s.is_empty()))
        .or_else(|| t.name.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("Title {}", t.index));

    // Role precedence: edit override -> identity -> Other.
    let role = edit
        .and_then(|e| e.role)
        .or_else(|| identity.map(|i| i.role))
        .unwrap_or(TitleRole::Other);

    // Chapter titles: edit overrides win wholesale when present, else
    // identity's chapters, else empty.
    let chapter_titles: Vec<String> = if let Some(e) = edit {
        if !e.chapter_titles.is_empty() {
            e.chapter_titles.clone()
        } else {
            chapters_from_identity(identity)
        }
    } else {
        chapters_from_identity(identity)
    };
    let chapters_json: Vec<serde_json::Value> = chapter_titles
        .iter()
        .enumerate()
        .map(|(i, title)| {
            json!({
                "Index": (i + 1) as u32, // TheDiscDB chapters are 1-based
                "Title": title,
            })
        })
        .collect();

    // Tracks: pass MakeMKV's stream info through, mapping to
    // TheDiscDB's Track shape. Audio-track name overrides come in a
    // follow-up (task #50) so for now this is a straight projection.
    let tracks_json: Vec<serde_json::Value> = t
        .streams
        .iter()
        .map(|s| render_track(s))
        .collect();

    let mut title_json = json!({
        "Index": t.index,
        "Item": {
            "Title": display_title,
            "Type": role_to_discdb(role),
            "Chapters": chapters_json,
        },
        "Tracks": tracks_json,
    });
    if let Some(src) = &t.source_file {
        title_json["SourceFile"] = serde_json::Value::String(src.clone());
    }
    if let Some(sm) = &t.segment_map {
        title_json["SegmentMap"] = serde_json::Value::String(sm.clone());
    }
    if let Some(d) = t.duration_seconds {
        title_json["Duration"] = serde_json::Value::String(format_duration(d));
    }
    if let Some(s) = t.size_bytes {
        title_json["Size"] = serde_json::Value::Number(s.into());
        title_json["DisplaySize"] = serde_json::Value::String(format_size(s));
    }
    if let Some(name) = &t.name {
        if !name.is_empty() {
            title_json["Comment"] = serde_json::Value::String(name.clone());
        }
    }
    title_json
}

fn render_track(s: &crate::rip::makemkv_parse::StreamAttributes) -> serde_json::Value {
    let mut tr = json!({ "Index": s.stream });
    if let Some(name) = &s.name {
        if !name.is_empty() {
            tr["Name"] = serde_json::Value::String(name.clone());
        }
    }
    if let Some(kind) = &s.kind {
        tr["Type"] = serde_json::Value::String(kind.clone());
    }
    if let Some(lang) = &s.language_name {
        tr["Language"] = serde_json::Value::String(lang.clone());
    }
    if let Some(code) = &s.language_code {
        tr["LanguageCode"] = serde_json::Value::String(code.clone());
    }
    if let Some(short) = &s.codec_short {
        // For audio streams, MakeMKV's short codec is often
        // "Surround 5.1" etc. which TheDiscDB stores as AudioType.
        match s.kind.as_deref() {
            Some("Audio") => {
                tr["AudioType"] = serde_json::Value::String(short.clone());
            }
            _ => {}
        }
    }
    if let Some(long) = &s.codec_long {
        // Codec long is "Mpeg4 AVC High@L4.1" etc. -- we keep this
        // as Description for audio + Resolution-adjacent metadata.
        if matches!(s.kind.as_deref(), Some("Video")) && long.is_empty() {
            // skip
        } else if tr.get("Name").is_none() {
            tr["Name"] = serde_json::Value::String(long.clone());
        }
    }
    if let Some(vs) = &s.video_size {
        tr["Resolution"] = serde_json::Value::String(vs.clone());
    }
    if let Some(ar) = &s.aspect_ratio {
        tr["AspectRatio"] = serde_json::Value::String(ar.clone());
    }
    tr
}

fn chapters_from_identity(identity: Option<&TitleIdentity>) -> Vec<String> {
    let Some(id) = identity else { return Vec::new(); };
    let mut chs = id.chapters.clone();
    chs.sort_by_key(|c| c.index);
    chs.into_iter().map(|c| c.title).collect()
}

fn match_identity<'a>(
    titles: &'a [TitleIdentity],
    t: &TitleAttributes,
) -> Option<&'a TitleIdentity> {
    if let Some(src) = t.source_file.as_deref().filter(|s| !s.is_empty()) {
        if let Some(found) = titles.iter().find(|i| {
            i.source_file
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(src))
        }) {
            return Some(found);
        }
    }
    titles.iter().find(|i| i.index == t.index)
}

fn role_to_discdb(role: TitleRole) -> &'static str {
    match role {
        TitleRole::Main => "MainMovie",
        TitleRole::Trailer => "Trailer",
        TitleRole::BehindTheScenes => "BehindTheScenes",
        TitleRole::DeletedScene => "DeletedScene",
        TitleRole::Featurette => "Featurette",
        TitleRole::Interview => "Interview",
        TitleRole::Scene => "Scene",
        TitleRole::Short => "Short",
        TitleRole::Other => "Other",
    }
}

fn format_duration(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Movie-level metadata; populates `data/<type>/{Title (Year)}/metadata.json`
/// in TheDiscDB/data.
#[derive(Debug, Clone, Default)]
pub struct MovieMetadata {
    pub title: String,
    pub year: Option<u32>,
    pub content_type: ContentType,
    pub plot: Option<String>,
    pub tagline: Option<String>,
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<String>,
    pub tvdb_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentType {
    #[default]
    Movie,
    Series,
}

impl ContentType {
    fn discdb_string(self) -> &'static str {
        match self {
            ContentType::Movie => "Movie",
            ContentType::Series => "Series",
        }
    }
    fn dir_segment(self) -> &'static str {
        match self {
            ContentType::Movie => "movie",
            ContentType::Series => "series",
        }
    }
}

/// Release-level metadata; populates the release.json under the
/// release-slug subdirectory.
#[derive(Debug, Clone, Default)]
pub struct ReleaseMetadata {
    pub slug: String,
    pub title: String,
    pub year: Option<u32>,
    pub locale: Option<String>,
    pub region_code: Option<String>,
    pub upc: Option<String>,
    pub asin: Option<String>,
    /// Relative path to the front-cover image, e.g.
    /// `Series/the-many-loves-of-dobie-gillis-1959/complete-series-boxset.jpg`.
    /// Format matches TheDiscDB convention: `<Type>/<slug>/<release-slug>.jpg`.
    pub image_url: Option<String>,
    /// ISO-8601 release date, e.g. `"2013-04-02T00:00:00+00:00"`.
    pub release_date: Option<String>,
    /// GitHub usernames credited with this submission. Renderered as
    /// `[{Name, Source: "github"}, ...]`. Empty `Vec` skips the field.
    pub contributors: Vec<String>,
    /// Publisher / studio identifiers (e.g. "Criterion", "Shout
    /// Factory"). Empty `Vec` renders `[]` (matches existing records).
    pub groups: Vec<String>,
}

/// Render the movie-level `metadata.json` per TheDiscDB/data shape.
pub fn render_metadata_json(movie: &MovieMetadata) -> String {
    let slug = title_slug(&movie.title, movie.year);
    let mut external_ids = serde_json::Map::new();
    if let Some(t) = movie.tmdb_id {
        external_ids.insert("Tmdb".into(), serde_json::Value::String(t.to_string()));
    }
    if let Some(i) = &movie.imdb_id {
        external_ids.insert("Imdb".into(), serde_json::Value::String(i.clone()));
    }
    if let Some(t) = &movie.tvdb_id {
        external_ids.insert("Tvdb".into(), serde_json::Value::String(t.clone()));
    }
    let mut obj = serde_json::json!({
        "Title": movie.title,
        "FullTitle": movie.title,
        "SortTitle": sort_title(&movie.title),
        "Slug": slug,
        "Type": movie.content_type.discdb_string(),
        "Groups": [],
    });
    if let Some(y) = movie.year {
        obj["Year"] = serde_json::json!(y);
    }
    if !external_ids.is_empty() {
        obj["ExternalIds"] = serde_json::Value::Object(external_ids);
    }
    if let Some(p) = &movie.plot {
        obj["Plot"] = serde_json::Value::String(p.clone());
    }
    if let Some(t) = &movie.tagline {
        obj["Tagline"] = serde_json::Value::String(t.clone());
    }
    serde_json::to_string_pretty(&obj).expect("serde_json never fails on owned data")
}

/// Render the release-level `release.json` per TheDiscDB/data shape.
pub fn render_release_json(release: &ReleaseMetadata) -> String {
    let mut obj = serde_json::json!({
        "Slug": release.slug,
        "Title": release.title,
        "SortTitle": format!(
            "{} {}",
            release.year.map(|y| y.to_string()).unwrap_or_default(),
            release.title
        )
        .trim()
        .to_string(),
    });
    if let Some(y) = release.year {
        obj["Year"] = serde_json::json!(y);
    }
    if let Some(l) = &release.locale {
        obj["Locale"] = serde_json::Value::String(l.clone());
    }
    if let Some(r) = &release.region_code {
        obj["RegionCode"] = serde_json::Value::String(r.clone());
    }
    if let Some(u) = &release.upc {
        obj["Upc"] = serde_json::Value::String(u.clone());
    }
    if let Some(a) = &release.asin {
        obj["Asin"] = serde_json::Value::String(a.clone());
    }
    if let Some(u) = &release.image_url {
        obj["ImageUrl"] = serde_json::Value::String(u.clone());
    }
    if let Some(d) = &release.release_date {
        obj["ReleaseDate"] = serde_json::Value::String(d.clone());
    }
    // DateAdded is always stamped at staging time; the submitter
    // doesn't supply it. Use the local timezone offset for
    // consistency with existing entries in the catalog.
    obj["DateAdded"] = serde_json::Value::String(now_iso8601_local());
    // Contributors: skip the field entirely when empty rather than
    // emit an empty array. Existing records always include at least
    // one contributor.
    if !release.contributors.is_empty() {
        let contributors: Vec<serde_json::Value> = release
            .contributors
            .iter()
            .map(|name| serde_json::json!({"Name": name, "Source": "github"}))
            .collect();
        obj["Contributors"] = serde_json::Value::Array(contributors);
    }
    obj["Groups"] = serde_json::Value::Array(
        release
            .groups
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect(),
    );
    serde_json::to_string_pretty(&obj).expect("serde_json never fails on owned data")
}

/// UTC ISO-8601 timestamp used as the `DateAdded` stamp on staged
/// release.json files. Existing entries in the catalog use the
/// submitter's local timezone, but UTC is valid ISO-8601 and avoids
/// pulling in a chrono/libc dependency just for the offset.
fn now_iso8601_local() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

fn epoch_to_ymdhms(epoch: i64) -> (i64, u32, u32, u32, u32, u32) {
    // Days/seconds since unix epoch. Computes calendar date in the
    // Gregorian proleptic calendar; correct for all sane future
    // timestamps. Algorithm: Howard Hinnant's `civil_from_days`.
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let h = (seconds / 3600) as u32;
    let mi = ((seconds % 3600) / 60) as u32;
    let s = (seconds % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let yy = y + if mo <= 2 { 1 } else { 0 };
    (yy, mo, d, h, mi, s)
}

/// Turn a movie title + year into the slug TheDiscDB uses (lowercase,
/// hyphenated, year suffix).
pub fn title_slug(title: &str, year: Option<u32>) -> String {
    let mut s: String = title
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c
            } else if c == ' ' || c == '-' {
                '-'
            } else {
                ' '
            }
        })
        .collect();
    s.retain(|c| c != ' ');
    // collapse runs of '-'
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let trimmed = s.trim_matches('-').to_string();
    match year {
        Some(y) => format!("{trimmed}-{y}"),
        None => trimmed,
    }
}

fn sort_title(title: &str) -> String {
    // Strip leading articles ("The", "A", "An") for sort key, per
    // TheDiscDB convention.
    let trimmed = title.trim();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["the ", "a ", "an "] {
        if lower.starts_with(prefix) {
            return trimmed[prefix.len()..].to_string();
        }
    }
    trimmed.to_string()
}

/// Optional artifacts that go alongside the JSON in the staged
/// submission: movie-level cover image, movie-level raw TMDB JSON
/// dump, release-level front-cover image. All optional -- bytes
/// missing means the file isn't written.
#[derive(Debug, Default, Clone)]
pub struct SubmissionArtifacts {
    /// Movie-level cover image (`cover.jpg` next to metadata.json).
    /// Typically a TMDB poster.
    pub cover_jpg: Option<Vec<u8>>,
    /// Movie-level raw TMDB JSON dump (`tmdb.json`).
    pub tmdb_json: Option<String>,
    /// Release-level front cover image (`front.jpg` in the release
    /// folder). Photograph of the physical packaging when the user
    /// can supply one; otherwise skipped.
    pub front_jpg: Option<Vec<u8>>,
}

/// Stage a full new-disc submission: movie metadata.json,
/// release.json, and the per-disc disc0N.json under the staging
/// root mirroring TheDiscDB's data tree layout. Returns the
/// directory the files landed in.
pub fn stage_full_submission(
    movie: &MovieMetadata,
    release: &ReleaseMetadata,
    disc: &DiscSubmission,
    scan: &MakemkvScan,
    identity: Option<&Identity>,
    edits: &HashMap<u32, TitleEdit>,
) -> anyhow::Result<std::path::PathBuf> {
    stage_full_submission_with(
        movie,
        release,
        disc,
        scan,
        identity,
        edits,
        &SubmissionArtifacts::default(),
    )
}

/// Same as `stage_full_submission` plus optional artifacts (cover
/// image, raw TMDB dump, front-cover photo). Callers that have
/// already fetched these from TMDB pass them here so the staging
/// tree matches the shape of existing TheDiscDB records.
pub fn stage_full_submission_with(
    movie: &MovieMetadata,
    release: &ReleaseMetadata,
    disc: &DiscSubmission,
    scan: &MakemkvScan,
    identity: Option<&Identity>,
    edits: &HashMap<u32, TitleEdit>,
    artifacts: &SubmissionArtifacts,
) -> anyhow::Result<std::path::PathBuf> {
    let folder_name = match movie.year {
        Some(y) => format!("{} ({y})", movie.title),
        None => movie.title.clone(),
    };
    let movie_dir = staging_root()
        .join("data")
        .join(movie.content_type.dir_segment())
        .join(sanitize_dir(&folder_name));
    let dir = movie_dir.join(sanitize_dir(&release.slug));
    std::fs::create_dir_all(&dir)?;

    let metadata_path = movie_dir.join("metadata.json");
    std::fs::write(&metadata_path, render_metadata_json(movie))?;

    if let Some(bytes) = &artifacts.cover_jpg {
        std::fs::write(movie_dir.join("cover.jpg"), bytes)?;
    }
    if let Some(json) = &artifacts.tmdb_json {
        std::fs::write(movie_dir.join("tmdb.json"), json)?;
    }

    let release_path = dir.join("release.json");
    std::fs::write(&release_path, render_release_json(release))?;

    if let Some(bytes) = &artifacts.front_jpg {
        std::fs::write(dir.join("front.jpg"), bytes)?;
    }

    let disc_filename = format!("disc{:02}.json", disc.disc_index.max(1));
    let disc_path = dir.join(&disc_filename);
    std::fs::write(&disc_path, render_disc_json(disc, scan, identity, edits))?;

    Ok(dir)
}

fn sanitize_dir(name: &str) -> String {
    // Posix-safe directory name: keep most chars, replace path separators.
    name.replace('/', "_").replace('\\', "_")
}

/// Where on disk staged submissions land. Honours `$XDG_DATA_HOME`,
/// falling back to `$HOME/.local/share/`. Public so the UI can pop a
/// toast / dialog telling the user where to look.
pub fn staging_root() -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg);
        if !p.as_os_str().is_empty() {
            return p.join("ripsaw").join("discdb-submissions");
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local").join("share").join("ripsaw").join("discdb-submissions")
}

/// Write a rendered `disc0N.json` for `disc` to the staging dir. The
/// directory layout mirrors TheDiscDb/data's: under the staging root
/// we create `<disc_hash>/` and drop `disc<NN>.json` inside it.
/// Returns the absolute path to the written file.
pub fn stage_submission(
    disc: &DiscSubmission,
    scan: &MakemkvScan,
    identity: Option<&Identity>,
    edits: &HashMap<u32, TitleEdit>,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = staging_root().join(&disc.content_hash);
    std::fs::create_dir_all(&dir).map_err(|e| {
        anyhow::anyhow!("creating staging dir {}: {e}", dir.display())
    })?;
    let json = render_disc_json(disc, scan, identity, edits);
    let filename = format!("disc{:02}.json", disc.disc_index.max(1));
    let path = dir.join(filename);
    std::fs::write(&path, json).map_err(|e| {
        anyhow::anyhow!("writing {}: {e}", path.display())
    })?;
    Ok(path)
}

/// URL to direct the user at after staging. The disc-hash-keyed path
/// is something a TheDiscDB maintainer can search for; the
/// upstream/issues fallback handles the "I don't know exactly where
/// this disc lives in the catalog" case.
pub fn github_repo_url() -> &'static str {
    "https://github.com/TheDiscDb/data"
}

/// Open a URL with the user's default browser via xdg-open. Returns
/// `Err` if xdg-open is missing -- the caller should fall back to
/// showing the URL in a toast or dialog.
pub fn open_in_browser(url: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("xdg-open").arg(url).status()?;
    if !status.success() {
        anyhow::bail!("xdg-open {url} exited with status {status}");
    }
    Ok(())
}

fn format_size(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identify::{ChapterIdentity, Identity, TitleIdentity, TitleRole};
    use crate::rip::makemkv_parse::{DiscAttributes, StreamAttributes, TitleAttributes};

    fn scan_with_one_title() -> MakemkvScan {
        MakemkvScan {
            disc: DiscAttributes::default(),
            titles: vec![TitleAttributes {
                index: 0,
                name: Some("Skyfall_t00.mkv".into()),
                source_file: Some("00500.mpls".into()),
                segment_map: Some("1".into()),
                duration_seconds: Some(8589),
                size_bytes: Some(35_830_161_408),
                output_file: Some("Skyfall_t00.mkv".into()),
                streams: vec![
                    StreamAttributes {
                        stream: 0,
                        kind: Some("Video".into()),
                        codec_short: Some("Mpeg4".into()),
                        codec_long: Some("Mpeg4 AVC High@L4.1".into()),
                        video_size: Some("1920x1080".into()),
                        aspect_ratio: Some("16:9".into()),
                        ..Default::default()
                    },
                    StreamAttributes {
                        stream: 1,
                        kind: Some("Audio".into()),
                        codec_short: Some("Surround 5.1".into()),
                        codec_long: Some("DTS-HD Master Audio".into()),
                        name: Some("DTS-HD MA".into()),
                        language_code: Some("eng".into()),
                        language_name: Some("English".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn skyfall_identity() -> Identity {
        Identity {
            media_item_id: "760".into(),
            release_slug: "2020-james-bond-collection-blu-ray".into(),
            disc_index: 1,
            titles: vec![TitleIdentity {
                index: 0,
                role: TitleRole::Main,
                display_title: "Skyfall".into(),
                source_file: Some("00500.mpls".into()),
                chapters: vec![
                    ChapterIdentity { index: 1, title: "Agent Down".into() },
                    ChapterIdentity { index: 2, title: "The Chase is On".into() },
                ],
                season: None,
                episode: None,
            }],
            item_title: "Skyfall".into(),
            year: Some(2012),
            tmdb_id: Some(37724),
            imdb_id: None,
            tvdb_id: None,
        }
    }

    fn disc_meta() -> DiscSubmission {
        DiscSubmission {
            disc_index: 1,
            disc_slug: "blu-ray".into(),
            disc_name: "Blu-ray".into(),
            format: "Blu-Ray".into(),
            content_hash: "F315672F1088242165689B2B8A471DE8".into(),
            comment: None,
        }
    }

    #[test]
    fn renders_disc_envelope_with_titles() {
        let scan = scan_with_one_title();
        let edits = HashMap::new();
        let out = render_disc_json(&disc_meta(), &scan, None, &edits);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["Slug"], "blu-ray");
        assert_eq!(v["Format"], "Blu-Ray");
        assert_eq!(v["ContentHash"], "F315672F1088242165689B2B8A471DE8");
        assert_eq!(v["Titles"][0]["Index"], 0);
        assert_eq!(v["Titles"][0]["SourceFile"], "00500.mpls");
        assert_eq!(v["Titles"][0]["Duration"], "2:23:09");
        assert_eq!(v["Titles"][0]["DisplaySize"], "35.8 GB");
        // Without identity, role falls back to Other and chapters are empty.
        assert_eq!(v["Titles"][0]["Item"]["Type"], "Other");
        assert_eq!(v["Titles"][0]["Item"]["Chapters"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn identity_supplies_role_display_title_and_chapters() {
        let scan = scan_with_one_title();
        let id = skyfall_identity();
        let out = render_disc_json(&disc_meta(), &scan, Some(&id), &HashMap::new());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["Titles"][0]["Item"]["Title"], "Skyfall");
        assert_eq!(v["Titles"][0]["Item"]["Type"], "MainMovie");
        let chs = v["Titles"][0]["Item"]["Chapters"].as_array().unwrap();
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[0]["Index"], 1);
        assert_eq!(chs[0]["Title"], "Agent Down");
    }

    #[test]
    fn edit_overrides_take_precedence_over_identity() {
        let scan = scan_with_one_title();
        let id = skyfall_identity();
        let mut edits = HashMap::new();
        edits.insert(
            0,
            TitleEdit {
                title_index: 0,
                display_title: Some("Skyfall (corrected)".into()),
                role: Some(TitleRole::Trailer),
                chapter_titles: vec!["Cold Open".into(), "Title Sequence".into()],
            },
        );
        let out = render_disc_json(&disc_meta(), &scan, Some(&id), &edits);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["Titles"][0]["Item"]["Title"], "Skyfall (corrected)");
        assert_eq!(v["Titles"][0]["Item"]["Type"], "Trailer");
        let chs = v["Titles"][0]["Item"]["Chapters"].as_array().unwrap();
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[0]["Title"], "Cold Open");
        assert_eq!(chs[1]["Index"], 2);
    }

    #[test]
    fn audio_track_codec_short_lands_as_audio_type() {
        let scan = scan_with_one_title();
        let out = render_disc_json(&disc_meta(), &scan, None, &HashMap::new());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let tracks = v["Titles"][0]["Tracks"].as_array().unwrap();
        let audio = tracks
            .iter()
            .find(|t| t["Type"] == "Audio")
            .expect("expected an audio track");
        assert_eq!(audio["AudioType"], "Surround 5.1");
        assert_eq!(audio["LanguageCode"], "eng");
        assert_eq!(audio["Language"], "English");
    }

    #[test]
    fn duration_formatter_matches_thediscdb_style() {
        assert_eq!(format_duration(8589), "2:23:09");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(3661), "1:01:01");
    }

    #[test]
    fn size_formatter_matches_thediscdb_style() {
        assert_eq!(format_size(35_830_161_408), "35.8 GB");
        assert_eq!(format_size(750_000_000), "750 MB");
    }

    #[test]
    fn metadata_json_contains_external_ids_when_present() {
        let m = MovieMetadata {
            title: "Skyfall".into(),
            year: Some(2012),
            content_type: ContentType::Movie,
            plot: Some("James Bond's loyalty to M is tested.".into()),
            tagline: Some("Think on your sins.".into()),
            tmdb_id: Some(37724),
            imdb_id: Some("tt1074638".into()),
            tvdb_id: None,
        };
        let j: serde_json::Value = serde_json::from_str(&render_metadata_json(&m)).unwrap();
        assert_eq!(j["Title"], "Skyfall");
        assert_eq!(j["Type"], "Movie");
        assert_eq!(j["Year"], 2012);
        assert_eq!(j["Slug"], "skyfall-2012");
        assert_eq!(j["SortTitle"], "Skyfall");
        assert_eq!(j["ExternalIds"]["Tmdb"], "37724");
        assert_eq!(j["ExternalIds"]["Imdb"], "tt1074638");
        assert_eq!(j["Plot"], "James Bond's loyalty to M is tested.");
        assert_eq!(j["Tagline"], "Think on your sins.");
    }

    #[test]
    fn release_json_renders_slug_year_locale_upc() {
        let r = ReleaseMetadata {
            slug: "2020-james-bond-collection-blu-ray".into(),
            title: "2020 James Bond Collection Blu-ray".into(),
            year: Some(2020),
            locale: Some("en-us".into()),
            region_code: Some("1".into()),
            upc: Some("883904346708".into()),
            asin: None,
            ..Default::default()
        };
        let j: serde_json::Value = serde_json::from_str(&render_release_json(&r)).unwrap();
        assert_eq!(j["Slug"], "2020-james-bond-collection-blu-ray");
        assert_eq!(j["Year"], 2020);
        assert_eq!(j["Locale"], "en-us");
        assert_eq!(j["RegionCode"], "1");
        assert_eq!(j["Upc"], "883904346708");
    }

    #[test]
    fn title_slug_lowercases_and_replaces_spaces() {
        assert_eq!(
            title_slug("10 Cloverfield Lane", Some(2016)),
            "10-cloverfield-lane-2016"
        );
        assert_eq!(
            title_slug("The Lord of the Rings", Some(2001)),
            "the-lord-of-the-rings-2001"
        );
        assert_eq!(title_slug("Movie!", None), "movie");
    }

    #[test]
    fn sort_title_strips_leading_article() {
        assert_eq!(sort_title("The Matrix"), "Matrix");
        assert_eq!(sort_title("A Few Good Men"), "Few Good Men");
        assert_eq!(sort_title("An American in Paris"), "American in Paris");
        assert_eq!(sort_title("Skyfall"), "Skyfall");
    }
}
