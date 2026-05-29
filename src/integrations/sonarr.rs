// Sonarr REST API v3 client. https://sonarr.tv/docs/api/

use anyhow::Result;
use serde::Deserialize;
use url::Url;

use super::servarr::ServarrClient;

#[derive(Debug, Clone)]
pub struct SonarrClient {
    inner: ServarrClient,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub tvdb_id: Option<u64>,
    #[serde(default)]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub tmdb_id: Option<u64>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub id: u64,
    pub series_id: u64,
    pub season_number: u32,
    pub episode_number: u32,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub air_date: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub has_file: Option<bool>,
}

impl SonarrClient {
    pub fn new(base: Url, api_key: String) -> Self {
        Self {
            inner: ServarrClient::new(base, api_key),
        }
    }

    /// `GET /api/v3/series/lookup?term=...` — searches the configured
    /// metadata source for matching shows. Use to resolve a guessed
    /// disc-label like "Dobiegillis" into a canonical "Dobie Gillis".
    pub async fn lookup(&self, term: &str) -> Result<Vec<Series>> {
        self.inner.get("series/lookup", &[("term", term)]).await
    }

    /// `GET /api/v3/series` — all series the user already has in
    /// their Sonarr library.
    pub async fn list(&self) -> Result<Vec<Series>> {
        self.inner.get("series", &[]).await
    }

    /// `GET /api/v3/episode?seriesId=N&seasonNumber=M` — episode list,
    /// optionally filtered to a single season.
    pub async fn episodes(&self, series_id: u64, season: Option<u32>) -> Result<Vec<Episode>> {
        let series_id_str = series_id.to_string();
        let mut params: Vec<(&str, &str)> = vec![("seriesId", series_id_str.as_str())];
        let season_str;
        if let Some(s) = season {
            season_str = s.to_string();
            params.push(("seasonNumber", &season_str));
        }
        self.inner.get("episode", &params).await
    }

    /// Convenience: parse the user-supplied URL/key into a client.
    pub fn from_config(url: &str, api_key: &str) -> Result<Self> {
        Ok(Self::new(super::servarr::parse_base(url)?, api_key.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_series_lookup_response() {
        let json = r#"[
          {"id": 0, "title": "Dobie Gillis", "year": 1959,
           "tvdbId": 78840, "imdbId": "tt0052474", "overview": "...",
           "status": "ended"},
          {"id": 0, "title": "Some Other Show", "year": 2020, "tvdbId": 9}
        ]"#;
        let parsed: Vec<Series> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].title, "Dobie Gillis");
        assert_eq!(parsed[0].tvdb_id, Some(78840));
    }

    #[test]
    fn deserialises_episode_list_response() {
        let json = r#"[
          {"id": 1, "seriesId": 7, "seasonNumber": 1, "episodeNumber": 1,
           "title": "Caper at the Bijou", "airDate": "1959-09-29",
           "hasFile": false},
          {"id": 2, "seriesId": 7, "seasonNumber": 1, "episodeNumber": 2,
           "title": "Best Dressed Man"}
        ]"#;
        let parsed: Vec<Episode> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].title.as_deref(), Some("Caper at the Bijou"));
        assert_eq!(parsed[1].episode_number, 2);
    }

    #[test]
    fn from_config_accepts_a_url_without_trailing_slash() {
        let client = SonarrClient::from_config("http://localhost:8989", "abc").unwrap();
        // base_str strips the trailing slash for display:
        assert_eq!(client.inner.base_str(), "http://localhost:8989");
    }
}
