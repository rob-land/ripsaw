use std::path::PathBuf;

use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "threedrip=debug,info".into()),
        )
        .init();

    // Headless CLI: --identify-iso PATH runs the full identify pipeline
    // (scan + mount + hash + TheDiscDB lookup) and prints results.
    let args: Vec<String> = std::env::args().collect();
    if let Some(iso_arg_pos) = args.iter().position(|a| a == "--identify-iso") {
        let path = args
            .get(iso_arg_pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--identify-iso requires a PATH argument"))?;
        return run_identify_cli(PathBuf::from(path));
    }

    gio::resources_register_include!("threedrip.gresource")
        .expect("register threedrip resources");

    threedrip::application::run()
}

fn run_identify_cli(path: PathBuf) -> Result<()> {
    use threedrip::identify::pipeline::identify_iso;
    let result = threedrip::runtime::tokio_runtime()
        .block_on(identify_iso(path.clone()))?;
    println!("disc.path        = {}", path.display());
    println!(
        "disc.name        = {}",
        result.scan.disc.name.as_deref().unwrap_or("(unnamed)")
    );
    println!(
        "disc.label       = {}",
        result.scan.disc.volume_label.as_deref().unwrap_or("(none)")
    );
    println!("disc.type        = {:?}", result.disc_type);
    println!("makemkv.version  = {:?}", result.scan.makemkv_version);
    println!(
        "mount.point      = {:?}",
        result.mount.as_ref().map(|m| m.mount_point.display().to_string())
    );
    println!(
        "content.hash     = {}",
        result.content_hash.as_deref().unwrap_or("(unavailable)")
    );
    println!("identities       = {} match(es)", result.identities.len());
    println!("titles           = {}", result.scan.titles.len());
    for t in &result.scan.titles {
        println!(
            "  t{:<2} dur={:>6}s size={:>11} src={:<14} segmap={:<14}",
            t.index,
            t.duration_seconds.unwrap_or(0),
            t.size_bytes.unwrap_or(0),
            t.source_file.as_deref().unwrap_or("?"),
            t.segment_map.as_deref().unwrap_or("?"),
        );
    }
    if let Some(m) = result.mount {
        threedrip::runtime::tokio_runtime().block_on(async { m.unmount().await.ok() });
    }
    Ok(())
}
