// Jellyfin scheme. See docs/naming.md § "Jellyfin".

use std::path::PathBuf;

use crate::settings::SchemeKind;

use super::{extras, sanitise, EpisodeContext, ExtraContext, MovieContext, MovieVariant, Scheme};

pub struct Jellyfin;

impl Scheme for Jellyfin {
    fn movie_path(&self, ctx: &MovieContext) -> PathBuf {
        let folder = movie_folder_name(ctx);
        let filename = format!("{}.mkv", movie_basename(ctx));
        ctx.root.join(folder).join(filename)
    }

    fn episode_path(&self, ctx: &EpisodeContext) -> PathBuf {
        let series_folder = series_folder_name(ctx);
        let season_folder = format!("Season {:02}", ctx.season);
        let ep_part = match ctx.episode_end {
            Some(end) => format!("S{:02}E{:02}-E{:02}", ctx.season, ctx.episode, end),
            None => format!("S{:02}E{:02}", ctx.season, ctx.episode),
        };
        let title = sanitise(ctx.series_title);
        let year_part = ctx.series_year.map(|y| format!(" ({y})")).unwrap_or_default();
        let episode_part = ctx
            .episode_title
            .map(|t| sanitise(t))
            .filter(|t| !t.is_empty())
            .map(|t| format!(" - {t}"))
            .unwrap_or_default();
        let filename = format!("{title}{year_part} {ep_part}{episode_part}.mkv");
        ctx.root.join(series_folder).join(season_folder).join(filename)
    }

    fn extras_path(&self, ctx: &ExtraContext) -> PathBuf {
        let folder = movie_folder_name(ctx.movie);
        let sub = extras::folder_for_role(SchemeKind::Jellyfin, ctx.role);
        let filename = format!("{}.mkv", sanitise(&ctx.title.display_title));
        let mut path = ctx.movie.root.join(folder);
        if !sub.is_empty() {
            path = path.join(sub);
        }
        path.join(filename)
    }
}

fn movie_folder_name(ctx: &MovieContext) -> String {
    let mut s = sanitise(ctx.title);
    if let Some(y) = ctx.year {
        s.push_str(&format!(" ({y})"));
    }
    if let Some(bracket) = movie_id_bracket(ctx) {
        s.push_str(&bracket);
    }
    s
}

fn movie_basename(ctx: &MovieContext) -> String {
    // Jellyfin's recommendation is that filename mirrors the folder
    // name, including the metadata-provider ID. Variant suffix comes
    // last so the matching prefix stays intact:
    //   Skyfall (2012) [tmdbid-37724] - 3D.mkv
    let mut s = sanitise(ctx.title);
    if let Some(y) = ctx.year {
        s.push_str(&format!(" ({y})"));
    }
    if let Some(bracket) = movie_id_bracket(ctx) {
        s.push_str(&bracket);
    }
    if let Some(suffix) = variant_suffix(ctx.variant) {
        s.push_str(suffix);
    }
    s
}

/// Jellyfin metadata-provider tag (" [imdbid-X]", " [tmdbid-N]").
/// IMDb wins when both are present — per Jellyfin docs, only one
/// provider tag is honoured, and IMDb is the most stable canonical
/// identifier. Returns `None` when no ID is known.
fn movie_id_bracket(ctx: &MovieContext) -> Option<String> {
    if let Some(imdb) = ctx.imdb_id {
        Some(format!(" [imdbid-{imdb}]"))
    } else {
        ctx.tmdb_id.map(|tmdb| format!(" [tmdbid-{tmdb}]"))
    }
}

fn series_folder_name(ctx: &EpisodeContext) -> String {
    let mut s = sanitise(ctx.series_title);
    if let Some(y) = ctx.series_year {
        s.push_str(&format!(" ({y})"));
    }
    if let Some(imdb) = ctx.imdb_id {
        s.push_str(&format!(" [imdbid-{imdb}]"));
    } else if let Some(tmdb) = ctx.tmdb_id {
        s.push_str(&format!(" [tmdbid-{tmdb}]"));
    } else if let Some(tvdb) = ctx.tvdb_id {
        s.push_str(&format!(" [tvdbid-{tvdb}]"));
    }
    s
}

