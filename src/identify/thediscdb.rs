// TheDiscDB GraphQL client. See docs/identify.md.
//
// The GraphQL endpoint is `https://thediscdb.com/graphql` (the same one
// `TheDiscDb/data/tools/ImportBuddy/source/ImportBuddy/ImportBuddy/appsettings.json`
// uses). No authentication is required for read queries.
//
// The query string is a slight modification of the vendored one in
// `data/graphql/GetDiscDetailByContentHash.graphql`: we also request the
// per-disc `contentHash` field so the response can be filtered to the
// disc that matched (a release may contain multiple discs; only one
// carries the queried hash).

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use url::Url;

use super::{Identity, TitleIdentity, TitleRole};

pub struct TheDiscDbClient {
    base: Url,
    http: reqwest::Client,
}

const DEFAULT_BASE: &str = "https://thediscdb.com/";

const QUERY: &str = r#"
query GetDiscDetailByContentHash($hash: String) {
  mediaItems(
    where: { releases: { some: { discs: { some: { contentHash: { eq: $hash } } } } } }
  ) {
    nodes {
      id title year slug imageUrl type
      externalids { tmdb imdb tvdb }
      releases {
        slug isbn locale regionCode year upc title imageUrl
        discs(order: { index: ASC }) {
          index name format slug contentHash
          titles(order: { index: ASC }) {
            index duration displaySize sourceFile size segmentMap
            item {
              title season episode type
              chapters(order: { index: ASC }) { index title }
            }
          }
        }
      }
    }
  }
}
"#;

impl TheDiscDbClient {
    pub fn new(base: Url) -> Self {
        Self { base, http: reqwest::Client::new() }
    }

    pub fn with_default_endpoint() -> Result<Self> {
        Ok(Self::new(Url::parse(DEFAULT_BASE)?))
    }

    pub async fn lookup_by_hash(&self, hash: &str) -> Result<Vec<Identity>> {
        let body = serde_json::json!({
            "operationName": "GetDiscDetailByContentHash",
            "query": QUERY,
            "variables": { "hash": hash },
        });
        let endpoint = self.base.join("graphql").context("constructing graphql endpoint URL")?;
        let raw = self
            .http
            .post(endpoint)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .context("POST to TheDiscDB /graphql")?
            .error_for_status()
            .context("TheDiscDB /graphql HTTP status")?
            .text()
            .await
            .context("reading TheDiscDB /graphql response body")?;
        parse_lookup_response(&raw, hash)
    }
}

/// Parse a `GetDiscDetailByContentHash` GraphQL response and project the
/// matching disc(s) onto our `Identity` shape. Pure function — no I/O —
/// so the bulk of correctness can be tested with golden JSON fixtures.
pub fn parse_lookup_response(json: &str, expected_hash: &str) -> Result<Vec<Identity>> {
    let response: GraphQLResponse<LookupResponseData> =
        serde_json::from_str(json).context("parsing GraphQL response as JSON")?;

    if let Some(errors) = response.errors {
        if !errors.is_empty() {
            let messages: Vec<String> = errors.into_iter().map(|e| e.message).collect();
            return Err(anyhow!("GraphQL errors from TheDiscDB: {}", messages.join("; ")));
        }
    }

    let data = response.data.ok_or_else(|| anyhow!("GraphQL response missing data block"))?;

    let mut out = Vec::new();
    for media_item in &data.media_items.nodes {
        for release in &media_item.releases {
            for disc in &release.discs {
                let disc_hash = match &disc.content_hash {
                    Some(h) => h,
                    None => continue,
                };
                if !hashes_equal(disc_hash, expected_hash) {
                    continue;
                }
                let tmdb_id = media_item
                    .external_ids
                    .as_ref()
                    .and_then(|e| e.tmdb.as_deref())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let imdb_id = media_item
                    .external_ids
                    .as_ref()
                    .and_then(|e| e.imdb.clone())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let tvdb_id = media_item
                    .external_ids
                    .as_ref()
                    .and_then(|e| e.tvdb.clone())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                // Cover art: prefer the media item's image, fall back to
                // the matched release's. Both are relative paths upstream.
                let image_url = media_item
                    .image_url
                    .clone()
                    .or_else(|| release.image_url.clone())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                out.push(Identity {
                    media_item_id: media_item.id.to_string(),
                    release_slug: release.slug.clone(),
                    disc_index: disc.index,
                    titles: disc.titles.iter().map(title_identity_from).collect(),
                    item_title: media_item.title.clone().unwrap_or_default(),
                    year: media_item
                        .year
                        .and_then(|y| if y > 0 { Some(y as u32) } else { None }),
                    tmdb_id,
                    imdb_id,
                    tvdb_id,
                    image_url,
                });
            }
        }
    }
    Ok(out)
}

