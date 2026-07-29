//! Spawn external tools that live on the host, not in the runtime.
//!
//! ripsaw drives a pile of host binaries — `makemkvcon`, `udisksctl`, ffmpeg,
//! mkvtoolnix — none of which exist inside a Flatpak sandbox. There they must be
//! run via `flatpak-spawn --host` (the manifest grants
//! `--talk-name=org.freedesktop.Flatpak` for exactly this). `flatpak-spawn`
//! forwards stdin/stdout/stderr, so even the decode→ffmpeg pipe works across the
//! boundary. Outside a sandbox this is a plain spawn.
//!
//! Every call site builds its command through [`host_command`] (async, the
//! common case) or [`host_command_std`] (the sync HW-encoder smoke test), then
//! adds arguments as usual — they land after the program name either way.

/// True when running inside a Flatpak sandbox.
pub fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// A [`tokio::process::Command`] for host program `prog`, wrapped in
/// `flatpak-spawn --host` when sandboxed.
pub fn host_command(prog: &str) -> tokio::process::Command {
    if in_flatpak() {
        let mut c = tokio::process::Command::new("flatpak-spawn");
        c.arg("--host").arg(prog);
        c
    } else {
        tokio::process::Command::new(prog)
    }
}

/// The synchronous [`std::process::Command`] equivalent of [`host_command`].
pub fn host_command_std(prog: &str) -> std::process::Command {
    if in_flatpak() {
        let mut c = std::process::Command::new("flatpak-spawn");
        c.arg("--host").arg(prog);
        c
    } else {
        std::process::Command::new(prog)
    }
}
