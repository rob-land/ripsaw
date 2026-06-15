// Run the orchestrator's stereo-source detector against an MKV.
//   cargo run --example detect_3d --release -- file.mkv
use ripsaw::convert::plan::detect_stereo_source;
fn main() {
    let path = std::env::args().nth(1).expect("usage: detect_3d <file.mkv>");
    match detect_stereo_source(std::path::Path::new(&path)) {
        Some(s) => println!("DETECTED stereo source: {s:?}"),
        None => println!("NO stereo/MVC source detected"),
    }
}
