// Ripping via makemkvcon. See docs/rip.md.

pub mod makemkv;
pub mod makemkv_parse;
pub mod makemkv_install;
pub mod drive;

#[derive(Debug, Clone)]
pub struct DiscScan {
    pub drive: drive::Drive,
    pub disc_type: DiscType,
    pub titles: Vec<TitleScan>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscType {
    Dvd,
    BluRay,
    UltraHdBluRay,
}

#[derive(Debug, Clone)]
pub struct TitleScan {
    pub index: u32,
    pub duration_seconds: u64,
    pub size_bytes: u64,
    pub segments: Vec<u32>,
    pub source_file: String,
    pub has_mvc: bool,
    pub languages: Vec<String>,
}
