// Jellyfin scheme. See docs/naming.md § "Jellyfin".

use std::path::PathBuf;

use super::{EpisodeContext, ExtraContext, MovieContext, Scheme, sanitise};

pub struct Jellyfin;

impl Scheme for Jellyfin {
    fn movie_path(&self, _ctx: &MovieContext) -> PathBuf {
        todo!("<root>/<Title> (<Year>) [imdbid-tt…]/<Title> (<Year>).mkv with variant suffix")
    }

    fn episode_path(&self, _ctx: &EpisodeContext) -> PathBuf {
        todo!("<root>/<Series> (<Year>) [imdbid-…]/Season <NN>/<Series> (<Year>) S<NN>E<NN>.mkv")
    }

    fn extras_path(&self, _ctx: &ExtraContext) -> PathBuf {
        todo!("movie folder + extras subdir from extras::folder_for_role()")
    }
}

#[allow(dead_code)]
fn movie_folder_name(title: &str, year: Option<u32>, imdb: Option<&str>) -> String {
    let mut s = sanitise(title);
    if let Some(y) = year { s.push_str(&format!(" ({y})")); }
    if let Some(imdb) = imdb { s.push_str(&format!(" [imdbid-{imdb}]")); }
    s
}
