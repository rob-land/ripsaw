// TheDiscDB content hash. See docs/disc-hash.md.

use std::path::Path;

use anyhow::{anyhow, Context};
use md5::{Digest, Md5};

#[derive(Debug, Clone)]
pub struct DiscFile {
    pub index: u32,
    pub name: String,
    pub size: u64,
}

pub fn content_hash(files: &[DiscFile]) -> String {
    let mut hasher = Md5::new();
    for f in files {
        hasher.update(f.size.to_le_bytes());
    }
    format!("{:X}", hasher.finalize())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscLayout {
    BluRayOrUhd,
    Dvd,
}

/// Walk a mounted disc's filesystem and produce the file list that feeds
/// `content_hash`. The layout is determined by which of `BDMV/STREAM` or
/// `VIDEO_TS` exists at the mount root; if both, BD wins (no commercial
/// disc carries both). Files are sorted lexicographically by name and the
/// `index` field is assigned in iteration order.
///
/// See docs/disc-hash.md § "What 'files' means" and confirmed against
/// `ImportBuddy/DiskContentHash.cs::HashMediaDisc`.
pub fn enumerate_disc_files(mount: &Path) -> anyhow::Result<Vec<DiscFile>> {
    let (dir, layout) = detect_layout(mount)?;
    enumerate_dir(&dir, layout)
}

fn detect_layout(mount: &Path) -> anyhow::Result<(std::path::PathBuf, DiscLayout)> {
    let bd_stream = mount.join("BDMV").join("STREAM");
    let dvd_video_ts = mount.join("VIDEO_TS");
    if bd_stream.is_dir() {
        Ok((bd_stream, DiscLayout::BluRayOrUhd))
    } else if dvd_video_ts.is_dir() {
        Ok((dvd_video_ts, DiscLayout::Dvd))
    } else {
        Err(anyhow!(
            "no BDMV/STREAM/ or VIDEO_TS/ directory found at {}",
            mount.display()
        ))
    }
}

fn enumerate_dir(dir: &Path, layout: DiscLayout) -> anyhow::Result<Vec<DiscFile>> {
    let mut entries: Vec<(String, u64)> = Vec::new();
    for read in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = read?;
        let metadata = entry.metadata()?;
        // Skip subdirectories (notably BDMV/STREAM/SSIF/ on 3D BDs).
        if !metadata.is_file() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|raw| anyhow!("non-UTF-8 filename: {:?}", raw))?;
        if !file_matches_layout(&name, layout) {
            continue;
        }
        entries.push((name, metadata.len()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries
        .into_iter()
        .enumerate()
        .map(|(i, (name, size))| DiscFile {
            index: i as u32,
            name,
            size,
        })
        .collect())
}

fn file_matches_layout(name: &str, layout: DiscLayout) -> bool {
    match layout {
        DiscLayout::BluRayOrUhd => {
            // Case-insensitive match for `.m2ts`. Modern discs use lowercase
            // but historically the spec sometimes wrote uppercase.
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".m2ts")
        }
        DiscLayout::Dvd => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn empty_files_hash_is_md5_of_empty() {
        // MD5("") = d41d8cd98f00b204e9800998ecf8427e
        assert_eq!(content_hash(&[]), "D41D8CD98F00B204E9800998ECF8427E");
    }

    #[test]
    fn single_zero_size_file_matches_dotnet() {
        // MD5(\x00\x00\x00\x00\x00\x00\x00\x00) = 7dea362b3fac8e00956a4952a3d4f474
        let files = vec![DiscFile { index: 0, name: "X".into(), size: 0 }];
        assert_eq!(content_hash(&files), "7DEA362B3FAC8E00956A4952A3D4F474");
    }

    /// Create a sparse file at `path` with the given logical length. The
    /// filesystem reports `len` from `metadata()` without consuming
    /// `len` bytes of disk space.
    fn make_sparse(path: &PathBuf, len: u64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let f = fs::File::create(path).unwrap();
        f.set_len(len).unwrap();
    }

    #[test]
    fn enumerate_blu_ray_sorts_m2ts_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let stream = tmp.path().join("BDMV").join("STREAM");
        make_sparse(&stream.join("00002.m2ts"), 200);
        make_sparse(&stream.join("00000.m2ts"), 100);
        make_sparse(&stream.join("00001.m2ts"), 150);

        let files = enumerate_disc_files(tmp.path()).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["00000.m2ts", "00001.m2ts", "00002.m2ts"]);
        assert_eq!(files[0].size, 100);
        assert_eq!(files[1].size, 150);
        assert_eq!(files[2].size, 200);
        // Index follows iteration order, which equals sorted-by-name order.
        for (i, f) in files.iter().enumerate() {
            assert_eq!(f.index, i as u32);
        }
    }

    #[test]
    fn enumerate_blu_ray_ignores_non_m2ts_and_ssif_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let stream = tmp.path().join("BDMV").join("STREAM");
        make_sparse(&stream.join("00000.m2ts"), 100);
        // Things that must NOT appear in the list:
        make_sparse(&stream.join(".DS_Store"), 6);
        make_sparse(&stream.join("readme.txt"), 50);
        // 3D Blu-ray SSIF subdirectory — present but must not be recursed into.
        make_sparse(&stream.join("SSIF").join("00000.ssif"), 999);

        let files = enumerate_disc_files(tmp.path()).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["00000.m2ts"]);
    }

    #[test]
    fn enumerate_blu_ray_matches_uppercase_m2ts_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let stream = tmp.path().join("BDMV").join("STREAM");
        make_sparse(&stream.join("00000.M2TS"), 100);
        let files = enumerate_disc_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "00000.M2TS");
    }

    #[test]
    fn enumerate_dvd_takes_every_file_without_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let video_ts = tmp.path().join("VIDEO_TS");
        make_sparse(&video_ts.join("VIDEO_TS.IFO"), 1000);
        make_sparse(&video_ts.join("VIDEO_TS.BUP"), 1000);
        make_sparse(&video_ts.join("VTS_01_0.IFO"), 2000);
        make_sparse(&video_ts.join("VTS_01_1.VOB"), 1_073_741_824);
        make_sparse(&video_ts.join("VTS_01_2.VOB"), 1_073_741_824);

        let files = enumerate_disc_files(tmp.path()).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["VIDEO_TS.BUP", "VIDEO_TS.IFO", "VTS_01_0.IFO", "VTS_01_1.VOB", "VTS_01_2.VOB"]
        );
    }

    #[test]
    fn enumerate_errors_when_neither_layout_present() {
        let tmp = tempfile::tempdir().unwrap();
        let err = enumerate_disc_files(tmp.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no BDMV/STREAM/ or VIDEO_TS/"), "got: {msg}");
    }

    #[test]
    fn bd_wins_when_both_layouts_present() {
        let tmp = tempfile::tempdir().unwrap();
        make_sparse(&tmp.path().join("BDMV").join("STREAM").join("00000.m2ts"), 100);
        make_sparse(&tmp.path().join("VIDEO_TS").join("VTS_01_1.VOB"), 999);
        let files = enumerate_disc_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "00000.m2ts");
    }
}
