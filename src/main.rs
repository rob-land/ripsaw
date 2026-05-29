use std::path::PathBuf;

use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ripsaw=debug,info".into()),
        )
        .init();

    // Headless CLI: --identify-iso PATH runs the full identify pipeline
    // (scan + mount + hash + TheDiscDB lookup) and prints results.
    // --identify-disc N MOUNT does the same for a physical drive with an
    // already-mounted UDF/ISO9660 filesystem at MOUNT.
    let args: Vec<String> = std::env::args().collect();
    if let Some(iso_arg_pos) = args.iter().position(|a| a == "--identify-iso") {
        let path = args
            .get(iso_arg_pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--identify-iso requires a PATH argument"))?;
        return run_identify_cli(PathBuf::from(path));
    }
    if let Some(disc_arg_pos) = args.iter().position(|a| a == "--identify-disc") {
        let index_arg = args
            .get(disc_arg_pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--identify-disc requires INDEX MOUNT_PATH arguments"))?;
        let index: u32 = index_arg.parse().map_err(|e| {
            anyhow::anyhow!("--identify-disc INDEX must be an integer: {e}")
        })?;
        let mount = args
            .get(disc_arg_pos + 2)
            .ok_or_else(|| anyhow::anyhow!("--identify-disc requires a MOUNT_PATH argument"))?;
        return run_identify_disc_cli(index, PathBuf::from(mount));
    }
    if let Some(mkv_arg_pos) = args.iter().position(|a| a == "--identify-mkv") {
        let path = args
            .get(mkv_arg_pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--identify-mkv requires a PATH argument"))?;
        return run_identify_mkv_cli(PathBuf::from(path));
    }

    gio::resources_register_include!("ripsaw.gresource")
        .expect("register ripsaw resources");

    ripsaw::application::run()
}

fn run_identify_mkv_cli(path: PathBuf) -> Result<()> {
    use ripsaw::identify::pipeline::identify_mkv;
    let result = ripsaw::runtime::tokio_runtime()
        .block_on(identify_mkv(path.clone()))?;
    print_identification(&format!("mkv:{}", path.display()), &path, &result);
    println!("has_mvc          = {}", result.has_mvc);
    Ok(())
}

fn run_identify_disc_cli(index: u32, mount: PathBuf) -> Result<()> {
    use ripsaw::identify::pipeline::identify_physical_disc;
    let result = ripsaw::runtime::tokio_runtime()
        .block_on(identify_physical_disc(index, mount.clone()))?;
    print_identification(&format!("disc:{index}"), &mount, &result);
    Ok(())
}

fn run_identify_cli(path: PathBuf) -> Result<()> {
    use ripsaw::identify::pipeline::identify_iso;
    let result = ripsaw::runtime::tokio_runtime()
        .block_on(identify_iso(path.clone()))?;
    print_identification(&path.display().to_string(), &path, &result);
    if let Some(m) = result.mount {
        ripsaw::runtime::tokio_runtime().block_on(async { m.unmount().await.ok() });
    }
    Ok(())
}

fn print_identification(
    source: &str,
    _path: &std::path::Path,
    result: &ripsaw::identify::pipeline::IdentificationResult,
) {
    println!("disc.source      = {source}");
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
            "  t{:<2} dur={:>6}s size={:>11} chap={:<2} src={:<14} segmap={:<14}",
            t.index,
            t.duration_seconds.unwrap_or(0),
            t.size_bytes.unwrap_or(0),
            t.chapter_count.unwrap_or(0),
            t.source_file.as_deref().unwrap_or("?"),
            t.segment_map.as_deref().unwrap_or("?"),
        );
    }
}
