// TheDiscDB GraphQL client. See docs/identify.md.

use super::Identity;

pub struct TheDiscDbClient {
    endpoint: url::Url,
    http: reqwest::Client,
}

impl TheDiscDbClient {
    pub fn new(endpoint: url::Url) -> Self {
        Self { endpoint, http: reqwest::Client::new() }
    }

    pub async fn lookup_by_hash(&self, _hash: &str) -> anyhow::Result<Vec<Identity>> {
        todo!("POST GetDiscDetailByContentHash query and map nodes -> Identity")
    }
}

// GraphQL query string lives next to the schema in data/graphql/ at build
// time. The schema and queries are vendored from TheDiscDb.Client.
const QUERY_GET_DISC_DETAIL_BY_CONTENT_HASH: &str = include_str!(
    "../../data/graphql/GetDiscDetailByContentHash.graphql"
);