fn hashes_equal(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn title_identity_from(t: &TitleNode) -> TitleIdentity {
    let item = t.item.as_ref();
    let chapters = item
        .map(|i| {
            i.chapters
                .iter()
                .filter_map(|c| {
                    c.title
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(|s| crate::identify::ChapterIdentity {
                            index: c.index,
                            title: s.to_string(),
                        })
                })
                .collect()
        })
        .unwrap_or_default();
    TitleIdentity {
        index: t.index,
        role: parse_role(item.and_then(|i| i.item_type.as_deref())),
        display_title: item.and_then(|i| i.title.clone()).unwrap_or_default(),
        source_file: t.source_file.clone(),
        chapters,
        season: item.and_then(|i| i.season),
        episode: item.and_then(|i| i.episode),
    }
}

fn parse_role(s: Option<&str>) -> TitleRole {
    match s {
        Some("MainMovie") | Some("MainFeature") | Some("Main") | Some("Movie") => TitleRole::Main,
        Some("Trailer") => TitleRole::Trailer,
        Some("BehindTheScenes") | Some("BehindThe Scenes") => TitleRole::BehindTheScenes,
        Some("DeletedScene") | Some("DeletedScenes") => TitleRole::DeletedScene,
        Some("Featurette") => TitleRole::Featurette,
        Some("Interview") => TitleRole::Interview,
        Some("Scene") => TitleRole::Scene,
        Some("Short") => TitleRole::Short,
        _ => TitleRole::Other,
    }
}

// ---------------------------------------------------------------------------
// GraphQL response types. These are private to the client.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GraphQLResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQLErrorNode>>,
}

#[derive(Deserialize)]
struct GraphQLErrorNode {
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LookupResponseData {
    media_items: MediaItemConnection,
}

#[derive(Deserialize)]
struct MediaItemConnection {
    nodes: Vec<MediaItemNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaItemNode {
    id: i64,
    title: Option<String>,
    year: Option<i32>,
    #[allow(dead_code)] slug: Option<String>,
    image_url: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    item_type: Option<String>,
    #[serde(default, rename = "externalids")]
    external_ids: Option<ExternalIdsNode>,
    #[serde(default)] releases: Vec<ReleaseNode>,
}

#[derive(Deserialize, Default)]
struct ExternalIdsNode {
    /// TheDiscDB stores all three ID flavours as strings; we keep
    /// TMDB as numeric since the canonical form is `tmdb:N`, but
    /// preserve IMDb/TVDb as strings (IMDb is `ttN` and TVDb is
    /// sometimes alphanumeric).
    #[serde(default)] tmdb: Option<String>,
    #[serde(default)] imdb: Option<String>,
    #[serde(default)] tvdb: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseNode {
    slug: String,
    #[allow(dead_code)] isbn: Option<String>,
    #[allow(dead_code)] locale: Option<String>,
    #[allow(dead_code)] region_code: Option<String>,
    #[allow(dead_code)] year: Option<i32>,
    #[allow(dead_code)] upc: Option<String>,
    #[allow(dead_code)] title: Option<String>,
    image_url: Option<String>,
    #[serde(default)] discs: Vec<DiscNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscNode {
    index: u32,
    #[allow(dead_code)] name: Option<String>,
    #[allow(dead_code)] format: Option<String>,
    #[allow(dead_code)] slug: Option<String>,
    content_hash: Option<String>,
    #[serde(default)] titles: Vec<TitleNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TitleNode {
    index: u32,
    #[allow(dead_code)] duration: Option<String>,
    #[allow(dead_code)] display_size: Option<String>,
    #[allow(dead_code)] source_file: Option<String>,
    #[allow(dead_code)] size: Option<u64>,
    #[allow(dead_code)] segment_map: Option<String>,
    item: Option<ItemNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemNode {
    title: Option<String>,
    season: Option<u32>,
    episode: Option<u32>,
    #[serde(rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    chapters: Vec<ChapterNode>,
}

#[derive(Deserialize)]
struct ChapterNode {
    index: u32,
    title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parser_recognises_canonical_strings() {
        assert_eq!(parse_role(Some("MainMovie")), TitleRole::Main);
        assert_eq!(parse_role(Some("MainFeature")), TitleRole::Main);
        assert_eq!(parse_role(Some("Trailer")), TitleRole::Trailer);
        assert_eq!(parse_role(Some("BehindTheScenes")), TitleRole::BehindTheScenes);
        assert_eq!(parse_role(Some("DeletedScene")), TitleRole::DeletedScene);
        assert_eq!(parse_role(Some("DeletedScenes")), TitleRole::DeletedScene);
        assert_eq!(parse_role(Some("Featurette")), TitleRole::Featurette);
        assert_eq!(parse_role(Some("Interview")), TitleRole::Interview);
        assert_eq!(parse_role(Some("Scene")), TitleRole::Scene);
        assert_eq!(parse_role(Some("Short")), TitleRole::Short);
        assert_eq!(parse_role(Some("Whatever")), TitleRole::Other);
        assert_eq!(parse_role(None), TitleRole::Other);
    }