fn variant_suffix(v: Option<MovieVariant>) -> Option<&'static str> {
    match v? {
        MovieVariant::Uhd4k => Some(" - 4K"),
        MovieVariant::Stereo3d => Some(" - 3D"),
        MovieVariant::Extended => Some(" - Extended"),
        MovieVariant::Theatrical => Some(" - Theatrical"),
        MovieVariant::Directors => Some(" - Director's Cut"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identify::{TitleIdentity, TitleRole};
    use std::path::Path;

    fn movie() -> MovieContext<'static> {
        MovieContext {
            root: Path::new("/lib/Movies"),
            title: "Avatar",
            year: Some(2009),
            tmdb_id: None,
            imdb_id: Some("tt0499549"),
            variant: None,
        }
    }

    #[test]
    fn movie_folder_and_filename_share_imdbid_bracket() {
        // Per Jellyfin docs (jellyfin.org/docs/general/server/media/movies):
        // "Each file must begin exactly with the parent folder name —
        // including any year and/or metadata provider IDs". So the ID
        // is duplicated in both folder and filename.
        let p = Jellyfin.movie_path(&movie());
        assert_eq!(
            p,
            Path::new(
                "/lib/Movies/Avatar (2009) [imdbid-tt0499549]/Avatar (2009) [imdbid-tt0499549].mkv"
            )
        );
    }

    #[test]
    fn movie_with_3d_variant_appends_suffix_after_id_bracket() {
        let mut m = movie();
        m.variant = Some(MovieVariant::Stereo3d);
        let p = Jellyfin.movie_path(&m);
        assert_eq!(
            p,
            Path::new(
                "/lib/Movies/Avatar (2009) [imdbid-tt0499549]/Avatar (2009) [imdbid-tt0499549] - 3D.mkv"
            )
        );
    }

    #[test]
    fn movie_falls_back_to_tmdb_when_no_imdb() {
        let mut m = movie();
        m.imdb_id = None;
        m.tmdb_id = Some(19995);
        let p = Jellyfin.movie_path(&m);
        assert_eq!(
            p,
            Path::new("/lib/Movies/Avatar (2009) [tmdbid-19995]/Avatar (2009) [tmdbid-19995].mkv")
        );
    }

    #[test]
    fn movie_drops_ids_section_when_none_known() {
        let mut m = movie();
        m.imdb_id = None;
        let p = Jellyfin.movie_path(&m);
        assert_eq!(p, Path::new("/lib/Movies/Avatar (2009)/Avatar (2009).mkv"));
    }

    #[test]
    fn episode_path_includes_episode_title_when_provided() {
        let ctx = EpisodeContext {
            root: Path::new("/lib/Shows"),
            series_title: "The Expanse",
            series_year: Some(2015),
            tmdb_id: None,
            imdb_id: Some("tt3230854"),
            tvdb_id: None,
            season: 1,
            episode: 3,
            episode_end: None,
            episode_title: Some("Remember the Cant"),
        };
        let p = Jellyfin.episode_path(&ctx);
        assert_eq!(
            p,
            Path::new(
                "/lib/Shows/The Expanse (2015) [imdbid-tt3230854]/Season 01/The Expanse (2015) S01E03 - Remember the Cant.mkv"
            )
        );
    }

    #[test]
    fn episode_path_omits_title_suffix_when_none_or_empty() {
        let mut ctx = EpisodeContext {
            root: Path::new("/lib/Shows"),
            series_title: "The Expanse",
            series_year: Some(2015),
            tmdb_id: None,
            imdb_id: Some("tt3230854"),
            tvdb_id: None,
            season: 1,
            episode: 3,
            episode_end: None,
            episode_title: None,
        };
        let p = Jellyfin.episode_path(&ctx);
        assert_eq!(
            p,
            Path::new(
                "/lib/Shows/The Expanse (2015) [imdbid-tt3230854]/Season 01/The Expanse (2015) S01E03.mkv"
            )
        );
        ctx.episode_title = Some("   ");
        let p = Jellyfin.episode_path(&ctx);
        // Whitespace-only title sanitises to empty and gets dropped.
        assert_eq!(
            p,
            Path::new(
                "/lib/Shows/The Expanse (2015) [imdbid-tt3230854]/Season 01/The Expanse (2015) S01E03.mkv"
            )
        );
    }

    #[test]
    fn multi_episode_uses_range_suffix() {
        let ctx = EpisodeContext {
            root: Path::new("/lib/Shows"),
            series_title: "The Expanse",
            series_year: Some(2015),
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            season: 2,
            episode: 1,
            episode_end: Some(2),
            episode_title: None,
        };
        let p = Jellyfin.episode_path(&ctx);
        assert_eq!(p, Path::new("/lib/Shows/The Expanse (2015)/Season 02/The Expanse (2015) S02E01-E02.mkv"));
    }

    #[test]
    fn extras_path_goes_into_role_subfolder() {
        let title = TitleIdentity {
            index: 2,
            role: TitleRole::Trailer,
            display_title: "Theatrical Trailer".into(),
            source_file: None,
            season: None,
            episode: None,
        };
        let m = movie();
        let ctx = ExtraContext { movie: &m, title: &title, role: TitleRole::Trailer };
        let p = Jellyfin.extras_path(&ctx);
        assert_eq!(
            p,
            Path::new("/lib/Movies/Avatar (2009) [imdbid-tt0499549]/trailers/Theatrical Trailer.mkv")
        );
    }

    #[test]
    fn extras_main_role_emits_no_subfolder() {
        // Catch-all guard: passing Main as a role to extras_path is a misuse,
        // but the result should still be sensible (file at the movie root).
        let title = TitleIdentity {
            index: 0,
            role: TitleRole::Main,
            display_title: "Avatar".into(),
            source_file: None,
            season: None,
            episode: None,
        };
        let m = movie();
        let ctx = ExtraContext { movie: &m, title: &title, role: TitleRole::Main };
        let p = Jellyfin.extras_path(&ctx);
        assert_eq!(p, Path::new("/lib/Movies/Avatar (2009) [imdbid-tt0499549]/Avatar.mkv"));
    }

    #[test]
    fn sanitises_path_unsafe_title() {
        // `sanitise()` replaces `:` with space and strips trailing dots
        // (Windows-share friendliness — see naming.md). Acronyms with a
        // trailing `.` lose it: "M.A.X." -> "M.A.X".
        let mut m = movie();
        m.title = "Lockout: M.A.X.";
        let p = Jellyfin.movie_path(&m);
        assert_eq!(
            p,
            Path::new(
                "/lib/Movies/Lockout M.A.X (2009) [imdbid-tt0499549]/Lockout M.A.X (2009) [imdbid-tt0499549].mkv"
            )
        );
    }
}
