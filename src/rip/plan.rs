// Translate a disc identification + selected title indexes into a
// concrete rip plan: where each title's MKV should land in the library
// once it has been extracted and renamed.

use std::collections::HashMap;
use std::path::PathBuf;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::identify::pipeline::IdentificationResult;
use crate::identify::{TitleIdentity, TitleRole};
use crate::naming::{
    self, jellyfin::Jellyfin, kodi::Kodi, plex::Plex, EpisodeContext, ExtraContext, MovieContext,
    Scheme,
};
use crate::rip::makemkv_parse::TitleAttributes;
use crate::settings::SchemeKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscContentKind {
    Movie,
    Series,
}

/// Auto-detect whether a disc carries a single film or a set of episodes.
/// Heuristic:
///
/// - Count titles longer than 10 minutes. Shorter ones are almost always
///   menu loops, idents, or under-the-radar extras.
/// - If there are at least 3 such "main candidate" titles AND they're all
///   within 20% of each other in duration AND no single candidate is over
///   90 minutes, classify as a series.
/// - Otherwise (single long title, two long titles, or a movie + extras
///   pattern), classify as a movie.
///
/// Examples:
/// - JP 3D BD: one 2h title + several short clips → Movie
/// - VS_305 DVD: one 88m title + 3 short clips → Movie
/// - Dobie Gillis DVD: 8 titles ~25m each → Series
pub fn auto_detect_content_kind(titles: &[TitleAttributes]) -> DiscContentKind {
    let main_durations: Vec<u64> = titles
        .iter()
        .filter_map(|t| t.duration_seconds)
        .filter(|d| *d >= 600)
        .collect();
    if main_durations.len() < 3 {
        return DiscContentKind::Movie;
    }
    let min = *main_durations.iter().min().unwrap();
    let max = *main_durations.iter().max().unwrap();
    if min == 0 {
        return DiscContentKind::Movie;
    }
    let ratio = max as f64 / min as f64;
    if ratio <= 1.20 && max <= 5400 {
        DiscContentKind::Series
    } else {
        DiscContentKind::Movie
    }
}

/// Parse a disc-volume label like `DOBIEGILLIS_S1D1` into a guessed series
/// title and season number. Returns `(title, season_opt)`. Always returns
/// a title; only `season_opt` may be `None` when the label has no season
/// hint.
pub fn parse_series_label(label: &str) -> (String, Option<u32>) {
    static SEASON_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)[_\- ](?:s|season[_\- ]?)(\d{1,2})(?:[_\- ]?d\d+)?\s*$")
            .expect("static regex")
    });

    let trimmed = label.trim();
    if let Some(captures) = SEASON_RE.captures(trimmed) {
        let season: Option<u32> = captures.get(1).and_then(|m| m.as_str().parse().ok());
        let start = captures.get(0).unwrap().start();
        let series_part = trimmed[..start].trim_end_matches(['_', '-', ' ']);
        return (title_case_from_label(series_part), season);
    }
    (title_case_from_label(trimmed), None)
}

fn title_case_from_label(s: &str) -> String {
    if s.is_empty() {
        return s.to_string();
    }
    let spaced = s.replace(['_', '-'], " ");
    let collapsed: String = spaced
        .split_whitespace()
        .map(capitalize_first)
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        s.to_string()
    } else {
        collapsed
    }
}

fn capitalize_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => {
            let rest: String = chars.flat_map(|c| c.to_lowercase()).collect();
            let mut out = String::new();
            out.extend(first.to_uppercase());
            out.push_str(&rest);
            out
        }
        None => String::new(),
    }
}

#[derive(Debug, Clone)]
pub struct NamingOpts {
    pub library_root: PathBuf,
    pub scheme: SchemeKind,
    pub content_kind: DiscContentKind,
    pub disc_title: String,
    pub disc_year: Option<u32>,
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<String>,
    /// Only used when content_kind is Series. Defaults to 1.
    pub season: u32,
    /// Only used when content_kind is Series. First selected title gets
    /// `episode_start`, subsequent titles get `episode_start + 1` etc.
    pub episode_start: u32,
    /// When set, every ripped title is post-processed into this 3D
    /// output layout via the convert pipeline after rip + rename land.
    pub conversion_format: Option<crate::convert::format::OutputFormat>,
    /// Codec for the post-rip 3D conversion. Carried through to the
    /// orchestrator's ConversionPlan build.
    pub conversion_codec: crate::convert::hw::EncodeCodec,
    /// HW encode backend for the post-rip 3D conversion.
    pub conversion_hw_backend: crate::convert::hw::HwBackend,
}

