// End-to-end "unknown-disc to ripped-Jellyfin-layout + TheDiscDB
// submission" run for the Dobie Gillis Complete Series boxset, S1 D1
// and D2. Mirrors what the GUI does on those two discs:
//
//   1. Probe the optical drives via the same drive::detect_mounted_*
//      helper the window uses.
//   2. For each mounted disc, run identify_physical_disc -- exercising
//      the makemkvcon scan + UDF mount walk + content-hash + DVD region
//      pre-fill + TheDiscDB lookup (which will miss for this boxset).
//   3. Build the same NamingOpts the title-list page builds when the
//      user toggles Series mode and types a TMDb ID, with episode
//      titles fetched from TMDB.
//   4. Pick the 8 long-form titles per disc (auto-detected) and run
//      plan_rip + extract_title for each, renaming to the Jellyfin path
//      the planner computed.
//   5. Stage TheDiscDB metadata.json + release.json + disc0N.json.
//
// Run:
//   cargo run --release --example dobie_gillis_e2e

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};

use ripsaw::convert::format::OutputFormat;
use ripsaw::identify::pipeline::{identify_physical_disc, IdentificationResult};
use ripsaw::identify::submission::{
    stage_full_submission, ContentType, DiscSubmission, MovieMetadata, ReleaseMetadata,
};
use ripsaw::identify::TitleRole;
use ripsaw::rip::drive::detect_mounted_optical_discs;
use ripsaw::rip::makemkv::{extract_title, ExtractEvent};
use ripsaw::rip::makemkv_parse::TitleAttributes;
use ripsaw::rip::plan::{auto_detect_content_kind, plan_rip, DiscContentKind, NamingOpts};
use ripsaw::settings::SchemeKind;

const SERIES_TITLE: &str = "The Many Loves of Dobie Gillis";
const SERIES_YEAR: u32 = 1959;
const TMDB_ID: u64 = 221;
const RELEASE_SLUG: &str = "complete-series-boxset";
const ASIN: &str = "B00C7A8WWO";
const UPC: &str = "826663140996";

