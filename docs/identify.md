# Disc identification

Identification turns "a disc in the drive" into either a confirmed
`Identity { media_item, release, disc_index, titles[] }` or an
unidentified-disc fallback that the user can submit to TheDiscDB.

## Sources of truth (as of 2026-05)

Title-level disc cataloguing — the kind that distinguishes a movie's
main feature, its director's cut, its trailer reel, and its
behind-the-scenes featurettes — has effectively one open source:

- **[TheDiscDB](https://thediscdb.com/)** is the only catalogue that
  records disc structure at title granularity. We query it as the
  primary identification.

Adjacent sources are useful as augmentation or fallback but do not
replace TheDiscDB:

- **MakeMKV's own disc scan** is good for *structure* (title list with
  durations, segments, sizes) but does not classify titles into roles
  (main feature vs. extra vs. trailer).
- **On-disc metadata** (`BDMV/META/DL/bdmt_*.xml` on Blu-ray, IFO
  fields on DVD) carries a disc title when present, but in practice
  is missing on the majority of commercial discs — HD-DVD is the only
  format that reliably carries it.
- **UPC / EAN, AACS Volume ID, MKB version** identify a *pressing* but
  not the *title structure*. They are useful keys for matching against
  TheDiscDB and for resubmissions.
- **MakeMKV forum KeyDB threads** ship per-disc Volume ID + AACS keys
  for community decryption, not metadata. The submission format is
  reusable as a secondary fingerprint reference.

Strategy: **collect every signal we reasonably can at scan time,
even if today we only use the content hash.** A future lookup
mechanism (a TheDiscDB UPC index, a new catalog, a perceptual-hash
service) does not require re-scanning the disc. See § "Disc
fingerprint" below for the full signal set.

## Flow

```
   ┌────────────────────────────────────────────┐
   │ User clicks "Identify"                     │
   └────────────────────┬───────────────────────┘
                        ▼
   ┌────────────────────────────────────────────┐
   │ scan via makemkvcon info                   │   ← per-title scan record
   │ walk UDF / ISO9660 filesystem              │   ← volume label, file tree
   │ read on-disc metadata XML (best-effort)    │   ← BDMV/META/DL/bdmt_*.xml
   │ extract AACS Vol ID + MKB version          │   ← BD/UHD with LibreDrive
   │ compute TheDiscDB content hash             │   ← MD5 over file sizes
   └────────────────────┬───────────────────────┘
                        ▼
   ┌────────────────────────────────────────────┐
   │ assemble DiscFingerprint (all signals)     │
   │ persist to                                 │
   │ $XDG_CACHE_HOME/threedrip/scans/<hash>.json│
   └────────────────────┬───────────────────────┘
                        ▼
   ┌────────────────────────────────────────────┐
   │ Lookup chain (extensible)                  │
   │  1. TheDiscDB by content hash   ← primary  │
   │  2. (future) by UPC                        │
   │  3. (future) by AACS Volume ID             │
   │  4. (future) other catalogs as they appear │
   └────────────────────┬───────────────────────┘
                        ▼
                ┌───────┴────────┐
                │  hit / miss    │
                └───────┬────────┘
                        │ miss
                        ▼
   ┌─────────────────────────────────────────────────────┐
   │ unidentified-disc page:                             │
   │ - show titles list (index, duration, size, segments)│
   │ - text fields: title, year, type (movie/series/...) │
   │ - per-title type override (main/extra/trailer/...)  │
   │ - optional TMDB/IMDB search to populate IDs         │
   │ - "Submit to TheDiscDB" button                      │
   │   → submission payload carries the full             │
   │     DiscFingerprint, not just user input            │
   └─────────────────────────────────────────────────────┘
```

## Disc fingerprint

The `DiscFingerprint` is everything we capture about a disc in a
single scan, persisted as JSON. It is the input to every lookup, the
payload of every submission, and the cache key (by content hash) for
re-runs.

### Signal inventory