#[derive(Debug, Clone)]
pub struct PlannedTitle {
    pub title_index: u32,
    pub display_label: String,
    pub role: TitleRole,
    /// Directory makemkvcon writes into.
    pub output_dir: PathBuf,
    /// Filename makemkvcon is expected to choose (from TINFO code 27).
    pub output_filename: String,
    /// Where the orchestrator should rename the produced MKV after extraction.
    /// `None` means leave the file where MakeMKV put it (used when no naming
    /// scheme is applied — flat output to a single directory).
    pub final_path: Option<PathBuf>,
    /// Per-chapter titles from TheDiscDB (1-based, ordered). Empty when
    /// there is no identity or the title has no submitted chapters.
    /// Embedded into the MKV via mkvpropedit after the rip succeeds.
    pub chapter_titles: Vec<String>,
    /// Value to write to the MKV's Segment.Title via
    /// `mkvpropedit --edit info --set title=...`. `None` skips the set.
    pub segment_title: Option<String>,
    /// When set, the orchestrator auto-runs the 3D conversion pipeline
    /// on the produced MKV after rename + metadata. Carried forward
    /// from NamingOpts.conversion_format on the source page.
    pub conversion_format: Option<crate::convert::format::OutputFormat>,
    pub conversion_codec: crate::convert::hw::EncodeCodec,
    pub conversion_hw_backend: crate::convert::hw::HwBackend,
}

/// Build per-title rip targets. Roles are inferred from
/// `identification.identities[0]` when present; otherwise the longest title
/// is treated as the Main feature and everything else as Other. For series
/// content (when `naming.content_kind == Series`), every selected title is
/// numbered sequentially as an episode starting at `naming.episode_start`.
///
/// `episode_titles_by_index` maps the makemkvcon title index (not the
/// episode number) to the user-supplied episode title. Empty when the
/// user hasn't filled any in — the planner falls back to bare
/// `S01E01.mkv` style filenames in that case.
pub fn plan_rip(
    identification: &IdentificationResult,
    selected_indexes: &[u32],
    naming: Option<&NamingOpts>,
    episode_titles_by_index: &HashMap<u32, String>,
) -> Vec<PlannedTitle> {
    let main_index = main_title_index(identification);
    let identity_titles: &[TitleIdentity] = identification
        .identities
        .first()
        .map(|i| i.titles.as_slice())
        .unwrap_or(&[]);
    let mut episode_counter: u32 = naming.map(|n| n.episode_start).unwrap_or(1);
    selected_indexes
        .iter()
        .filter_map(|idx| {
            identification
                .scan
                .titles
                .iter()
                .find(|t| t.index == *idx)
                .map(|t| {
                    let identity = match_identity_for(identity_titles, t);
                    let role = identity.map(|i| i.role).unwrap_or_else(|| {
                        if Some(t.index) == main_index {
                            TitleRole::Main
                        } else {
                            TitleRole::Other
                        }
                    });
                    let is_series_episode = matches!(
                        naming,
                        Some(n) if n.content_kind == DiscContentKind::Series
                    );
                    let episode_number = if is_series_episode {
                        let n = episode_counter;
                        episode_counter += 1;
                        Some(n)
                    } else {
                        None
                    };
                    let episode_title = episode_titles_by_index
                        .get(&t.index)
                        .map(|s| s.as_str())
                        .filter(|s| !s.trim().is_empty());
                    plan_one_title(t, role, identity, naming, episode_number, episode_title)
                })
        })
        .collect()
}

/// Match a MakeMKV title to its TheDiscDB identity by `source_file`,
/// since the two indices are independent. Falls back to comparing
/// indices when sourceFile is missing on either side (defensive --
/// real responses set it).
pub fn match_identity_for<'a>(
    identities: &'a [TitleIdentity],
    title: &TitleAttributes,
) -> Option<&'a TitleIdentity> {
    if let Some(src) = title.source_file.as_deref().filter(|s| !s.is_empty()) {
        if let Some(found) = identities
            .iter()
            .find(|i| i.source_file.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(src)))
        {
            return Some(found);
        }
    }
    identities.iter().find(|i| i.index == title.index)
}

