// UPCitemDB lookup. Used by the submission form to pre-fill release-
// level metadata (Asin, release Title) from a barcode the user has
// typed.
//
// Endpoint: https://api.upcitemdb.com/prod/trial/lookup?upc=<code>
//
// The "trial" tier is unauthenticated and rate-limited to ~100
// lookups per IP per day. Sufficient for occasional submission work;
// when a user blows through it we'll need to add an API-key tier and
// surface it in Preferences alongside TMDB.
//
// Response shape (only the fields we use):
//   { "code": "OK", "total": 1, "items": [{
//       "title": "Jurassic Park 3D [Blu-ray 3D]",
//       "upc": "0025192190919",
//       "ean": "0025192190919",
//       "asin": "B00DLFV04G",
//       "brand": "Universal",
//       ...
//   }] }

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const BASE_URL: &str = "https://api.upcitemdb.com/prod/trial/lookup";

/// What a UPC lookup yields. All optional -- coverage varies by entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpcDetails {
    /// Title as printed on the packaging (e.g. "Jurassic Park 3D
    /// [Blu-ray 3D]"). Usually verbose; the submission form treats
    /// it as a suggestion for `release_title`.
    pub title: Option<String>,
    /// Amazon Standard Identification Number when known. Maps
    /// straight into release.json's `Asin`.
    pub asin: Option<String>,
    /// Publisher / studio brand, e.g. "Universal" / "Warner Bros".
    /// Not in release.json today but useful for the user to confirm
    /// they have the right barcode.
    pub brand: Option<String>,
    /// Canonical UPC echoed back -- useful for letting the user know
    /// what was queried even when the rest of the record is sparse.
    pub upc: Option<String>,
}

pub struct UpcClient {
    http: reqwest::Client,
    base_url: String,
}

impl Default for UpcClient {
    fn default() -> Self {
        Self::new()
    }
}

impl UpcClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: BASE_URL.to_string(),
        }
    }

    /// Look up a UPC. The user-supplied string is sent verbatim; the
    /// API accepts 12- and 13-digit codes. An empty / non-numeric
    /// payload bails before the HTTP round-trip.
    pub async fn lookup(&self, code: &str) -> Result<UpcDetails> {
        let trimmed = code.trim();
        if trimmed.is_empty() {
            bail!("UPC is empty");
        }
        if !trimmed.chars().all(|c| c.is_ascii_digit()) {
            bail!("UPC must be all digits (got {trimmed:?})");
        }
        let url = format!("{}?upc={}", self.base_url, trimmed);
        let body: LookupResponse = self
            .http
            .get(&url)
            .send()
            .await
            .context("GET UPCitemDB lookup")?
            .error_for_status()
            .context("UPCitemDB HTTP status")?
            .json()
            .await
            .context("parse UPCitemDB JSON")?;

        if body.code.as_deref() != Some("OK") {
            bail!(
                "UPCitemDB returned code={:?} (UPC may be unknown)",
                body.code.unwrap_or_default()
            );
        }
        let Some(item) = body.items.into_iter().next() else {
            // UPCitemDB's free tier doesn't index much in the way of
            // physical media -- common Blu-ray / DVD UPCs return code=OK
            // with an empty items[] array. Surface that as a distinct
            // signal so the UI can suggest entering the ASIN by hand
            // rather than implying the lookup itself crashed.
            bail!(
                "UPCitemDB has no record for {trimmed}. Coverage for \
                 physical discs is sparse on the free tier -- enter the \
                 ASIN manually from amazon.com/dp/<ASIN>."
            );
        };
        Ok(UpcDetails {
            title: nonempty(item.title),
            asin: nonempty(item.asin),
            brand: nonempty(item.brand),
            upc: nonempty(item.upc).or_else(|| nonempty(item.ean)),
        })
    }
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.and_then(|t| {
        let trimmed = t.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[derive(Debug, Deserialize)]
struct LookupResponse {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    items: Vec<LookupItem>,
}

#[derive(Debug, Default, Deserialize)]
struct LookupItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    asin: Option<String>,
    #[serde(default)]
    brand: Option<String>,
    #[serde(default)]
    upc: Option<String>,
    #[serde(default)]
    ean: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_upc() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(UpcClient::new().lookup("")).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn rejects_non_digit_upc() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(UpcClient::new().lookup("not-a-barcode"))
            .unwrap_err();
        assert!(err.to_string().contains("digits"));
    }

    #[test]
    fn nonempty_drops_blank_and_whitespace() {
        assert_eq!(nonempty(None), None);
        assert_eq!(nonempty(Some(String::new())), None);
        assert_eq!(nonempty(Some("   ".into())), None);
        assert_eq!(nonempty(Some("  abc ".into())), Some("abc".into()));
    }

    #[test]
    fn parses_ok_response_with_full_item() {
        let raw = r#"{
          "code": "OK",
          "total": 1,
          "items": [{
            "title": "Jurassic Park 3D [Blu-ray 3D]",
            "upc": "0025192190919",
            "ean": "0025192190919",
            "asin": "B00DLFV04G",
            "brand": "Universal"
          }]
        }"#;
        let resp: LookupResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.code.as_deref(), Some("OK"));
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].asin.as_deref(), Some("B00DLFV04G"));
        assert_eq!(resp.items[0].brand.as_deref(), Some("Universal"));
    }

    #[test]
    fn parses_response_with_missing_optional_fields() {
        // Real UPCitemDB results often skip asin/brand for niche titles.
        let raw = r#"{
          "code": "OK",
          "total": 1,
          "items": [{
            "title": "Some Obscure Disc",
            "upc": "012345678905"
          }]
        }"#;
        let resp: LookupResponse = serde_json::from_str(raw).unwrap();
        assert!(resp.items[0].asin.is_none());
        assert!(resp.items[0].brand.is_none());
    }
}
