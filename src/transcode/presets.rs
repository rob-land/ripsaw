// Preset loading. See docs/transcode.md.

use super::Preset;

pub fn builtin() -> Vec<Preset> {
    todo!("load TOML files from data/presets/ via include_str! + toml::from_str")
}

pub fn user_dir() -> std::path::PathBuf {
    todo!("dirs::data_dir().join('threedrip').join('presets')")
}

pub fn load_user() -> anyhow::Result<Vec<Preset>> {
    todo!("walk user_dir(), parse each TOML")
}