fn plan_one_title(
    t: &TitleAttributes,
    role: TitleRole,
    identity: Option<&TitleIdentity>,
    naming: Option<&NamingOpts>,
    episode_number: Option<u32>,
    episode_title: Option<&str>,
) -> PlannedTitle {
    let identity_display = identity
        .map(|i| i.display_title.as_str())
        .filter(|s| !s.is_empty());
    let display_label = match (t.index, identity_display, t.name.as_deref()) {
        (idx, Some(dt), _) => format!("Title {idx} — {dt}"),
        (idx, None, Some(n)) if !n.is_empty() => format!("Title {idx} — {n}"),
        (idx, _, _) => format!("Title {idx}"),
    };
    let output_filename = t
        .output_file
        .clone()
        .unwrap_or_else(|| format!("title_t{:02}.mkv", t.index));
    let (output_dir, final_path) = match naming {
        Some(opts) => {
            let final_p = scheme_path(t, role, identity, opts, episode_number, episode_title);
            let dir = final_p
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| opts.library_root.clone());
            (dir, Some(final_p))
        }
        None => (PathBuf::new(), None),
    };
    let chapter_titles = identity
        .map(|i| {
            let mut by_index = i.chapters.clone();
            by_index.sort_by_key(|c| c.index);
            by_index.into_iter().map(|c| c.title).collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // The MKV Segment.Title is the human-facing name shown by some
    // players. Use the filename basename (without .mkv) so it always
    // matches what is on disk -- per the user's request that this
    // mirror the filename.
    let segment_title = final_path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned());
    PlannedTitle {
        title_index: t.index,
        display_label,
        role,
        output_dir,
        output_filename,
        final_path,
        chapter_titles,
        segment_title,
        conversion_format: naming.and_then(|n| n.conversion_format),
        conversion_codec: naming
            .map(|n| n.conversion_codec)
            .unwrap_or_else(crate::convert::plan::ConversionPlan::default_codec),
        conversion_hw_backend: naming
            .map(|n| n.conversion_hw_backend)
            .unwrap_or_else(crate::convert::plan::ConversionPlan::default_hw_backend),
    }
}

fn scheme_path(
    t: &TitleAttributes,
    role: TitleRole,
    identity: Option<&TitleIdentity>,
    opts: &NamingOpts,
    episode_number: Option<u32>,
    episode_title: Option<&str>,
) -> PathBuf {
    let scheme: Box<dyn Scheme> = match opts.scheme {
        SchemeKind::Jellyfin | SchemeKind::Emby => Box::new(Jellyfin),
        SchemeKind::Plex => Box::new(Plex),
        SchemeKind::Kodi => Box::new(Kodi),
    };
    if opts.content_kind == DiscContentKind::Series {
        let shows_root = opts.library_root.join("Shows");
        let episode_ctx = EpisodeContext {
            root: &shows_root,
            series_title: &opts.disc_title,
            series_year: opts.disc_year,
            tmdb_id: opts.tmdb_id,
            imdb_id: opts.imdb_id.as_deref(),
            tvdb_id: None,
            season: opts.season,
            episode: episode_number.unwrap_or(1),
            episode_end: None,
            episode_title,
        };
        let _ = t.name; // we used to default to this; explicit episode_title wins.
        return scheme.episode_path(&episode_ctx);
    }

    let movies_root = opts.library_root.join("Movies");
    let movie_ctx = MovieContext {
        root: &movies_root,
        title: &opts.disc_title,
        year: opts.disc_year,
        tmdb_id: opts.tmdb_id,
        imdb_id: opts.imdb_id.as_deref(),
        variant: None,
    };
    match role {
        TitleRole::Main => scheme.movie_path(&movie_ctx),
        other_role => {
            // Prefer TheDiscDB's display title for the extra's filename
            // ("Theatrical Trailer.mkv"); fall back to MakeMKV's title
            // name; last resort "Title N".
            let display_title = identity
                .map(|i| i.display_title.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    t.name
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("Title {}", t.index));
            let title_identity = crate::identify::TitleIdentity {
                index: t.index,
                role: other_role,
                display_title,
                source_file: t.source_file.clone(),
                chapters: Vec::new(),
                season: None,
                episode: None,
            };
            let extra_ctx = ExtraContext {
                movie: &movie_ctx,
                title: &title_identity,
                role: other_role,
            };
            scheme.extras_path(&extra_ctx)
        }
    }
}

