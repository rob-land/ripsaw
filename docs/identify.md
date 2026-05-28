# Disc identification

Identification turns "a disc in the drive" into either a confirmed
`Identity { media_item, release, disc_index, titles[] }` or an
unidentified-disc fallback that the user can submit to TheDiscDB.

## Flow

```
   ┌─────────────────────────┐
   │ User clicks "Identify"  │
   └────────────┬────────────┘
                ▼
   ┌─────────────────────────┐
   │ scan disc via makemkvcon│   ← produces title/file size list
   └────────────┬────────────┘
                ▼
   ┌─────────────────────────┐
   │ compute content hash    │   ← see docs/disc-hash.md
   └────────────┬────────────┘
                ▼
   ┌─────────────────────────┐
   │ GraphQL query TheDiscDB │
   │ GetDiscDetailByContent  │
   │ Hash(hash: $hash)       │
   └────────────┬────────────┘
                ▼
        ┌───────┴────────┐
        │  1+ mediaItems │  ──► identified ──► resolve to single release/disc
        └───────┬────────┘                    (UI disambiguates if N>1)
                │ 0 results
                ▼
   ┌─────────────────────────────────────────────────────┐
   │ unidentified-disc page:                              │
   │ - show titles list (index, duration, size, segments) │
   │ - text fields: title, year, type (movie/series/...)  │
   │ - per-title type override (main/extra/trailer/...)   │
   │ - optional TMDB/IMDB search to populate IDs          │
   │ - "Submit to TheDiscDB" button                       │
   └─────────────────────────────────────────────────────┘
```

## GraphQL query

Verbatim from
[`GetDiscDetailByContentHash.graphql`](https://github.com/TheDiscDb/data/blob/main/tools/ImportBuddy/source/ImportBuddy/TheDiscDb.Client/GraphQL/Queries/GetDiscDetailByContentHash.graphql):

```graphql
query GetDiscDetailByContentHash($hash: String) {
  mediaItems(
    where: { releases: { some: { discs: { some: { contentHash: { eq: $hash } } } } } }
  ) {
    nodes {
      id title year slug imageUrl type
      releases {
        slug isbn locale regionCode year upc title imageUrl
        discs(order: { index: ASC }) {
          index name format slug
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
```

The endpoint URL and any auth are TBD — pin them once TheDiscDB's public
API is confirmed. The C# `TheDiscDb.Client` project's
`ServiceCollectionExtensions.cs` and `.graphqlrc.json` are the canonical
references.

## Multi-release disambiguation

A `contentHash` *should* be unique, but releases occasionally share
content across regions/printings. When the query returns more than one
match, the UI presents a chooser with cover art, region code, locale,
and UPC; the user picks the release that matches the physical packaging.

## Title role assignment

A `Disc.titles[*].item.type` value drives extras layout. TheDiscDB
classifies titles into roles (main feature, trailer, deleted scene,
behind-the-scenes, featurette, interview, commentary, …). We map these
to Jellyfin's
[extras folder names](https://jellyfin.org/docs/general/server/media/movies/#extras)
in `naming::extras`:

| TheDiscDB role | Output folder | Suffix |
|---|---|---|
| main feature | `<root>/` | (none — file is the primary) |
| trailer | `<root>/trailers/` | (filename preserved) |
| behindthescenes | `<root>/behindthescenes/` | |
| deleted_scene | `<root>/deleted/` | |
| featurette | `<root>/featurettes/` | |
| interview | `<root>/interviews/` | |
| scene | `<root>/scenes/` | |
| short | `<root>/shorts/` | |
| other | `<root>/extras/` | |

(See [naming](naming.md) for the per-scheme variants.)

## TMDB / IMDB enrichment

Once identified, we look up the media item against TMDB by title+year
(falling back to user search if ambiguous) and obtain:

- `tmdbid` (always)
- `imdbid` (from TMDB's external IDs endpoint when present)

These IDs end up in the filename per the chosen naming scheme — see
[naming](naming.md).

For series, season/episode come from the `item` block on the
TheDiscDB response (which already encodes the per-title episode
mapping). TMDB is used to confirm the series and supply IDs, not to
re-derive the episode list.

## Unidentified-disc submission

If TheDiscDB returns zero matches:

1. The UI shows the makemkvcon scan: per-title `index, duration,
   displaySize, segmentMap, sourceFile` table.
2. User enters: media title, year, type, region, locale; per-title
   role; optional TMDB/IMDB IDs (or "look up" button using
   the title+year against TMDB).
3. User clicks **Submit to TheDiscDB**. We construct a contribution
   payload mirroring the JSON schema in
   `github.com/TheDiscDb/data/blob/main/data/` and either:
   - open a pre-filled GitHub PR via the web (no API key needed), or
   - POST to a TheDiscDB contribution endpoint if/when one exists.

The PR route is the safer day-one approach: no auth secrets in 3drip,
and the user retains review of what gets sent. Implementation lives in
`src/identify/submit.rs`.

## Caching

- Disc hash → identity response is cached under
  `$XDG_CACHE_HOME/threedrip/identify/<hash>.json` for `7d`.
- TMDB lookups cache by `(tmdb_id, language)` under
  `$XDG_CACHE_HOME/threedrip/tmdb/`.
- A cache-busting "re-identify" action is exposed in the UI.

## Error handling

| Failure | UX |
|---|---|
| TheDiscDB unreachable | Show inline banner; offer "use cached / retry / submit later" |
| TMDB unreachable / no key | Identify still proceeds; IDs are blank in filenames; warn user |
| makemkvcon failed to scan | Surface raw stderr in an expander; halt before any rip |
| hash matches no release | Fall through to manual entry + submit flow |
