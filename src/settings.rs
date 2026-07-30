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
    /// Encoder backend for the 3D convert step. Global preference (the
    /// per-disc selector was removed). `None` means "Auto" — probe for a
    /// hardware encoder, fall back to software.
    #[serde(default)]
    pub conversion_hw_backend: Option<crate::convert::hw::HwBackend>,
    /// Video codec for the 3D convert step. Global preference. `None` means
    /// the default (H.264) — broadest player compatibility, notably for the
    /// Xreal Beam Pro and older TVs. H.265 roughly halves file size.
    #[serde(default)]
    pub conversion_codec: Option<crate::convert::hw::EncodeCodec>,
    /// Quality target for the convert step (Higher / Balanced / Smaller).
    /// `None` = Balanced. Maps per encoder to a good CRF/QP/global-quality.
    #[serde(default)]
    pub conversion_quality_preset: Option<crate::convert::hw::QualityPreset>,
    /// Power-user escape hatch: an explicit CRF / QP / global-quality number
    /// that overrides the preset entirely. Not shown in the GUI — set it by
    /// hand in `config.json`. Interpreted directly by the chosen encoder
    /// (lower = better), so its meaning depends on codec + backend.
    #[serde(default)]
    pub conversion_quality_override: Option<u32>,
    /// Optional override for the local TheDiscDB mirror root. `None` uses
    /// the default under `$XDG_CACHE_HOME/ripsaw/thediscdb`. See
    /// `thediscdb_mirror_root`.
    #[serde(default)]
    pub thediscdb_mirror: Option<PathBuf>,
}

impl UserSettings {
    /// Resolved encoder backend, defaulting to `Auto` when unset.
    pub fn conversion_hw_backend(&self) -> crate::convert::hw::HwBackend {
        self.conversion_hw_backend
            .unwrap_or(crate::convert::hw::HwBackend::Auto)
    }

    /// Resolved output codec, defaulting to H.265 when unset — roughly half the
    /// file size at the same quality, and played by current 3D targets (e.g. the
    /// Xreal Beam Pro). Users needing maximum compatibility can pick H.264.
    pub fn conversion_codec(&self) -> crate::convert::hw::EncodeCodec {
        self.conversion_codec
            .unwrap_or(crate::convert::hw::EncodeCodec::H265)
    }

    /// Resolved quality preset, defaulting to Balanced when unset.
    pub fn conversion_quality_preset(&self) -> crate::convert::hw::QualityPreset {
        self.conversion_quality_preset.unwrap_or_default()
    }
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

/// Root of the local TheDiscDB mirror (a sync of `TheDiscDb/data`). The
/// per-user setting overrides; otherwise default to
/// `$XDG_CACHE_HOME/ripsaw/thediscdb` (`~/.cache/ripsaw/thediscdb`). The
/// mirror's `data/` tree lives directly under this root.
pub fn thediscdb_mirror_root() -> PathBuf {
    if let Some(custom) = settings()
        .lock()
        .ok()
        .and_then(|g| g.thediscdb_mirror.clone())
        .filter(|p| !p.as_os_str().is_empty())
    {
        return custom;
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("ripsaw").join("thediscdb")
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
