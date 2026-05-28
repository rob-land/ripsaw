// Plex scheme. See docs/naming.md § "Plex".

use std::path::PathBuf;

use crate::settings::SchemeKind;

use super::{extras, sanitise, EpisodeContext, ExtraContext, MovieContext, MovieVariant, Scheme};

pub struct Plex;

impl Scheme for Plex {
    fn movie_path(&self, ctx: &MovieContext) -> PathBuf {
        let basename = movie_basename_with_ids(ctx);
        let folder = movie_folder_name(ctx);
        let mut filename = basename;
        if let Some(s) = variant_suffix(ctx.variant) {
            // Plex's multi-version suffix goes at end of filename, after the ID braces.
            filename.push_str(s);
        }
        filename.push_str(".mkv");
        ctx.root.join(folder).join(filename)
    }

    fn episode_path(&self, ctx: &EpisodeContext) -> PathBuf {
        let folder = series_folder_name(ctx);
        let season_folder = format!("Season {:02}", ctx.season);
        let ep_part = match ctx.episode_end {
            Some(end) => format!("s{:02}e{:02}-e{:02}", ctx.season, ctx.episode, end),
            None => format!("s{:02}e{:02}", ctx.season, ctx.episode),
        };
        let title = sanitise(ctx.series_title);
        let year_part = ctx.series_year.map(|y| format!(" ({y})")).unwrap_or_default();
        let filename = format!("{title}{year_part} - {ep_part}.mkv");
        ctx.root.join(folder).join(season_folder).join(filename)
    }

    fn extras_path(&self, ctx: &ExtraContext) -> PathBuf {
        let folder = movie_folder_name(ctx.movie);
        let sub = extras::folder_for_role(SchemeKind::Plex, ctx.role);
        let filename = format!("{}.mkv", sanitise(&ctx.title.display_title));
        let mut path = ctx.movie.root.join(folder);
        if !sub.is_empty() {
            path = path.join(sub);
        }
        path.join(filename)
    }
}

/// `Avatar (2009) {imdb-tt0499549}` — Plex's folder format with brace-style IDs.
fn movie_folder_name(ctx: &MovieContext) -> String {
    movie_basename_with_ids(ctx)
}

/// Same as the folder name. Plex repeats the title+year+ids in the filename.
fn movie_basename_with_ids(ctx: &MovieContext) -> String {
    let mut s = sanitise(ctx.title);
    if let Some(y) = ctx.year {
        s.push_str(&format!(" ({y})"));
    }
    if let Some(id) = id_segment(ctx) {
        s.push(' ');
        s.push_str(&id);
    }
    s
}

fn id_segment(ctx: &MovieContext) -> Option<String> {
    if let Some(imdb) = ctx.imdb_id {
        Some(format!("{{imdb-{imdb}}}"))
    } else {
        ctx.tmdb_id.map(|t| format!("{{tmdb-{t}}}"))
    }
}

fn series_folder_name(ctx: &EpisodeContext) -> String {
    let mut s = sanitise(ctx.series_title);
    if let Some(y) = ctx.series_year {
        s.push_str(&format!(" ({y})"));
    }
    if let Some(id) = series_id_segment(ctx) {
        s.push(' ');
        s.push_str(&id);
    }
    s
}

fn series_id_segment(ctx: &EpisodeContext) -> Option<String> {
    if let Some(tvdb) = ctx.tvdb_id {
        Some(format!("{{tvdb-{tvdb}}}"))
    } else if let Some(imdb) = ctx.imdb_id {
        Some(format!("{{imdb-{imdb}}}"))
    } else {
        ctx.tmdb_id.map(|t| format!("{{tmdb-{t}}}"))
    }
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
    fn movie_folder_and_file_repeat_ids_in_brace_form() {
        let p = Plex.movie_path(&movie());
        assert_eq!(
            p,
            Path::new("/lib/Movies/Avatar (2009) {imdb-tt0499549}/Avatar (2009) {imdb-tt0499549}.mkv")
        );
    }

    #[test]
    fn movie_3d_variant_suffix_after_ids() {
        let mut m = movie();
        m.variant = Some(MovieVariant::Stereo3d);
        let p = Plex.movie_path(&m);
        assert_eq!(
            p,
            Path::new("/lib/Movies/Avatar (2009) {imdb-tt0499549}/Avatar (2009) {imdb-tt0499549} - 3D.mkv")
        );
    }

    #[test]
    fn movie_falls_back_to_tmdb_brace() {
        let mut m = movie();
        m.imdb_id = None;
        m.tmdb_id = Some(19995);
        let p = Plex.movie_path(&m);
        assert_eq!(p, Path::new("/lib/Movies/Avatar (2009) {tmdb-19995}/Avatar (2009) {tmdb-19995}.mkv"));
    }

    #[test]
    fn episode_path_uses_lowercase_sxxeyy_with_hyphen() {
        let ctx = EpisodeContext {
            root: Path::new("/lib/Shows"),
            series_title: "The Expanse",
            series_year: Some(2015),
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: Some(280619),
            season: 1,
            episode: 3,
            episode_end: None,
            episode_title: None,
        };
        let p = Plex.episode_path(&ctx);
        assert_eq!(
            p,
            Path::new(
                "/lib/Shows/The Expanse (2015) {tvdb-280619}/Season 01/The Expanse (2015) - s01e03.mkv"
            )
        );
    }

    #[test]
    fn extras_path_uses_title_case_subfolders() {
        let title = TitleIdentity {
            index: 2,
            role: TitleRole::BehindTheScenes,
            display_title: "Making of Avatar".into(),
            season: None,
            episode: None,
        };
        let m = movie();
        let ctx = ExtraContext { movie: &m, title: &title, role: TitleRole::BehindTheScenes };
        let p = Plex.extras_path(&ctx);
        assert_eq!(
            p,
            Path::new(
                "/lib/Movies/Avatar (2009) {imdb-tt0499549}/Behind The Scenes/Making of Avatar.mkv"
            )
        );
    }
}
