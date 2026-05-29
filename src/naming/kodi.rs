// Kodi scheme. See docs/naming.md § "Kodi".

use std::path::PathBuf;

use crate::identify::TitleRole;

use super::{sanitise, EpisodeContext, ExtraContext, MovieContext, MovieVariant, Scheme};

pub struct Kodi;

impl Scheme for Kodi {
    fn movie_path(&self, ctx: &MovieContext) -> PathBuf {
        let folder = movie_folder_name(ctx);
        let mut basename = folder.clone();
        if let Some(s) = variant_suffix(ctx.variant) {
            basename.push_str(s);
        }
        basename.push_str(".mkv");
        ctx.root.join(folder).join(basename)
    }

    fn episode_path(&self, ctx: &EpisodeContext) -> PathBuf {
        let folder = series_folder_name(ctx);
        let season_folder = format!("Season {:02}", ctx.season);
        let ep_part = match ctx.episode_end {
            Some(end) => format!("S{:02}E{:02}-E{:02}", ctx.season, ctx.episode, end),
            None => format!("S{:02}E{:02}", ctx.season, ctx.episode),
        };
        let title = sanitise(ctx.series_title);
        let episode_part = ctx
            .episode_title
            .map(|t| sanitise(t))
            .filter(|t| !t.is_empty())
            .map(|t| format!(" {t}"))
            .unwrap_or_default();
        let filename = format!("{title} {ep_part}{episode_part}.mkv");
        ctx.root.join(folder).join(season_folder).join(filename)
    }

    fn extras_path(&self, ctx: &ExtraContext) -> PathBuf {
        let movie_folder = movie_folder_name(ctx.movie);
        let path = ctx.movie.root.join(&movie_folder);
        match ctx.role {
            TitleRole::Trailer => {
                // Kodi recognises `<MovieName>-trailer.<ext>` inline in the movie folder.
                // Multiple trailers fall to conflict resolution at the caller.
                let basename = format!("{movie_folder}-trailer.mkv");
                path.join(basename)
            }
            TitleRole::Main => path.join(format!("{movie_folder}.mkv")),
            _ => path.join("extras").join(format!("{}.mkv", sanitise(&ctx.title.display_title))),
        }
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

/// Kodi (Omega / v21+) filename identifier: " {tmdb=N}" or
/// " {imdb=ttN}". Uses the equals-sign syntax documented at
/// kodi.wiki/view/Naming_video_files/Movies § "Filename
/// identifiers". TMDb is preferred when present; IMDb is the
/// fallback (Kodi recommends using a scraper's native ID).
fn movie_id_bracket(ctx: &MovieContext) -> Option<String> {
    if let Some(tmdb) = ctx.tmdb_id {
        Some(format!(" {{tmdb={tmdb}}}"))
    } else {
        ctx.imdb_id.map(|imdb| format!(" {{imdb={imdb}}}"))
    }
}

fn series_folder_name(ctx: &EpisodeContext) -> String {
    let mut s = sanitise(ctx.series_title);
    if let Some(y) = ctx.series_year {
        s.push_str(&format!(" ({y})"));
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
    fn movie_path_uses_imdb_bracket_in_folder_and_filename() {
        // Kodi v21+ filename identifiers: `{tmdb=N}` / `{imdb=ttN}`.
        // TMDb is preferred when present; this fixture only has IMDb,
        // so the IMDb form lands.
        let p = Kodi.movie_path(&movie());
        assert_eq!(
            p,
            Path::new(
                "/lib/Movies/Avatar (2009) {imdb=tt0499549}/Avatar (2009) {imdb=tt0499549}.mkv"
            )
        );
    }

    #[test]
    fn movie_path_prefers_tmdb_when_both_present() {
        let mut m = movie();
        m.tmdb_id = Some(19995);
        let p = Kodi.movie_path(&m);
        assert_eq!(
            p,
            Path::new("/lib/Movies/Avatar (2009) {tmdb=19995}/Avatar (2009) {tmdb=19995}.mkv")
        );
    }

    #[test]
    fn movie_path_drops_id_section_when_none_known() {
        let mut m = movie();
        m.imdb_id = None;
        let p = Kodi.movie_path(&m);
        assert_eq!(p, Path::new("/lib/Movies/Avatar (2009)/Avatar (2009).mkv"));
    }

    #[test]
    fn movie_3d_variant_suffix_in_filename() {
        let mut m = movie();
        m.variant = Some(MovieVariant::Stereo3d);
        let p = Kodi.movie_path(&m);
        assert_eq!(
            p,
            Path::new(
                "/lib/Movies/Avatar (2009) {imdb=tt0499549}/Avatar (2009) {imdb=tt0499549} - 3D.mkv"
            )
        );
    }

    #[test]
    fn trailer_goes_inline_with_trailer_suffix() {
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
        let p = Kodi.extras_path(&ctx);
        assert_eq!(
            p,
            Path::new(
                "/lib/Movies/Avatar (2009) {imdb=tt0499549}/Avatar (2009) {imdb=tt0499549}-trailer.mkv"
            )
        );
    }

    #[test]
    fn non_trailer_extras_go_into_extras_subfolder() {
        let title = TitleIdentity {
            index: 3,
            role: TitleRole::BehindTheScenes,
            display_title: "Making of Avatar".into(),
            source_file: None,
            season: None,
            episode: None,
        };
        let m = movie();
        let ctx = ExtraContext { movie: &m, title: &title, role: TitleRole::BehindTheScenes };
        let p = Kodi.extras_path(&ctx);
        assert_eq!(
            p,
            Path::new("/lib/Movies/Avatar (2009) {imdb=tt0499549}/extras/Making of Avatar.mkv")
        );
    }

    #[test]
    fn episode_path_uses_capital_sxxeyy() {
        let ctx = EpisodeContext {
            root: Path::new("/lib/Shows"),
            series_title: "The Expanse",
            series_year: Some(2015),
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            season: 1,
            episode: 3,
            episode_end: None,
            episode_title: None,
        };
        let p = Kodi.episode_path(&ctx);
        assert_eq!(p, Path::new("/lib/Shows/The Expanse (2015)/Season 01/The Expanse S01E03.mkv"));
    }

    #[test]
    fn episode_path_appends_title_after_episode_marker() {
        let ctx = EpisodeContext {
            root: Path::new("/lib/Shows"),
            series_title: "The Expanse",
            series_year: Some(2015),
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            season: 1,
            episode: 3,
            episode_end: None,
            episode_title: Some("Remember the Cant"),
        };
        let p = Kodi.episode_path(&ctx);
        assert_eq!(
            p,
            Path::new("/lib/Shows/The Expanse (2015)/Season 01/The Expanse S01E03 Remember the Cant.mkv")
        );
    }
}
