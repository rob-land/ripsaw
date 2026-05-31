// Resume the Dobie Gillis D2 rip from where it failed (T01 / E10).
// Same submission staging at the end. See examples/dobie_gillis_e2e.rs
// for the full first-run version.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;

use ripsaw::convert::format::OutputFormat;
use ripsaw::identify::pipeline::identify_physical_disc;
use ripsaw::identify::submission::{
    stage_full_submission, ContentType, DiscSubmission, MovieMetadata, ReleaseMetadata,
};
use ripsaw::identify::TitleRole;
use ripsaw::rip::makemkv::{extract_title, ExtractEvent};
use ripsaw::rip::plan::{plan_rip, DiscContentKind, NamingOpts};
use ripsaw::settings::SchemeKind;

const SERIES_TITLE: &str = "The Many Loves of Dobie Gillis";
const SERIES_YEAR: u32 = 1959;
const TMDB_ID: u64 = 221;
const RELEASE_SLUG: &str = "complete-series-boxset";
const ASIN: &str = "B00C7A8WWO";
const UPC: &str = "826663140996";

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
    // D2 mount was on sr0; identify against the mount path used by udisks2.
    let mount = PathBuf::from("/run/media/rob/DOBIEGILLIS_S1D2");
    let ident = identify_physical_disc(0, mount).await?;
    println!(
        "Re-identified D2: type {:?}, region {:?}, titles {}",
        ident.disc_type, ident.dvd_region_code, ident.scan.titles.len()
    );

    // We want to skip T00 (E09 already ripped) and rerun T01..T07.
    let episodes: Vec<&ripsaw::rip::makemkv_parse::TitleAttributes> = ident.scan.titles.iter()
        .filter(|t| t.duration_seconds.unwrap_or(0) >= 600)
        .collect();
    let selected: Vec<u32> = episodes.iter().take(8).map(|t| t.index).collect();

    let mut episode_titles_map = HashMap::new();
    let mut role_overrides = HashMap::new();
    for (slot, idx) in selected.iter().enumerate() {
        episode_titles_map.insert(*idx, D2_EPISODES[slot].to_string());
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
        episode_start: 9,
        conversion_format: None::<OutputFormat>,
        conversion_codec: ripsaw::convert::plan::ConversionPlan::default_codec(),
        conversion_hw_backend: ripsaw::convert::plan::ConversionPlan::default_hw_backend(),
    };
    let plan = plan_rip(
        &ident, &selected, Some(&opts), &episode_titles_map,
        &HashMap::new(), &role_overrides,
    );
    println!("Plan has {} titles; will skip the first (T{:02}, already ripped).",
        plan.len(), plan.first().map(|p| p.title_index).unwrap_or(0));

    // Skip the already-ripped first title; retry the rest. Between
    // titles we sleep 2 s to give the drive + kernel time to settle --
    // the back-to-back invocation in the first run hit a "Failed to
    // open disc" right after E09 finished.
    for (q_i, item) in plan.iter().enumerate().skip(1) {
        let started = Instant::now();
        // Small pause so the drive's previous makemkvcon exit doesn't
        // race with the new one opening the device.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tokio::fs::create_dir_all(&item.output_dir).await?;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ExtractEvent>(64);
        let source = ident.source.clone();
        let title_index = item.title_index;
        let output_dir = item.output_dir.clone();
        let filename = item.output_filename.clone();
        let extract = tokio::spawn(async move {
            extract_title(&source, title_index, &output_dir, &filename, Some(tx)).await
        });

        let mut last_pct = 0u8;
        let mut last_print = Instant::now();
        while let Some(ev) = rx.recv().await {
            if let ExtractEvent::Progress(p) = &ev {
                if p.max == 0 { continue; }
                let pct = ((p.total as u64 * 100) / p.max as u64) as u8;
                if pct != last_pct && (pct == 100 || last_print.elapsed().as_secs() >= 5) {
                    last_pct = pct;
                    last_print = Instant::now();
                    println!("    [Q{q_i:02} T{title_index:02} {pct:>3}%] {}",
                        p.current_label.as_deref().unwrap_or(""));
                }
            }
        }
        let produced = extract.await??;
        if let Some(final_path) = &item.final_path {
            if produced != *final_path {
                if let Some(parent) = final_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::rename(&produced, final_path).await?;
            }
            let bytes = tokio::fs::metadata(final_path).await?.len();
            println!("  [done T{:02}] {} ({:.1}s, {:.2} GB)",
                item.title_index, final_path.display(),
                started.elapsed().as_secs_f64(),
                bytes as f64 / 1_000_000_000.0,
            );
            if let Err(e) = ripsaw::rip::metadata::apply_post_rip_metadata(
                final_path, &item.chapter_titles, item.segment_title.as_deref(),
            ).await {
                eprintln!("    metadata apply warn: {e:#}");
            }
        }
    }

    // Stage D2 submission once all titles land.
    let movie = MovieMetadata {
        title: SERIES_TITLE.to_string(),
        year: Some(SERIES_YEAR),
        content_type: ContentType::Series,
        plot: None, tagline: None,
        tmdb_id: Some(TMDB_ID), imdb_id: None, tvdb_id: None,
    };
    let release = ReleaseMetadata {
        slug: RELEASE_SLUG.to_string(),
        title: "The Many Loves of Dobie Gillis: The Complete Series".to_string(),
        year: Some(SERIES_YEAR),
        locale: Some("en-us".to_string()),
        region_code: ident.dvd_region_code.clone().or_else(|| Some("1".to_string())),
        upc: Some(UPC.to_string()), asin: Some(ASIN.to_string()),
    };
    let disc_submission = DiscSubmission {
        disc_index: 2,
        disc_slug: "season-1-disc-2".to_string(),
        disc_name: "DOBIEGILLIS_S1D2".to_string(),
        format: "DVD".to_string(),
        content_hash: ident.content_hash.clone().unwrap_or_default(),
        comment: None,
    };
    let mut edits: HashMap<u32, ripsaw::ui::title_detail_page::TitleEdit> = HashMap::new();
    for (slot, idx) in selected.iter().enumerate() {
        edits.insert(*idx, ripsaw::ui::title_detail_page::TitleEdit {
            title_index: *idx,
            display_title: Some(D2_EPISODES[slot].to_string()),
            role: Some(TitleRole::Main),
            chapter_titles: Vec::new(),
        });
    }
    let staged = stage_full_submission(
        &movie, &release, &disc_submission, &ident.scan,
        ident.identities.first(), &edits,
    )?;
    println!("Staged D2 submission: {}", staged.display());
    Ok(())
}