| # | Signal | Source | Format | Availability | Lookup use |
|---|---|---|---|---|---|
| 1 | **TheDiscDB content hash** | `makemkvcon info` file sizes + UDF walk | 32-hex uppercase MD5 | Always (any disc with extractable file sizes) | TheDiscDB primary key — see [docs/disc-hash.md](disc-hash.md) |
| 2 | **Disc volume label** | UDF / ISO9660 filesystem | string, ≤32 chars | Almost always (commercial discs nearly all set it; BD often sets it to the disc title) | TheDiscDB free-text match, manual-entry hint |
| 3 | **Disc type** | makemkvcon DRV/MSG; presence of `BDMV/`, `VIDEO_TS/`, `AACS2/` | enum: Dvd / BluRay / UltraHdBluRay / BluRay3D | Always | gates which decoders/codecs we expect |
| 4 | **UPC / EAN** | makemkvcon CINFO record code 31 | 12 or 13 digit string | Sometimes (perhaps a third of BDs in our sample; rare on DVDs) | future TheDiscDB UPC index; UPCitemdb / BarcodeLookup fallback |
| 5 | **MakeMKV CINFO disc record** | `makemkvcon info` | struct: name, comment, language, content type, year | Usually (MakeMKV synthesises some fields from filename/UDF when disc metadata is missing) | manual-entry hint; degraded fallback when TheDiscDB misses |
| 6 | **AACS Volume ID** | makemkvcon MSG `Using LibreDrive mode (v.. id=…)` and verbose log | 16 bytes / 32 hex | BD / UHD only, requires a LibreDrive-friendly drive | uniquely identifies a pressing run; could feed a future per-pressing key |
| 7 | **MKB version** | makemkvcon MSG records | integer | BD / UHD with LibreDrive | distinguishes successive printings of the same release |
| 8 | **On-disc metadata XML** | read `BDMV/META/DL/bdmt_*.xml` per language code | struct: title, description, language code | Rare on commercial BD (per user observation), reliable only on HD-DVD; absent on DVD | manual-entry hint when present |
| 9 | **BDMV index summary** | parse `BDMV/index.bdmv`, `BDMV/MovieObject.bdmv` | counts: playlists, BDJ objects, first-play target | BD always | secondary structural fingerprint |
| 10 | **Per-title scan records** | makemkvcon TINFO + SINFO | array of `(index, duration, size, source_file, segment_map, chapter_count, streams[])` | Always | already used by manual-entry UI; also forms the submission payload |
| 11 | **Per-stream records** | makemkvcon SINFO | array per title of `(index, kind, codec, language, channels, title)` | Always | manual-entry hint (track names sometimes encode role) |
| 12 | **Drive identification** | makemkvcon DRV record + udisks2 | `{vendor, model, firmware, libredrive_mode}` | Always | flag drive-specific scan quirks; reproducibility |
| 13 | **MakeMKV version + beta key state** | `makemkvcon --version` + parsing settings.conf | strings | Always | makes the scan record reproducible / replayable |
| 14 | **Structural hash (secondary)** | hash of `(playlist count, per-title sizes, per-title segment maps)` | 32-hex SHA-256 | Always | future fallback when content hash misses due to pressing differences |

Signals 1–5, 10–13 are mandatory; 6, 7 are best-effort (depend on
drive capability); 8 is best-effort; 9 is cheap so we capture it; 14
is computed at zero extra I/O cost.

### What we do NOT collect

- **First-frame perceptual hashes of titles** — expensive (full decode
  of one frame per title), and not used by any current catalog. Worth
  reconsidering if a perceptual-hash disc catalog appears.
- **Whole-disc cryptographic hash** (raw SHA-256 of every byte) —
  takes minutes per disc, only ever distinguishes bit-identical
  copies, which we never need to distinguish.
- **Burst Cutting Area (BCA) serial** — not reliably exposed by
  Linux optical-drive APIs; vendor-specific.

### Persistence

The fingerprint is written to
`$XDG_CACHE_HOME/threedrip/scans/<content_hash>.json` at end of scan,
**before** any lookup. This means:

- A scan is never wasted; lookup retries are free.
- The submission payload for an unidentified disc is just the
  fingerprint plus the user's manual-entry block, by file reference.
- Future catalogs can be retro-queried against existing scans without
  the disc being present.

The cache is unbounded by default (scan records are small, ~50 KB
each); a future "forget this disc" UI action can remove an entry.

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
   payload from:
   - The full `DiscFingerprint` from § "Disc fingerprint" — every
     signal we captured, not just the content hash. This gives
     TheDiscDB the richest possible new-record seed.
   - The user's manual-entry block.

   The payload mirrors the JSON schema in
   [`TheDiscDb/data`](https://github.com/TheDiscDb/data/blob/main/data/);
   delivery is either:
   - open a pre-filled GitHub PR via the web (no API key needed), or
   - POST to a TheDiscDB contribution endpoint if/when one exists.

The PR route is the safer day-one approach: no auth secrets in 3drip,
and the user retains review of what gets sent. Implementation lives in
`src/identify/submit.rs`.

When a future lookup mechanism arrives (e.g. UPC-indexed search), the
same fingerprint records already cached under
`$XDG_CACHE_HOME/threedrip/scans/` can be re-queried without the disc
being inserted.

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
