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
    source: ScanSource,
    queue: Vec<RipQueueItem>,
    progress_weak: glib::WeakRef<RipProgressPage>,
) {
    let (rip_tx, rip_rx) = async_channel::unbounded::<RipMessage>();

    // Worker on the tokio runtime: walks the queue, runs extract_title
    // for each title, relays events on rip_tx.
    crate::runtime::tokio_runtime().spawn(async move {
        for (index_in_queue, item) in queue.iter().enumerate() {
            let _ = rip_tx
                .send(RipMessage::Started(index_in_queue, item.clone()))
                .await;

            if let Err(e) = tokio::fs::create_dir_all(&item.output_dir).await {
                let _ = rip_tx
                    .send(RipMessage::Finished(
                        index_in_queue,
                        Err(anyhow::anyhow!(
                            "could not create output directory {}: {e}",
                            item.output_dir.display()
                        )),
                    ))
                    .await;
                continue;
            }

            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<ExtractEvent>(64);

            let extract_handle = {
                let source = source.clone();
                let output_dir = item.output_dir.clone();
                let filename = item.expected_output_filename.clone();
                let title_index = item.title_index;
                tokio::spawn(async move {
                    extract_title(
                        &source,
                        title_index,
                        &output_dir,
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

            let extract_result = match extract_handle.await {
                Ok(r) => r,
                Err(join_err) => Err(anyhow::anyhow!("extract task panicked: {join_err}")),
            };

            // If the planner requested a final path, rename the produced
            // file to match (creating parent directories as needed). This is
            // how Jellyfin/Plex/Kodi naming actually lands on disk.
            let final_result = match (extract_result, &item.final_path) {
                (Ok(produced), Some(final_path)) if produced != *final_path => {
                    let move_res = rename_to_final(&produced, final_path).await;
                    match move_res {
                        Ok(()) => Ok(final_path.clone()),
                        Err(e) => Err(anyhow::anyhow!(
                            "extracted to {}; rename to {} failed: {e}",
                            produced.display(),
                            final_path.display()
                        )),
                    }
                }
                (other, _) => other,
            };

            // After a successful rip, embed chapter titles + Segment.Title
            // via mkvpropedit. This is best-effort -- the metadata module
            // logs and continues on tool/format failures; we only treat
            // setup failures as fatal here.
            if let Ok(landed) = &final_result {
                if let Err(e) = crate::rip::metadata::apply_post_rip_metadata(
                    landed,
                    &item.chapter_titles,
                    item.segment_title.as_deref(),
                )
                .await
                {
                    tracing::warn!(
                        "post-rip metadata for {} failed at setup: {e:#}",
                        landed.display()
                    );
                }
            }

            // If the user requested a 3D conversion alongside the rip,
            // detect the MKV's StereoSource flavour and run the convert
            // pipeline now. We deliberately keep this as a separate phase
            // -- the rip's "Finished" message refers only to the rip; a
            // convert failure is surfaced through the rip log expander
            // rather than failing the rip retroactively.
            if let (Ok(landed), Some(format)) =
                (&final_result, item.conversion_format)
            {
                match crate::convert::plan::detect_stereo_source(landed) {
                    Some(source) => {
                        let output = crate::convert::plan::ConversionPlan::default_output_path(
                            landed, format,
                        );
                        let plan = crate::convert::plan::ConversionPlan {
                            input: landed.clone(),
                            output: output.clone(),
                            format,
                            source,
                        };
                        let _ = rip_tx
                            .send(RipMessage::Event(
                                crate::rip::makemkv::ExtractEvent::Message(
                                    crate::rip::makemkv_parse::MsgRecord {
                                        code: 0,
                                        priority: 0,
                                        text: format!(
                                            "Converting → {} ({})",
                                            format.label(),
                                            output.display()
                                        ),
                                    },
                                ),
                            ))
                            .await;
                        match crate::convert::runner::run_conversion(plan, None).await {
                            Ok(p) => {
                                let _ = rip_tx
                                    .send(RipMessage::Event(
                                        crate::rip::makemkv::ExtractEvent::Message(
                                            crate::rip::makemkv_parse::MsgRecord {
                                                code: 0,
                                                priority: 0,
                                                text: format!(
                                                    "Converted: {}",
                                                    p.display()
                                                ),
                                            },
                                        ),
                                    ))
                                    .await;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "post-rip convert for {} failed: {e:#}",
                                    landed.display()
                                );
                                let _ = rip_tx
                                    .send(RipMessage::Event(
                                        crate::rip::makemkv::ExtractEvent::Message(
                                            crate::rip::makemkv_parse::MsgRecord {
                                                code: 0,
                                                priority: 0,
                                                text: format!("Convert failed: {e}"),
                                            },
                                        ),
                                    ))
                                    .await;
                            }
                        }
                    }
                    None => {
                        tracing::info!(
                            "no MVC/stereo info in {}; skipping post-rip convert",
                            landed.display()
                        );
                    }
                }
            }

            let _ = rip_tx
                .send(RipMessage::Finished(index_in_queue, final_result))
                .await;
        }
        let _ = rip_tx.send(RipMessage::AllDone).await;
    });

    // Consumer on the GTK main thread: receives RipMessages and updates
    // the RipProgressPage. Holds only a weak ref so a closed page lets
    // this loop fall through.

    async fn rename_to_final(
        from: &std::path::Path,
        to: &std::path::Path,
    ) -> anyhow::Result<()> {
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Same-filesystem rename is atomic and cheap. If `to` is on a
        // different filesystem `rename` errors with EXDEV — fall back to
        // copy-and-remove for that case.
        match tokio::fs::rename(from, to).await {
            Ok(()) => Ok(()),
            // EXDEV = 18 on Linux. We're a Linux-only crate.
            Err(e) if e.raw_os_error() == Some(18) => {
                tokio::fs::copy(from, to).await?;
                tokio::fs::remove_file(from).await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

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