    #[test]
    fn case_insensitive_hash_comparison() {
        assert!(hashes_equal(
            "7523415F507191611266DD68594593A3",
            "7523415f507191611266dd68594593a3",
        ));
        assert!(!hashes_equal("ABC", "ABD"));
    }

    #[test]
    fn empty_data_block_yields_empty_vec() {
        let json = r#"{"data": {"mediaItems": {"nodes": []}}}"#;
        let result = parse_lookup_response(json, "ABCDEF").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn graphql_errors_surface_as_error() {
        let json = r#"{
            "errors": [{"message": "bad query"}, {"message": "auth required"}],
            "data": null
        }"#;
        let err = parse_lookup_response(json, "ABCDEF").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bad query"));
        assert!(msg.contains("auth required"));
    }

    #[test]
    fn multi_region_match_returns_one_identity_per_release() {
        // Same hash hit two regional pressings (NA + EU) — the disambiguation
        // case described in docs/identify.md § "Multi-release disambiguation".
        let json = r#"{
            "data": {
              "mediaItems": {
                "nodes": [{
                  "id": 760, "title": "Some Movie", "year": 2020,
                  "slug": "some-movie", "imageUrl": null, "type": "Movie",
                  "releases": [
                    {"slug": "us-bd", "isbn": null, "locale": "en-US",
                     "regionCode": "A", "year": 2020, "upc": "012345678905",
                     "title": "Some Movie", "imageUrl": null,
                     "discs": [{
                       "index": 0, "name": "Disc 1", "format": "BluRay",
                       "slug": "us-d1", "contentHash": "AAAA",
                       "titles": [{
                         "index": 0, "duration": "1:30:00", "displaySize": "20 GB",
                         "sourceFile": "00800.mpls", "size": 21000000000,
                         "segmentMap": "1", "item": {
                           "title": "Some Movie", "season": null, "episode": null,
                           "type": "MainMovie", "chapters": []
                         }
                       }]
                     }]
                    },
                    {"slug": "eu-bd", "isbn": null, "locale": "en-GB",
                     "regionCode": "B", "year": 2020, "upc": "5051892241366",
                     "title": "Some Movie", "imageUrl": null,
                     "discs": [{
                       "index": 0, "name": "Disc 1", "format": "BluRay",
                       "slug": "eu-d1", "contentHash": "AAAA",
                       "titles": [{
                         "index": 0, "duration": "1:30:00", "displaySize": "20 GB",
                         "sourceFile": "00800.mpls", "size": 21000000000,
                         "segmentMap": "1", "item": {
                           "title": "Some Movie", "season": null, "episode": null,
                           "type": "MainMovie", "chapters": []
                         }
                       }]
                     }]
                    }
                  ]
                }]
              }
            }
        }"#;
        let result = parse_lookup_response(json, "AAAA").unwrap();
        assert_eq!(result.len(), 2, "expected one Identity per matching release");
        let slugs: Vec<&str> = result.iter().map(|i| i.release_slug.as_str()).collect();
        assert!(slugs.contains(&"us-bd"));
        assert!(slugs.contains(&"eu-bd"));
    }

    #[test]
    fn matched_disc_in_multi_disc_release_is_isolated() {
        // A release contains two discs; only one carries our hash. Identity
        // should contain only that disc, not the sibling.
        let json = r#"{
            "data": {
              "mediaItems": {
                "nodes": [{
                  "id": 760,
                  "title": "Some Movie",
                  "year": 2020,
                  "slug": "some-movie",
                  "imageUrl": null,
                  "type": "Movie",
                  "releases": [{
                    "slug": "us-bd",
                    "isbn": null, "locale": "en-US", "regionCode": "A",
                    "year": 2020, "upc": "012345678905",
                    "title": "Some Movie",
                    "imageUrl": null,
                    "discs": [
                      {"index": 0, "name": "Disc 1", "format": "BluRay", "slug": "d1",
                       "contentHash": "AAAA", "titles": []},
                      {"index": 1, "name": "Disc 2", "format": "BluRay", "slug": "d2",
                       "contentHash": "BBBB", "titles": [
                         {"index": 0, "duration": "1:30:00", "displaySize": "20 GB",
                          "sourceFile": "00800.mpls", "size": 21000000000,
                          "segmentMap": "1", "item":
                            {"title": "Some Movie", "season": null, "episode": null,
                             "type": "MainMovie", "chapters": []}}
                       ]}
                    ]
                  }]
                }]
              }
            }
        }"#;
        let result = parse_lookup_response(json, "BBBB").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].media_item_id, "760");
        assert_eq!(result[0].release_slug, "us-bd");
        assert_eq!(result[0].disc_index, 1);
        assert_eq!(result[0].titles.len(), 1);
        assert_eq!(result[0].titles[0].role, TitleRole::Main);
        assert_eq!(result[0].titles[0].display_title, "Some Movie");
    }
}
