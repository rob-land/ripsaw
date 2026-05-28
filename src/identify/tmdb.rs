// TMDB REST client. See docs/identify.md.

#[derive(Debug, Clone)]
pub struct TmdbIds {
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<String>,
    pub tvdb_id: Option<u64>,
}

pub struct TmdbClient {
    api_key: String,
    http: reqwest::Client,
}

impl TmdbClient {
    pub fn new(api_key: String) -> Self {
        Self { api_key, http: reqwest::Client::new() }
    }

    pub async fn lookup_movie(&self, _title: &str, _year: Option<u32>) -> anyhow::Result<TmdbIds> {
        todo!("/search/movie + /movie/{{id}}/external_ids")
    }

    pub async fn lookup_series(&self, _title: &str, _year: Option<u32>) -> anyhow::Result<TmdbIds> {
        todo!("/search/tv + /tv/{{id}}/external_ids")
    }
}
