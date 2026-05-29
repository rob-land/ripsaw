// Third-party media-manager integrations. Sonarr (TV) and Radarr
// (movies) share an API style (Servarr `/api/v3`, `X-Api-Key` header,
// JSON request/response); the modules below share a tiny common
// client type to avoid duplication.

pub mod radarr;
pub mod sonarr;
pub mod servarr;
