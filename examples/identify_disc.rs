// Quick disc-identification round-trip:
//   1. enumerate the BDMV/STREAM (or VIDEO_TS) files at a mount path
//   2. compute TheDiscDB content hash
//   3. query TheDiscDB GraphQL and print any matches
//
// Usage:
//   cargo run --release --example identify_disc -- /run/media/$USER/<label>

use std::path::PathBuf;

use ripsaw::identify::disc_hash::{content_hash, enumerate_disc_files};
use ripsaw::identify::thediscdb::TheDiscDbClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mount = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: identify_disc <mount-path>"))?;
    let mount = PathBuf::from(mount);

    let files = enumerate_disc_files(&mount)?;
    println!("disc files at {} ({} entries):", mount.display(), files.len());
    let mut total: u64 = 0;
    for f in &files {
        println!("  [{:>3}] {:>14}  {}", f.index, f.size, f.name);
        total = total.saturating_add(f.size);
    }
    println!("total bytes: {}", total);

    let hash = content_hash(&files);
    println!("content_hash: {}", hash);

    let client = TheDiscDbClient::with_default_endpoint()?;
    println!("querying TheDiscDB…");
    let identities = client.lookup_by_hash(&hash).await?;
    if identities.is_empty() {
        println!("no match");
    } else {
        for (i, id) in identities.iter().enumerate() {
            println!("--- match #{} ---", i);
            println!("{:#?}", id);
        }
    }
    Ok(())
}