// TMDB-sourced episode titles for season 1 (8 per disc).
const D1_EPISODES: [&str; 8] = [
    "Caper at the Bijou",
    "The Best Dressed Man",
    "Love is a Science",
    "The Right Triangle",
    "Maynard's Farewell to the Troops",
    "The Sweet Singer of Central High",
    "Greater Love Hath No Man",
    "The Old Goat",
];
const D2_EPISODES: [&str; 8] = [
    "Dobie Gillis: Boy Actor",
    "It Takes Two",
    "Dobie's Birthday Party",
    "Deck the Halls",
    "Couchville, USA",
    "The Gaucho",
    "The Smoke-Filled Room",
    "The Fist Fighter",
];

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let library_root = PathBuf::from(std::env::var_os("HOME").unwrap()).join("Videos");
    println!("Library root: {}", library_root.display());

    let drives = detect_mounted_optical_discs()?;
    if drives.is_empty() {
        anyhow::bail!("no optical drives mounted -- load the discs first");
    }
    println!("Detected {} optical drives:", drives.len());
    for d in &drives {
        println!(
            "  {} -> {} (label {:?})",
            d.device.display(),
            d.mount_path.display(),
            d.label.as_deref().unwrap_or("(none)")
        );
    }

    // Sort by disc number from the label suffix so we run D1 before D2
    // regardless of which /dev/srN happens to hold which platter.
    let mut sorted = drives.clone();
    sorted.sort_by_key(|d| {
        d.label
            .as_deref()
            .and_then(|s| s.rsplit_once('D'))
            .and_then(|(_, n)| n.parse::<u32>().ok())
            .unwrap_or(0)
    });

    for (i, drive) in sorted.iter().enumerate() {
        let episode_start = 1 + (i as u32 * 8);
        let episode_titles: &[&str] = if i == 0 { &D1_EPISODES } else { &D2_EPISODES };
        let disc_label = drive
            .label
            .as_deref()
            .unwrap_or("DOBIEGILLIS");
        println!("\n==== Disc {} ({}): start E{:02} ====",
            i + 1, disc_label, episode_start);

        let started = Instant::now();
        let ident = identify_physical_disc(drive.disc_index, drive.mount_path.clone())
            .await
            .with_context(|| format!("identify {}", drive.device.display()))?;

        report_identify(&ident, drive.label.as_deref());

        let episodes = pick_episode_titles(&ident.scan.titles);
        if episodes.len() < episode_titles.len() {
            anyhow::bail!(
                "expected at least {} episode-length titles on disc {}, found {}",
                episode_titles.len(),
                i + 1,
                episodes.len()
            );
        }
        let selected_indexes: Vec<u32> = episodes.iter().take(episode_titles.len())
            .map(|t| t.index).collect();

        let mut episode_titles_map = HashMap::new();
        for (slot, idx) in selected_indexes.iter().enumerate() {
            episode_titles_map.insert(*idx, episode_titles[slot].to_string());
        }
        let display_overrides = HashMap::new();
        let mut role_overrides = HashMap::new();
        for idx in &selected_indexes {
            role_overrides.insert(*idx, TitleRole::Main);
        }

        let opts = NamingOpts {
            library_root: library_root.clone(),
            scheme: SchemeKind::Jellyfin,
            content_kind: DiscContentKind::Series,
            disc_title: SERIES_TITLE.to_string(),
            disc_year: Some(SERIES_YEAR),
            tmdb_id: Some(TMDB_ID),
            imdb_id: None,
            season: 1,
            episode_start,
            conversion_format: None::<OutputFormat>,
            conversion_codec: ripsaw::convert::plan::ConversionPlan::default_codec(),
            conversion_hw_backend: ripsaw::convert::plan::ConversionPlan::default_hw_backend(),
        };

        let plan = plan_rip(
            &ident,
            &selected_indexes,
            Some(&opts),
            &episode_titles_map,
            &display_overrides,
            &role_overrides,
        );
        println!("\nPlanned {} episode rips:", plan.len());
        for p in &plan {
            println!(
                "  T{:02} -> {}",
                p.title_index,
                p.final_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
        }

        // Execute the rips sequentially. The orchestrator in the GUI
        // does exactly this; we replicate the loop here so the
        // example doesn't need GTK.
        for (q_i, item) in plan.iter().enumerate() {
            let title_started = Instant::now();
            tokio::fs::create_dir_all(&item.output_dir).await?;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ExtractEvent>(64);
            let source = ident.source.clone();
            let title_index = item.title_index;
            let output_dir = item.output_dir.clone();
            let filename = item.output_filename.clone();
            let extract = tokio::spawn(async move {
                extract_title(&source, title_index, &output_dir, &filename, Some(tx)).await
            });

            // Drain events with periodic progress prints (every ~5s).
            let mut last_print = Instant::now();
            let mut last_pct = 0u8;
            while let Some(ev) = rx.recv().await {
                if let ExtractEvent::Progress(p) = &ev {
                    if p.max == 0 { continue; }
                    let pct = ((p.total as u64 * 100) / p.max as u64) as u8;
                    if pct != last_pct
                        && (pct == 100 || last_print.elapsed().as_secs() >= 5)
                    {
                        last_pct = pct;
                        last_print = Instant::now();
                        println!(
                            "    [T{:02} {:>3}%] {}",
                            title_index, pct, p.current_label.as_deref().unwrap_or("")
                        );
                    }
                }
            }
            let extracted = extract.await??;
            // Rename to the planner's final_path.
            if let Some(final_path) = &item.final_path {
                if extracted != *final_path {
                    if let Some(parent) = final_path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    match tokio::fs::rename(&extracted, final_path).await {
                        Ok(()) => {}
                        Err(e) if e.raw_os_error() == Some(18) => {
                            tokio::fs::copy(&extracted, final_path).await?;
                            tokio::fs::remove_file(&extracted).await?;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                println!(
                    "  [done T{:02}] {} ({:.1}s, {:.1} GB)",
                    item.title_index,
                    final_path.display(),
                    title_started.elapsed().as_secs_f64(),
                    file_gb(final_path).await,
                );
                // Apply post-rip Segment.Title + chapter titles, same
                // way the orchestrator does.
                if let Err(e) = ripsaw::rip::metadata::apply_post_rip_metadata(
                    final_path,
                    &item.chapter_titles,
                    item.segment_title.as_deref(),
                ).await {
                    eprintln!("    metadata apply warn: {e:#}");
                }
            }
        }
        println!("Disc {} done in {:.0}s", i + 1, started.elapsed().as_secs_f64());

        // Stage TheDiscDB submission for this disc. We re-use the same
        // helper the GUI calls when the user hits Submit corrections.
        let movie = MovieMetadata {
            title: SERIES_TITLE.to_string(),
            year: Some(SERIES_YEAR),
            content_type: ContentType::Series,
            plot: None,
            tagline: None,
            tmdb_id: Some(TMDB_ID),
            imdb_id: None,
            tvdb_id: None,
        };
        let release = ReleaseMetadata {
            slug: RELEASE_SLUG.to_string(),
            title: "The Many Loves of Dobie Gillis: The Complete Series".to_string(),
            year: Some(SERIES_YEAR),
            locale: Some("en-us".to_string()),
            region_code: ident.dvd_region_code.clone().or_else(|| Some("1".to_string())),
            upc: Some(UPC.to_string()),
            asin: Some(ASIN.to_string()),
            ..Default::default()
        };
        let disc_submission = DiscSubmission {
            disc_index: (i as u32) + 1,
            disc_slug: format!("season-1-disc-{}", i + 1),
            disc_name: disc_label.to_string(),
            format: "DVD".to_string(),
            content_hash: ident.content_hash.clone().unwrap_or_default(),
            comment: None,
        };
        let mut edits: HashMap<u32, ripsaw::ui::title_detail_page::TitleEdit> = HashMap::new();
        for (slot, idx) in selected_indexes.iter().enumerate() {
            edits.insert(*idx, ripsaw::ui::title_detail_page::TitleEdit {
                title_index: *idx,
                display_title: Some(episode_titles[slot].to_string()),
                role: Some(TitleRole::Main),
                chapter_titles: Vec::new(),
            });
        }
        let staged = stage_full_submission(
            &movie,
            &release,
            &disc_submission,
            &ident.scan,
            ident.identities.first(),
            &edits,
        )?;
        println!("Staged submission: {}", staged.display());
    }
    Ok(())
}

fn report_identify(r: &IdentificationResult, label: Option<&str>) {
    println!("  disc-type: {:?}", r.disc_type);
    println!("  label: {:?}", label);
    println!("  content_hash: {:?}", r.content_hash);
    println!("  TheDiscDB matches: {}", r.identities.len());
    println!("  region code: {:?}", r.dvd_region_code);
    println!("  titles: {}", r.scan.titles.len());
    let kind = auto_detect_content_kind(&r.scan.titles);
    println!("  auto-detected kind: {:?}", kind);
    for t in &r.scan.titles {
        let dur = t.duration_seconds.unwrap_or(0);
        let mins = dur / 60;
        let secs = dur % 60;
        println!(
            "    T{:02} {}:{:02} {}",
            t.index,
            mins,
            secs,
            t.source_file.as_deref().unwrap_or("")
        );
    }
}

fn pick_episode_titles(titles: &[TitleAttributes]) -> Vec<&TitleAttributes> {
    // 8 episodes are the 8 longest titles above 10 minutes.
    let mut candidates: Vec<&TitleAttributes> = titles
        .iter()
        .filter(|t| t.duration_seconds.unwrap_or(0) >= 600)
        .collect();
    candidates.sort_by_key(|t| t.index);
    candidates
}

async fn file_gb(p: &std::path::Path) -> f64 {
    tokio::fs::metadata(p)
        .await
        .map(|m| m.len() as f64 / 1_000_000_000.0)
        .unwrap_or(0.0)
}
