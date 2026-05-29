// Shared base for Sonarr and Radarr clients. They both speak the same
// "Servarr" REST API conventions on `/api/v3` with an `X-Api-Key`
// header, so the HTTP wiring lives here once.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone)]
pub struct ServarrClient {
    base: Url,
    api_key: String,
    http: reqwest::Client,
}

impl ServarrClient {
    pub fn new(base: Url, api_key: String) -> Self {
        Self {
            base,
            api_key,
            http: reqwest::Client::new(),
        }
    }

    /// Issue a GET against `/api/v3/<endpoint>` with the configured
    /// API key header, deserialise the JSON response into `T`.
    pub async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let mut url = self
            .base
            .join(&format!("api/v3/{endpoint}"))
            .context("building Servarr URL")?;
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        let response = self
            .http
            .get(url)
            .header("X-Api-Key", &self.api_key)
            .header("accept", "application/json")
            .send()
            .await
            .context("GET to Servarr endpoint")?
            .error_for_status()
            .context("Servarr HTTP status")?;
        let body = response.bytes().await.context("Servarr response body")?;
        serde_json::from_slice::<T>(&body).with_context(|| {
            format!(
                "parsing Servarr JSON ({} bytes from {endpoint})",
                body.len()
            )
        })
    }

    /// Convenience: returns base URL without trailing slash, for diagnostics.
    pub fn base_str(&self) -> String {
        let s = self.base.as_str();
        s.trim_end_matches('/').to_string()
    }
}

/// A small ID payload used by every "lookup" / "list" response.
#[derive(Debug, Clone, Deserialize)]
pub struct IdOnly {
    #[serde(default)]
    pub id: Option<u64>,
}

#[derive(Debug)]
pub struct InvalidConfig;

impl std::fmt::Display for InvalidConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no Servarr URL or API key configured")
    }
}

impl std::error::Error for InvalidConfig {}

pub fn parse_base(url: &str) -> Result<Url> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("base URL is empty"));
    }
    let mut url = Url::parse(trimmed).context("parsing base URL")?;
    if !url.path().ends_with('/') {
        let path = url.path().to_string();
        url.set_path(&format!("{path}/"));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_base_normalises_trailing_slash() {
        let url = parse_base("http://sonarr.local:8989").unwrap();
        assert_eq!(url.as_str(), "http://sonarr.local:8989/");

        let url = parse_base("http://sonarr.local:8989/api/v3").unwrap();
        assert_eq!(url.as_str(), "http://sonarr.local:8989/api/v3/");
    }

    #[test]
    fn parse_base_rejects_empty() {
        assert!(parse_base("").is_err());
        assert!(parse_base("   ").is_err());
    }
}
