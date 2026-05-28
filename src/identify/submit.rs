// Submit unidentified discs to TheDiscDB. See docs/identify.md § "Unidentified-disc submission".

use super::TitleRole;

#[derive(Debug, Clone)]
pub struct SubmissionDraft {
    pub title: String,
    pub year: Option<u32>,
    pub media_type: String,
    pub region: Option<String>,
    pub locale: Option<String>,
    pub upc: Option<String>,
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<String>,
    pub titles: Vec<TitleDraft>,
    pub disc_hash: String,
}

#[derive(Debug, Clone)]
pub struct TitleDraft {
    pub index: u32,
    pub duration_seconds: u64,
    pub size_bytes: u64,
    pub segment_map: String,
    pub source_file: String,
    pub role: TitleRole,
    pub display_title: String,
}

pub fn open_github_pr(_draft: &SubmissionDraft) -> anyhow::Result<()> {
    todo!("render JSON per TheDiscDb/data schema and xdg-open a pre-filled PR URL")
}
