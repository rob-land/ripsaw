// Translate a disc identification + selected title indexes into a
// concrete rip plan: where each title's MKV should land in the library
// once it has been extracted and renamed.

use std::path::PathBuf;

use crate::identify::pipeline::IdentificationResult;
use crate::identify::TitleRole;
use crate::naming::{
    self, jellyfin::Jellyfin, kodi::Kodi, plex::Plex, ExtraContext, MovieContext, Scheme,
};
use crate::rip::makemkv_parse::TitleAttributes;
use crate::settings::SchemeKind;

#[derive(Debug, Clone)]
pub struct NamingOpts {
    pub library_root: PathBuf,
    pub scheme: SchemeKind,
    pub disc_title: String,
    pub disc_year: Option<u32>,
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<String>,
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
}

/// Build per-title rip targets. Roles are inferred from
/// `identification.identities[0]` when present; otherwise the longest title
/// is treated as the Main feature and everything else as Other.
pub fn plan_rip(
    identification: &IdentificationResult,
    selected_indexes: &[u32],
    naming: Option<&NamingOpts>,
) -> Vec<PlannedTitle> {
    let main_index = main_title_index(identification);
    let role_for = |idx: u32| role_for_title(idx, identification, main_index);
    selected_indexes
        .iter()
        .filter_map(|idx| {
            identification
                .scan
                .titles
                .iter()
                .find(|t| t.index == *idx)
                .map(|t| {
                    let role = role_for(t.index);
                    plan_one_title(t, role, naming)
                })
        })
        .collect()
}

fn plan_one_title(
    t: &TitleAttributes,
    role: TitleRole,
    naming: Option<&NamingOpts>,
) -> PlannedTitle {
    let display_label = match (t.index, t.name.as_deref()) {
        (idx, Some(n)) if !n.is_empty() => format!("Title {idx} — {n}"),
        (idx, _) => format!("Title {idx}"),
    };
    let output_filename = t
        .output_file
        .clone()
        .unwrap_or_else(|| format!("title_t{:02}.mkv", t.index));
    let (output_dir, final_path) = match naming {
        Some(opts) => {
            let final_p = scheme_path(t, role, opts);
            // Have makemkvcon write into the same directory, so the rename is
            // a same-filesystem hardlink-like move rather than a copy.
            let dir = final_p
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| opts.library_root.clone());
            (dir, Some(final_p))
        }
        None => (PathBuf::new(), None),
    };
    PlannedTitle {
        title_index: t.index,
        display_label,
        role,
        output_dir,
        output_filename,
        final_path,
    }
}

fn scheme_path(t: &TitleAttributes, role: TitleRole, opts: &NamingOpts) -> PathBuf {
    let scheme: Box<dyn Scheme> = match opts.scheme {
        SchemeKind::Jellyfin | SchemeKind::Emby => Box::new(Jellyfin),
        SchemeKind::Plex => Box::new(Plex),
        SchemeKind::Kodi => Box::new(Kodi),
    };
    let movie_ctx = MovieContext {
        root: &opts.library_root.join("Movies"),
        title: &opts.disc_title,
        year: opts.disc_year,
        tmdb_id: opts.tmdb_id,
        imdb_id: opts.imdb_id.as_deref(),
        variant: None,
    };
    match role {
        TitleRole::Main => scheme.movie_path(&movie_ctx),
        other_role => {
            let display_title = t
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Title {}", t.index));
            let title_identity = crate::identify::TitleIdentity {
                index: t.index,
                role: other_role,
                display_title,
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

/// Build a `NamingOpts` for an unidentified disc by treating the disc's
/// MakeMKV label as the movie title and otherwise leaving IDs / year blank.
/// Sanitises the disc name for path safety.
pub fn naming_opts_for_unidentified(
    library_root: PathBuf,
    scheme: SchemeKind,
    disc_name: &str,
) -> NamingOpts {
    NamingOpts {
        library_root,
        scheme,
        disc_title: naming::sanitise(disc_name),
        disc_year: None,
        tmdb_id: None,
        imdb_id: None,
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
            "Some Disc",
        );
        let plan = plan_rip(&id, &[0, 1], Some(&opts));
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
    fn no_naming_opts_means_flat_output_with_no_final_path() {
        let id = scan_with(vec![title(0, "M", 60, "x_t00.mkv")], Some("d"));
        let plan = plan_rip(&id, &[0], None);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].final_path.is_none());
        assert_eq!(plan[0].output_filename, "x_t00.mkv");
    }
}
