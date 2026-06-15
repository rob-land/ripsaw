// Run the app's ffprobe identify path against a file.
//   cargo run --example probe_mkv -- file.mkv
fn main() {
    let path = std::env::args().nth(1).expect("usage: probe_mkv <file>");
    let rt = tokio::runtime::Runtime::new().unwrap();
    match rt.block_on(ripsaw::identify::ffprobe::probe(std::path::Path::new(&path))) {
        Ok(r) => println!(
            "OK: {} streams, {} chapters, duration {:?}s",
            r.streams.len(), r.chapters.len(), r.duration_seconds()
        ),
        Err(e) => { eprintln!("FAILED: {e:#}"); std::process::exit(1); }
    }
}
