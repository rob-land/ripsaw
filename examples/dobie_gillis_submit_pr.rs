// Resubmit the Dobie Gillis staging tree with the new enrichment
// (plot, tagline, cover.jpg, tmdb.json, ImageUrl, ReleaseDate,
// DateAdded, Contributors, Groups) and push a PR via gh.
//
// Reads the existing staged disc01.json + disc02.json verbatim --
// those came from real disc scans yesterday and don't need to be
// regenerated. Re-renders metadata.json + release.json with the
// new fields and the freshly-fetched TMDB content.
//
// Run:
//   cargo run --release --example dobie_gillis_submit_pr

use std::path::PathBuf;

use anyhow::{Context, Result};

use ripsaw::identify::submission::{
    render_metadata_json, render_release_json, staging_root, title_slug,
    ContentType, MovieMetadata, ReleaseMetadata, SubmissionArtifacts,
};
use ripsaw::identify::submit_pr::{push_pr, PrPushRequest};
use ripsaw::identify::tmdb::TmdbClient;

const SERIES_TITLE: &str = "The Many Loves of Dobie Gillis";
const SERIES_YEAR: u32 = 1959;
const TMDB_ID: u64 = 221;
const RELEASE_SLUG: &str = "complete-series-boxset";
const ASIN: &str = "B00C7A8WWO";
const UPC: &str = "826663140996";

#[tokio::main]
async fn main() -> Result<()> {
    let tmdb_key = read_tmdb_key()?;
    let gh_user = read_gh_user()?;
    println!("Using TMDB key (last 4: ****{}), gh user: {}",
        &tmdb_key[tmdb_key.len() - 4..], gh_user);

    let client = TmdbClient::new(tmdb_key);

    // 1. Fetch TMDB details + raw JSON + best poster.
    let details = client.fetch_series(TMDB_ID).await
        .context("fetching TMDB series details")?;
    let raw = client.fetch_raw("tv", TMDB_ID).await
        .context("fetching TMDB raw")?;
    let images = client.fetch_images("tv", TMDB_ID).await
        .context("fetching TMDB images")?;
    let poster = images.posters.first()
        .ok_or_else(|| anyhow::anyhow!("TMDB has no posters for series {TMDB_ID}"))?;
    println!("Top poster: {}x{} vote_avg={:.2} {}",
        poster.width, poster.height, poster.vote_average, poster.file_path);
    let cover_bytes = client.download_image(&poster.file_path, "original").await
        .context("downloading TMDB poster")?;
    println!("Downloaded cover.jpg: {} bytes", cover_bytes.len());

    // 2. Build the enriched metadata + release records.
    let movie = MovieMetadata {
        title: SERIES_TITLE.to_string(),
        year: Some(SERIES_YEAR),
        content_type: ContentType::Series,
        plot: details.plot.clone(),
        tagline: details.tagline.clone(),
        tmdb_id: Some(TMDB_ID),
        imdb_id: details.imdb_id.clone(),
        tvdb_id: None,
    };

    let movie_slug = title_slug(SERIES_TITLE, Some(SERIES_YEAR));
    let image_url = format!("Series/{movie_slug}/{RELEASE_SLUG}.jpg");

    let release = ReleaseMetadata {
        slug: RELEASE_SLUG.to_string(),
        title: "The Many Loves of Dobie Gillis: The Complete Series".to_string(),
        year: Some(SERIES_YEAR),
        locale: Some("en-us".to_string()),
        region_code: Some("1".to_string()),
        upc: Some(UPC.to_string()),
        asin: Some(ASIN.to_string()),
        image_url: Some(image_url),
        // Source: blu-ray.com listing for the Shout! Factory complete-series boxset.
        release_date: Some("2013-07-02T00:00:00+00:00".to_string()),
        contributors: vec![gh_user.clone()],
        groups: vec!["Shout Factory".to_string()],
    };

    // 3. Re-write the movie + release files in place at the staging
    //    tree (disc0N.json remain untouched). Also drop cover.jpg
    //    and tmdb.json next to metadata.json per TheDiscDB
    //    convention.
    let folder_name = format!("{SERIES_TITLE} ({SERIES_YEAR})");
    let movie_dir = staging_root()
        .join("data")
        .join("series")
        .join(&folder_name);
    let release_dir = movie_dir.join(RELEASE_SLUG);
    std::fs::create_dir_all(&release_dir)?;

    let metadata_path = movie_dir.join("metadata.json");
    std::fs::write(&metadata_path, render_metadata_json(&movie))?;
    println!("Wrote {}", metadata_path.display());

    let cover_path = movie_dir.join("cover.jpg");
    std::fs::write(&cover_path, &cover_bytes)?;
    println!("Wrote {} ({} bytes)", cover_path.display(), cover_bytes.len());

    let tmdb_path = movie_dir.join("tmdb.json");
    std::fs::write(&tmdb_path, serde_json::to_string_pretty(&raw)?)?;
    println!("Wrote {}", tmdb_path.display());

    let release_path = release_dir.join("release.json");
    std::fs::write(&release_path, render_release_json(&release))?;
    println!("Wrote {}", release_path.display());

    let _ = SubmissionArtifacts::default(); // suppress unused-import warning

    if std::env::var_os("RIPSAW_DRY_RUN").is_some() {
        println!("\nRIPSAW_DRY_RUN set -- staged everything but not pushing PR.");
        println!("Re-run without RIPSAW_DRY_RUN to push.");
        return Ok(());
    }

    // 4. Open the PR.
    let data_root = staging_root().join("data");
    let subpath = std::path::PathBuf::from("series").join(&folder_name);
    let body = format!(
        "Adds {SERIES_TITLE} ({SERIES_YEAR}) S1 D1 + D2 (Shout Factory complete-series boxset).\n\n\
         - Source: real disc scans ({}) via Ripsaw\n\
         - Cover: TMDB poster (vote_avg {:.2})\n\
         - TMDB id: {TMDB_ID}\n\
         - UPC: {UPC} / ASIN: {ASIN} / Region 1\n\n\
         Submitted via Ripsaw automated PR push (https://git.rob.land/rob/ripsaw).",
        gh_user, poster.vote_average,
    );
    let pr_title = format!("Add {SERIES_TITLE} ({SERIES_YEAR}) — complete series boxset (S1 D1+D2)");

    println!("\nOpening PR against TheDiscDb/data...");
    let result = push_pr(&PrPushRequest {
        staged_data_root: &data_root,
        staged_subpath: &subpath,
        slug: &movie_slug,
        pr_title: &pr_title,
        pr_body: &body,
    })?;
    println!("PR opened: {}", result.pr_url);
    Ok(())
}

fn read_tmdb_key() -> Result<String> {
    let path = PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(".config").join("ripsaw").join("config.json");
    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&path).context("reading ripsaw config")?
    )?;
    raw.get("tmdb_api_key")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("tmdb_api_key not set in {}", path.display()))
}

fn read_gh_user() -> Result<String> {
    let out = std::process::Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .output().context("gh api user (gh auth login may be needed)")?;
    if !out.status.success() {
        anyhow::bail!("gh api user failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
