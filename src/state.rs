// Process-wide state for things that can't comfortably live on a single
// widget — currently just the set of MountedIso instances that need to
// be unmounted on application shutdown.

use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::rip::iso_mount::MountedIso;

static MOUNTS: Lazy<Mutex<Vec<MountedIso>>> = Lazy::new(|| Mutex::new(Vec::new()));

pub fn track_mount(m: MountedIso) {
    MOUNTS.lock().expect("mounts mutex").push(m);
}

/// Drain the tracked mounts and unmount each (best-effort). Called from
/// the Application's `shutdown` handler on the GTK main thread; blocks
/// the main loop briefly so the loops actually get cleaned up before
/// the process exits.
pub async fn cleanup_mounts() {
    let mounts: Vec<MountedIso> = {
        let mut guard = MOUNTS.lock().expect("mounts mutex");
        std::mem::take(&mut *guard)
    };
    for m in mounts {
        if let Err(e) = m.unmount().await {
            tracing::warn!(
                "failed to unmount {}: {e}",
                m.loop_device.display()
            );
        }
    }
}
