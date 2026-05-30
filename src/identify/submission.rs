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
}
