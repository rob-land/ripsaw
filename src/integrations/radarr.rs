// Radarr REST API v3 client. https://radarr.video/docs/api/

use anyhow::Result;
use serde::Deserialize;
use url::Url;

use super::servarr::ServarrClient;

#[derive(Debug, Clone)]
pub struct RadarrClient {
    inner: ServarrClient,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Movie {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub tmdb_id: Option<u64>,
    #[serde(default)]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub has_file: Option<bool>,
    #[serde(default)]
    pub runtime: Option<u32>,
}

impl RadarrClient {
    pub fn new(base: Url, api_key: String) -> Self {
        Self {
            inner: ServarrClient::new(base, api_key),
        }
    }

    /// `GET /api/v3/movie/lookup?term=...` — search.
    pub async fn lookup(&self, term: &str) -> Result<Vec<Movie>> {
        self.inner.get("movie/lookup", &[("term", term)]).await
    }

    /// `GET /api/v3/movie` — all movies in the library.
    pub async fn list(&self) -> Result<Vec<Movie>> {
        self.inner.get("movie", &[]).await
    }

    pub fn from_config(url: &str, api_key: &str) -> Result<Self> {
        Ok(Self::new(super::servarr::parse_base(url)?, api_key.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_movie_lookup_response() {
        let json = r#"[
          {"id": 0, "title": "Avatar", "year": 2009, "tmdbId": 19995,
           "imdbId": "tt0499549", "runtime": 162, "status": "released"},
          {"id": 0, "title": "Some Other Movie", "year": 2020}
        ]"#;
        let parsed: Vec<Movie> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].title, "Avatar");
        assert_eq!(parsed[0].imdb_id.as_deref(), Some("tt0499549"));
        assert_eq!(parsed[0].runtime, Some(162));
    }

    #[test]
    fn from_config_constructs_client() {
        assert!(RadarrClient::from_config("http://localhost:7878", "abc").is_ok());
        assert!(RadarrClient::from_config("", "abc").is_err());
    }
}
