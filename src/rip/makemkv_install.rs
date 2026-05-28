// "MakeMKV not installed / out of date" helper. See docs/rip.md § "Setup Required page".

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPath {
    UpstreamSource, // download + build from www.makemkv.com/download/
    DistroPackage,  // dnf/apt/pacman wrappers
    Flatpak,        // flathub
}

#[derive(Debug, Clone)]
pub struct DistroHint {
    pub id: String,            // /etc/os-release ID
    pub package_command: String,
}

pub fn detect_distro() -> Option<DistroHint> {
    todo!("parse /etc/os-release, map to package manager invocation")
}

pub async fn install(_path: InstallPath) -> anyhow::Result<()> {
    todo!("orchestrate the install path; never call sudo directly, use pkexec")
}

pub fn write_beta_key(_key: &str) -> anyhow::Result<()> {
    todo!("write app_Key to ~/.MakeMKV/settings.conf, preserving other keys")
}
