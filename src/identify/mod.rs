// Disc identification. See docs/identify.md.

pub mod disc_hash;
pub mod thediscdb;
pub mod tmdb;
pub mod submit;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub media_item_id: String,
    pub release_slug: String,
    pub disc_index: u32,
    pub titles: Vec<TitleIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleIdentity {
    pub index: u32,
    pub role: TitleRole,
    pub display_title: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TitleRole {
    Main,
    Trailer,
    BehindTheScenes,
    DeletedScene,
    Featurette,
    Interview,
    Scene,
    Short,
    Other,
}
