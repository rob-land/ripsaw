// Sequential rip-queue orchestrator. Bridges:
//
//   tokio task pool      <-- extract_title async fn + event channel
//   |
//   v
//   async_channel        <-- RipMessages from worker to UI thread
//   |
//   v
//   glib MainContext     <-- consumed by RipProgressPage on UI thread

use std::path::PathBuf;

use gtk::glib;

use crate::rip::makemkv::{extract_title, ExtractEvent, ScanSource};
use crate::ui::rip_progress_page::{RipProgressPage, RipQueueItem};

#[derive(Debug)]
enum RipMessage {
    Started(usize, RipQueueItem),
    Event(ExtractEvent),
    Finished(usize, anyhow::Result<PathBuf>),
    AllDone,
}

/// Drive a rip queue: for each `RipQueueItem`, call extract_title with a
/// per-title progress channel, forward events to the `RipProgressPage`,
/// and continue to the next item. `progress_weak` is a weak reference so
/// the task tears itself down cleanly if the page disappears.
pub fn run_rip_queue(
    iso_path: PathBuf,
    output_root: PathBuf,
    queue: Vec<RipQueueItem>,
    progress_weak: glib::WeakRef<RipProgressPage>,
) {
    let (rip_tx, rip_rx) = async_channel::unbounded::<RipMessage>();

    // Worker on the tokio runtime: walks the queue, runs extract_title
    // for each title, relays events on rip_tx.
    crate::runtime::tokio_runtime().spawn(async move {
        if let Err(e) = tokio::fs::create_dir_all(&output_root).await {
            let _ = rip_tx
                .send(RipMessage::Finished(
                    0,
                    Err(anyhow::anyhow!(
                        "could not create output directory {}: {e}",
                        output_root.display()
                    )),
                ))
                .await;
            let _ = rip_tx.send(RipMessage::AllDone).await;
            return;
        }

        for (index_in_queue, item) in queue.iter().enumerate() {
            let _ = rip_tx
                .send(RipMessage::Started(index_in_queue, item.clone()))
                .await;

            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<ExtractEvent>(64);

            let extract_handle = {
                let iso_path = iso_path.clone();
                let output_root = output_root.clone();
                let filename = item.expected_output_filename.clone();
                let title_index = item.title_index;
                tokio::spawn(async move {
                    extract_title(
                        &ScanSource::Iso(iso_path),
                        title_index,
                        &output_root,
                        &filename,
                        Some(event_tx),
                    )
                    .await
                })
            };

            // Pump events to UI thread until the extract finishes.
            while let Some(event) = event_rx.recv().await {
                let _ = rip_tx.send(RipMessage::Event(event)).await;
            }

            let result = match extract_handle.await {
                Ok(r) => r,
                Err(join_err) => Err(anyhow::anyhow!("extract task panicked: {join_err}")),
            };
            let _ = rip_tx
                .send(RipMessage::Finished(index_in_queue, result))
                .await;
        }
        let _ = rip_tx.send(RipMessage::AllDone).await;
    });

    // Consumer on the GTK main thread: receives RipMessages and updates
    // the RipProgressPage. Holds only a weak ref so a closed page lets
    // this loop fall through.
    glib::MainContext::default().spawn_local(async move {
        while let Ok(msg) = rip_rx.recv().await {
            let Some(progress) = progress_weak.upgrade() else {
                break;
            };
            match msg {
                RipMessage::Started(i, item) => progress.mark_started(i, &item),
                RipMessage::Event(event) => progress.apply_event(&event),
                RipMessage::Finished(i, result) => {
                    if let Err(e) = &result {
                        progress.append_log(&format!("[ERR ] title {}: {e}", i));
                    } else if let Ok(p) = &result {
                        progress.append_log(&format!(
                            "[done] title {}: wrote {}",
                            i,
                            p.display()
                        ));
                    }
                    progress.mark_finished(i, result);
                }
                RipMessage::AllDone => {
                    progress.finish_queue();
                    break;
                }
            }
        }
    });
}
