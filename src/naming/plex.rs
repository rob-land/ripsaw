// Plex scheme. See docs/naming.md § "Plex".

use std::path::PathBuf;

use super::{EpisodeContext, ExtraContext, MovieContext, Scheme};

pub struct Plex;

impl Scheme for Plex {
    fn movie_path(&self, _ctx: &MovieContext) -> PathBuf {
        todo!("<root>/<Title> (<Year>) {{imdb-tt…}}/<Title> (<Year>) {{imdb-tt…}}.mkv")
    }

    fn episode_path(&self, _ctx: &EpisodeContext) -> PathBuf {
        todo!("Plex series convention with {{tvdb-…}} or {{tmdb-…}}")
    }

    fn extras_path(&self, _ctx: &ExtraContext) -> PathBuf {
        todo!("Plex extras: Behind The Scenes / Deleted Scenes / Featurettes / Interviews / Scenes / Shorts / Trailers / Other")
    }
}
