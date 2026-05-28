// TheDiscDB content hash. See docs/disc-hash.md.

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

pub fn enumerate_disc_files(_mount: &std::path::Path) -> anyhow::Result<Vec<DiscFile>> {
    // BD/UHD: BDMV/STREAM/*.m2ts ; DVD: VIDEO_TS/* (no extension filter).
    // Sort lexicographically by file name; assign Index in iteration order.
    // See docs/disc-hash.md § "What 'files' means".
    todo!("walk the disc and emit DiscFile in the documented sort order")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
