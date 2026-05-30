// User settings: persisted at $XDG_CONFIG_HOME/ripsaw/config.json
// (`~/.config/ripsaw/config.json` typically). Plain JSON for now —
// migrating to GSettings later is mechanical once we ship a proper
// install step that registers our gschema.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::naming::Scheme;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserSettings {
    pub library_root: Option<PathBuf>,
    #[serde(default)]
    pub scheme: SchemeKind,
    #[serde(default)]
    pub sonarr: ServarrConfig,
    #[serde(default)]
    pub radarr: ServarrConfig,
    /// TMDB v3 API key. Sign up at <https://www.themoviedb.org/settings/api>.
    /// Used for the per-disc "look up by TMDB ID / IMDb ID" buttons on
    /// the submission form.
    #[serde(default)]
    pub tmdb_api_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServarrConfig {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl ServarrConfig {
    /// `true` when both fields are non-empty.
    pub fn is_configured(&self) -> bool {
        self.url
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
            && self
                .api_key
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SchemeKind {
    #[default]
    Jellyfin,
    Plex,
    Kodi,
    Emby,
}

impl SchemeKind {
    pub fn label(self) -> &'static str {
        match self {
            SchemeKind::Jellyfin => "Jellyfin",
            SchemeKind::Plex => "Plex",
            SchemeKind::Kodi => "Kodi",
            SchemeKind::Emby => "Emby",
        }
    }

    pub fn from_index(idx: u32) -> Self {
        match idx {
            0 => SchemeKind::Jellyfin,
            1 => SchemeKind::Plex,
            2 => SchemeKind::Kodi,
            _ => SchemeKind::Emby,
        }
    }

    pub fn to_index(self) -> u32 {
        match self {
            SchemeKind::Jellyfin => 0,
            SchemeKind::Plex => 1,
            SchemeKind::Kodi => 2,
            SchemeKind::Emby => 3,
        }
    }

    pub fn into_scheme(self) -> Box<dyn Scheme> {
        match self {
            SchemeKind::Jellyfin | SchemeKind::Emby => Box::new(crate::naming::jellyfin::Jellyfin),
            SchemeKind::Plex => Box::new(crate::naming::plex::Plex),
            SchemeKind::Kodi => Box::new(crate::naming::kodi::Kodi),
        }
    }
}

static SETTINGS: Lazy<Mutex<UserSettings>> = Lazy::new(|| Mutex::new(UserSettings::load()));

pub fn settings() -> &'static Mutex<UserSettings> {
    &SETTINGS
}

impl UserSettings {
    pub fn load() -> Self {
        match config_path() {
            Ok(path) => match std::fs::read_to_string(&path) {
                Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                    tracing::warn!("config at {} is malformed ({e}); using defaults", path.display());
                    Self::default()
                }),
                Err(_) => Self::default(),
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        Ok(())
    }
}

fn config_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow::anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(base.join("ripsaw").join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_index_roundtrip() {
        for kind in [
            SchemeKind::Jellyfin,
            SchemeKind::Plex,
            SchemeKind::Kodi,
            SchemeKind::Emby,
        ] {
            assert_eq!(SchemeKind::from_index(kind.to_index()), kind);
        }
    }

    #[test]
    fn default_scheme_is_jellyfin() {
        let s = UserSettings::default();
        assert_eq!(s.scheme, SchemeKind::Jellyfin);
        assert!(s.library_root.is_none());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        let mut s = UserSettings::default();
        s.scheme = SchemeKind::Plex;
        s.library_root = Some(PathBuf::from("/lib/example"));
        s.save().unwrap();

        let loaded = UserSettings::load();
        assert_eq!(loaded.scheme, SchemeKind::Plex);
        assert_eq!(loaded.library_root, Some(PathBuf::from("/lib/example")));
    }
}
