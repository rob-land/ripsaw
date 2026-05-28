// GSettings-backed user configuration. See docs/architecture.md.

use crate::naming::Scheme;

#[derive(Debug, Clone)]
pub struct UserSettings {
    pub default_scheme: SchemeKind,
    pub movies_root: Option<std::path::PathBuf>,
    pub shows_root: Option<std::path::PathBuf>,
    pub default_transcode_preset: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemeKind {
    Jellyfin,
    Plex,
    Kodi,
    Emby,
}

impl SchemeKind {
    pub fn into_scheme(self) -> Box<dyn Scheme> {
        todo!("instantiate the chosen scheme")
    }
}

pub fn load() -> UserSettings {
    todo!("read from gio::Settings(dev.threedrip.ThreeDrip)")
}
