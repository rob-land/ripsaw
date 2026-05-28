// Kodi scheme. See docs/naming.md § "Kodi".

use std::path::PathBuf;

use super::{EpisodeContext, ExtraContext, MovieContext, Scheme};

pub struct Kodi;

impl Scheme for Kodi {
    fn movie_path(&self, _ctx: &MovieContext) -> PathBuf {
        todo!("<root>/<Title> (<Year>)/<Title> (<Year>).mkv; .nfo sidecar with IDs written separately")
    }

    fn episode_path(&self, _ctx: &EpisodeContext) -> PathBuf {
        todo!("<root>/<Series> (<Year>)/Season <NN>/<Series> SxxEyy.mkv")
    }

    fn extras_path(&self, _ctx: &ExtraContext) -> PathBuf {
        todo!("trailer -> '<Title>-trailer.mkv' inline; other extras -> 'extras/' subfolder")
    }
}
