// Shared Tokio runtime for backend tasks (subprocess, HTTP, async filesystem).
// Lives outside the GTK main loop; results cross back via async_channel.

use once_cell::sync::Lazy;

static TOKIO_RT: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("ripsaw-tokio")
        .build()
        .expect("build tokio runtime")
});

pub fn tokio_runtime() -> &'static tokio::runtime::Runtime {
    &TOKIO_RT
}