fn main_title_index(identification: &IdentificationResult) -> Option<u32> {
    if let Some(identity) = identification.identities.first() {
        if let Some(main) = identity.titles.iter().find(|t| t.role == TitleRole::Main) {
            // Match TheDiscDB's main title to a MakeMKV title by
            // sourceFile (the two indices are independent orderings).
            if let Some(src) = main.source_file.as_deref().filter(|s| !s.is_empty()) {
                if let Some(t) = identification.scan.titles.iter().find(|t| {
                    t.source_file
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(src))
                }) {
                    return Some(t.index);
                }
            }
            return Some(main.index);
        }
    }
    identification
        .scan
        .titles
        .iter()
        .max_by_key(|t| t.duration_seconds.unwrap_or(0))
        .map(|t| t.index)
}

fn role_for_title(
    idx: u32,
    identification: &IdentificationResult,
    main_index: Option<u32>,
) -> TitleRole {
    if let Some(identity) = identification.identities.first() {
        if let Some(t) = identity.titles.iter().find(|t| t.index == idx) {
            return t.role;
        }
    }
    if Some(idx) == main_index {
        TitleRole::Main
    } else {
        TitleRole::Other
    }
}

/// A sensible default library root: `$HOME/Videos`. Falls back to `/tmp`
/// if `$HOME` is not set.
pub fn default_library_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Videos")
}

