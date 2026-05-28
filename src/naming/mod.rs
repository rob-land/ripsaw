// Output naming. See docs/naming.md.

pub mod jellyfin;
pub mod plex;
pub mod kodi;
pub mod emby;
pub mod extras;

use std::path::{Path, PathBuf};

use crate::identify::{TitleIdentity, TitleRole};

pub trait Scheme: Send + Sync {
    fn movie_path(&self, ctx: &MovieContext) -> PathBuf;
    fn episode_path(&self, ctx: &EpisodeContext) -> PathBuf;
    fn extras_path(&self, ctx: &ExtraContext) -> PathBuf;
}

#[derive(Debug, Clone)]
pub struct MovieContext<'a> {
    pub root: &'a Path,
    pub title: &'a str,
    pub year: Option<u32>,
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<&'a str>,
    pub variant: Option<MovieVariant>, // 4K / 3D / extended cut, etc.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovieVariant {
    Uhd4k,
    Stereo3d,
    Extended,
    Theatrical,
    Directors,
}

#[derive(Debug, Clone)]
pub struct EpisodeContext<'a> {
    pub root: &'a Path,
    pub series_title: &'a str,
    pub series_year: Option<u32>,
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<&'a str>,
    pub tvdb_id: Option<u64>,
    pub season: u32,
    pub episode: u32,
    pub episode_end: Option<u32>, // for multi-episode files
    pub episode_title: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct ExtraContext<'a> {
    pub movie: &'a MovieContext<'a>,
    pub title: &'a TitleIdentity,
    pub role: TitleRole,
}

pub fn sanitise(name: &str) -> String {
    let illegal: &[char] = &[':', '?', '*', '"', '<', '>', '|', '/', '\\', '\0'];
    let cleaned: String = name
        .chars()
        .map(|c| if illegal.contains(&c) || c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_end_matches(['.', ' ']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_strips_path_separators_and_collapses_ws() {
        assert_eq!(sanitise("Avatar:  The Way of Water"), "Avatar The Way of Water");
        assert_eq!(sanitise("M.A.S.H. "), "M.A.S.H");
        assert_eq!(sanitise("a/b"), "a b");
    }
}
