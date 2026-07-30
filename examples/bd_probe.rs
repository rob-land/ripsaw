// Probe the 3D feature detection on a mounted Blu-ray: bd_probe <mount-path>
use ripsaw::rip::bd_playlist::{find_feature_3d, is_encrypted};
fn main() {
    let mount = std::env::args().nth(1).expect("usage: bd_probe <mount>");
    let m = std::path::Path::new(&mount);
    println!("encrypted: {}", is_encrypted(m));
    match find_feature_3d(m) {
        Some(f) => println!("feature: {} clip(s) {:?}, {} s ({} min)", f.clips.len(), f.clips, f.duration_seconds(), f.duration_seconds()/60),
        None => println!("find_feature_3d → None"),
    }
}