/// Build a `NamingOpts` for an unidentified disc. The disc's MakeMKV
/// label is used as the title for movies; for series the label is run
/// through `parse_series_label` to extract a series title and season.
/// IDs / year are left blank.
pub fn naming_opts_for_unidentified(
    library_root: PathBuf,
    scheme: SchemeKind,
    content_kind: DiscContentKind,
    disc_name: &str,
) -> NamingOpts {
    let (title, season) = match content_kind {
        DiscContentKind::Movie => (naming::sanitise(disc_name), 1),
        DiscContentKind::Series => {
            let (parsed_title, parsed_season) = parse_series_label(disc_name);
            (naming::sanitise(&parsed_title), parsed_season.unwrap_or(1))
        }
    };
    NamingOpts {
        library_root,
        scheme,
        content_kind,
        disc_title: title,
        disc_year: None,
        tmdb_id: None,
        imdb_id: None,
        season,
        episode_start: 1,
        conversion_format: None,
        conversion_codec: crate::convert::plan::ConversionPlan::default_codec(),
        conversion_hw_backend: crate::convert::plan::ConversionPlan::default_hw_backend(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identify::DiscType;
    use crate::rip::makemkv_parse::{DiscAttributes, MakemkvScan, TitleAttributes};

    fn scan_with(titles: Vec<TitleAttributes>, name: Option<&str>) -> IdentificationResult {
        IdentificationResult {
            scan: MakemkvScan {
                disc: DiscAttributes { name: name.map(str::to_string), ..Default::default() },
                titles,
                ..Default::default()
            },
            mount: None,
            disc_type: DiscType::BluRay,
            content_hash: None,
            identities: Vec::new(),
            source: crate::rip::makemkv::ScanSource::Iso(std::path::PathBuf::new()),
            source_file: None,
            has_mvc: false,
        }
    }

    fn title(index: u32, name: &str, dur: u64, output: &str) -> TitleAttributes {
        TitleAttributes {
            index,
            name: Some(name.into()),
            duration_seconds: Some(dur),
            output_file: Some(output.into()),
            ..Default::default()
        }
    }

    #[test]
    fn longest_unidentified_title_is_treated_as_main() {
        let id = scan_with(
            vec![
                title(0, "short", 60, "x_t00.mkv"),
                title(1, "long", 7200, "x_t01.mkv"),
                title(2, "medium", 300, "x_t02.mkv"),
            ],
            Some("Some Disc"),
        );
        assert_eq!(main_title_index(&id), Some(1));
        assert_eq!(role_for_title(1, &id, Some(1)), TitleRole::Main);
        assert_eq!(role_for_title(0, &id, Some(1)), TitleRole::Other);
        assert_eq!(role_for_title(2, &id, Some(1)), TitleRole::Other);
    }

    #[test]
    fn jellyfin_unidentified_plan_writes_main_to_movie_folder() {
        let id = scan_with(
            vec![
                title(0, "Main Feature", 7200, "X_t00.mkv"),
                title(1, "Trailer", 90, "X_t01.mkv"),
            ],
            Some("Some Disc"),
        );
        let opts = naming_opts_for_unidentified(
            PathBuf::from("/lib"),
            SchemeKind::Jellyfin,
            DiscContentKind::Movie,
            "Some Disc",
        );
        let plan = plan_rip(&id, &[0, 1], Some(&opts), &HashMap::new());
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Movies/Some Disc/Some Disc.mkv")
        );
        // Title 1 is treated as Other (no catalog role) -> goes to extras/.
        assert_eq!(
            plan[1].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Movies/Some Disc/extras/Trailer.mkv")
        );
    }

    #[test]
    fn auto_detect_classifies_dobie_gillis_as_series() {
        // 8 episodes ~25min each, like the user's Dobie Gillis disc.
        let titles: Vec<TitleAttributes> = (0..8)
            .map(|i| title(i as u32, &format!("ep{i}"), 1535 + (i * 2) as u64, "x.mkv"))
            .collect();
        assert_eq!(auto_detect_content_kind(&titles), DiscContentKind::Series);
    }

    #[test]
    fn auto_detect_classifies_jurassic_park_as_movie() {
        // One ~2h main + several short clips.
        let titles = vec![
            title(0, "main", 7602, "x.mkv"),
            title(1, "ext", 507, "x.mkv"),
            title(2, "ext", 44, "x.mkv"),
            title(3, "clip", 59, "x.mkv"),
        ];
        assert_eq!(auto_detect_content_kind(&titles), DiscContentKind::Movie);
    }

    #[test]
    fn auto_detect_classifies_movie_with_long_extras_as_movie() {
        // Main + featurette: 2 long titles, not enough to be a series.
        let titles = vec![
            title(0, "main", 5400, "x.mkv"),
            title(1, "ext", 830, "x.mkv"),
            title(2, "clip", 60, "x.mkv"),
        ];
        assert_eq!(auto_detect_content_kind(&titles), DiscContentKind::Movie);
    }

    #[test]
    fn auto_detect_rejects_wildly_uneven_durations() {
        let titles = vec![
            title(0, "a", 5400, "x.mkv"),
            title(1, "b", 1200, "x.mkv"),
            title(2, "c", 700, "x.mkv"),
        ];
        // Ratio 5400/700 ~= 7.7 -- not similar enough.
        assert_eq!(auto_detect_content_kind(&titles), DiscContentKind::Movie);
    }

    #[test]
    fn parse_series_label_extracts_season_and_strips_disc_suffix() {
        assert_eq!(
            parse_series_label("DOBIEGILLIS_S1D1"),
            ("Dobiegillis".into(), Some(1))
        );
        assert_eq!(
            parse_series_label("the_expanse_s02d03"),
            ("The Expanse".into(), Some(2))
        );
        assert_eq!(
            parse_series_label("Buffy The Vampire Slayer S7"),
            ("Buffy The Vampire Slayer".into(), Some(7))
        );
    }

    #[test]
    fn parse_series_label_falls_back_when_no_season_marker() {
        assert_eq!(parse_series_label("Random Disc"), ("Random Disc".into(), None));
    }

    #[test]
    fn jellyfin_series_plan_numbers_episodes_sequentially() {
        let id = scan_with(
            vec![
                title(0, "ep0", 1500, "X_t00.mkv"),
                title(1, "ep1", 1505, "X_t01.mkv"),
                title(2, "ep2", 1510, "X_t02.mkv"),
            ],
            Some("DOBIEGILLIS_S1D1"),
        );
        let opts = naming_opts_for_unidentified(
            PathBuf::from("/lib"),
            SchemeKind::Jellyfin,
            DiscContentKind::Series,
            "DOBIEGILLIS_S1D1",
        );
        assert_eq!(opts.disc_title, "Dobiegillis");
        assert_eq!(opts.season, 1);

        let plan = plan_rip(&id, &[0, 1, 2], Some(&opts), &HashMap::new());
        assert_eq!(plan.len(), 3);
        assert_eq!(
            plan[0].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Shows/Dobiegillis/Season 01/Dobiegillis S01E01.mkv")
        );
        assert_eq!(
            plan[1].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Shows/Dobiegillis/Season 01/Dobiegillis S01E02.mkv")
        );
        assert_eq!(
            plan[2].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Shows/Dobiegillis/Season 01/Dobiegillis S01E03.mkv")
        );
    }

    #[test]
    fn series_episode_start_can_be_offset() {
        let id = scan_with(
            vec![title(0, "ep", 1500, "X_t00.mkv")],
            Some("series"),
        );
        let mut opts = naming_opts_for_unidentified(
            PathBuf::from("/lib"),
            SchemeKind::Jellyfin,
            DiscContentKind::Series,
            "Series Name",
        );
        opts.episode_start = 7;
        opts.season = 2;
        let plan = plan_rip(&id, &[0], Some(&opts), &HashMap::new());
        assert_eq!(
            plan[0].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Shows/Series Name/Season 02/Series Name S02E07.mkv")
        );
    }

    #[test]
    fn user_supplied_episode_titles_land_in_filename() {
        let id = scan_with(
            vec![
                title(0, "raw0", 1500, "X_t00.mkv"),
                title(1, "raw1", 1505, "X_t01.mkv"),
            ],
            Some("series"),
        );
        let opts = naming_opts_for_unidentified(
            PathBuf::from("/lib"),
            SchemeKind::Jellyfin,
            DiscContentKind::Series,
            "Some Series",
        );
        let mut episode_titles = HashMap::new();
        episode_titles.insert(0u32, "Pilot".to_string());
        episode_titles.insert(1u32, "The Sequel".to_string());

        let plan = plan_rip(&id, &[0, 1], Some(&opts), &episode_titles);
        assert_eq!(
            plan[0].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Shows/Some Series/Season 01/Some Series S01E01 - Pilot.mkv")
        );
        assert_eq!(
            plan[1].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Shows/Some Series/Season 01/Some Series S01E02 - The Sequel.mkv")
        );
    }

    #[test]
    fn missing_episode_title_falls_back_to_bare_sxxeyy() {
        let id = scan_with(
            vec![
                title(0, "raw0", 1500, "X_t00.mkv"),
                title(1, "raw1", 1505, "X_t01.mkv"),
            ],
            Some("series"),
        );
        let opts = naming_opts_for_unidentified(
            PathBuf::from("/lib"),
            SchemeKind::Jellyfin,
            DiscContentKind::Series,
            "Some Series",
        );
        let mut episode_titles = HashMap::new();
        episode_titles.insert(0u32, "Pilot".to_string());
        // Title 1 is left out of the map.

        let plan = plan_rip(&id, &[0, 1], Some(&opts), &episode_titles);
        assert_eq!(
            plan[0].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Shows/Some Series/Season 01/Some Series S01E01 - Pilot.mkv")
        );
        assert_eq!(
            plan[1].final_path.as_deref().unwrap(),
            std::path::Path::new("/lib/Shows/Some Series/Season 01/Some Series S01E02.mkv")
        );
    }

    #[test]
    fn no_naming_opts_means_flat_output_with_no_final_path() {
        let id = scan_with(vec![title(0, "M", 60, "x_t00.mkv")], Some("d"));
        let plan = plan_rip(&id, &[0], None, &HashMap::new());
        assert_eq!(plan.len(), 1);
        assert!(plan[0].final_path.is_none());
        assert_eq!(plan[0].output_filename, "x_t00.mkv");
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn title_case_handles_underscored_caps() {
        assert_eq!(title_case_from_label("THE_EXPANSE"), "The Expanse");
        assert_eq!(title_case_from_label("dobiegillis"), "Dobiegillis");
        assert_eq!(title_case_from_label("foo bar baz"), "Foo Bar Baz");
    }
}
