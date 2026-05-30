// TMDB REST client. Used by the submission form to fill in title,
// year, plot, tagline, and the cross-references (IMDb ↔ TMDB) when the
// user types one of the two IDs.
//
// Endpoint reference:
//   https://developer.themoviedb.org/reference/intro/getting-started
//
// API key handling: v3 keys live on UserSettings.tmdb_api_key. We
// always pass the key as the `api_key` query parameter -- v4 bearer
// tokens are also accepted by TMDB but the `api_key` form is more
// forgiving and matches the docs' first-tab examples.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

const BASE_URL: &str = "https://api.themoviedb.org/3";

/// What a lookup returns. All fields are optional because the user's
/// view of "what TMDB knows" varies per record.
#[derive(Debug, Clone, Default)]
pub struct TmdbDetails {
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<String>,
    pub title: Option<String>,
    pub year: Option<u32>,
    pub plot: Option<String>,
    pub tagline: Option<String>,
    /// "Movie" or "Series". `None` when the caller passed an ambiguous
    /// IMDb ID and we didn't bother disambiguating.
    pub content_type: Option<&'static str>,
}

pub struct TmdbClient {
    api_key: String,
    http: reqwest::Client,
}

impl TmdbClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            http: reqwest::Client::new(),
        }
    }

    /// Fetch a movie's details by TMDB numeric ID. Always returns
    /// `tmdb_id = Some(tmdb_id)` and `content_type = Some("Movie")`.
    pub async fn fetch_movie(&self, tmdb_id: u64) -> Result<TmdbDetails> {
        let url = format!("{BASE_URL}/movie/{tmdb_id}?api_key={}", self.api_key);
        let body: MovieResponse = self
            .http
            .get(&url)
            .send()
            .await
            .context("GET TMDB movie")?
            .error_for_status()
            .context("TMDB movie HTTP status")?
            .json()
            .await
            .context("parse TMDB movie JSON")?;
        Ok(TmdbDetails {
            tmdb_id: Some(body.id),
            imdb_id: body.imdb_id.filter(|s| !s.is_empty()),
            title: body.title.or(body.original_title).filter(|s| !s.is_empty()),
            year: year_from_release_date(body.release_date.as_deref()),
            plot: body.overview.filter(|s| !s.is_empty()),
            tagline: body.tagline.filter(|s| !s.is_empty()),
            content_type: Some("Movie"),
        })
    }

    /// Fetch a series' details by TMDB numeric ID. Series is "tv" in
    /// TMDB's REST paths.
    pub async fn fetch_series(&self, tmdb_id: u64) -> Result<TmdbDetails> {
        let url = format!(
            "{BASE_URL}/tv/{tmdb_id}?api_key={}&append_to_response=external_ids",
            self.api_key
        );
        let body: SeriesResponse = self
            .http
            .get(&url)
            .send()
            .await
            .context("GET TMDB tv")?
            .error_for_status()
            .context("TMDB tv HTTP status")?
            .json()
            .await
            .context("parse TMDB tv JSON")?;
        Ok(TmdbDetails {
            tmdb_id: Some(body.id),
            imdb_id: body
                .external_ids
                .and_then(|e| e.imdb_id)
                .filter(|s| !s.is_empty()),
            title: body
                .name
                .or(body.original_name)
                .filter(|s| !s.is_empty()),
            year: year_from_release_date(body.first_air_date.as_deref()),
            plot: body.overview.filter(|s| !s.is_empty()),
            tagline: body.tagline.filter(|s| !s.is_empty()),
            content_type: Some("Series"),
        })
    }

    /// Resolve an IMDb ID ("tt0…") to TMDB details. Uses TMDB's
    /// `/find/{imdb}?external_source=imdb_id` endpoint, which returns
    /// matches across movie / tv / person; we take the first hit in
    /// movie_results, then tv_results.
    pub async fn fetch_by_imdb_id(&self, imdb_id: &str) -> Result<TmdbDetails> {
        let id = imdb_id.trim();
        if id.is_empty() {
            bail!("empty IMDb ID");
        }
        let url = format!(
            "{BASE_URL}/find/{id}?api_key={}&external_source=imdb_id",
            self.api_key
        );
        let body: FindResponse = self
            .http
            .get(&url)
            .send()
            .await
            .context("GET TMDB find")?
            .error_for_status()
            .context("TMDB find HTTP status")?
            .json()
            .await
            .context("parse TMDB find JSON")?;
        if let Some(movie) = body.movie_results.into_iter().next() {
            return self.fetch_movie(movie.id).await;
        }
        if let Some(tv) = body.tv_results.into_iter().next() {
            return self.fetch_series(tv.id).await;
        }
        Err(anyhow!("TMDB found no movie or series for IMDb id {id}"))
    }
}

fn year_from_release_date(date: Option<&str>) -> Option<u32> {
    date.and_then(|s| s.split('-').next())
        .and_then(|y| y.parse().ok())
}

// ---------------------------------------------------------------
// TMDB v3 JSON shapes (subsets of what we use).
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MovieResponse {
    id: u64,
    title: Option<String>,
    original_title: Option<String>,
    overview: Option<String>,
    tagline: Option<String>,
    release_date: Option<String>,
    imdb_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SeriesResponse {
    id: u64,
    name: Option<String>,
    original_name: Option<String>,
    overview: Option<String>,
    tagline: Option<String>,
    first_air_date: Option<String>,
    external_ids: Option<SeriesExternalIds>,
}

#[derive(Debug, Deserialize)]
struct SeriesExternalIds {
    imdb_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FindResponse {
    #[serde(default)]
    movie_results: Vec<FindRef>,
    #[serde(default)]
    tv_results: Vec<FindRef>,
}

#[derive(Debug, Deserialize)]
struct FindRef {
    id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_parsed_from_iso_release_date() {
        assert_eq!(year_from_release_date(Some("2012-10-26")), Some(2012));
        assert_eq!(year_from_release_date(Some("1993")), Some(1993));
        assert_eq!(year_from_release_date(None), None);
        assert_eq!(year_from_release_date(Some("")), None);
    }
}
