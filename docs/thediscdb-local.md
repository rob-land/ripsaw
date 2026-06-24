# Local TheDiscDB data source

Status: **implemented 2026-06-24** (scoped 2026-06-22). Motivated by the
hosted endpoint outage that made every lookup fail. Companion to
`docs/identify.md`.

## What shipped

- `src/identify/thediscdb_local.rs` — `LocalDiscDb`: walks a mirror to
  build a `contentHash -> disc*.json` index, and resolves a hash to our
  `Identity` (mapping the PascalCase, disc-centric on-disk JSON, reading
  the sibling `release.json` + the title's `metadata.json` for IDs and
  cover art). Plus `sync_mirror()` / `disc_count()`. Golden-JSON tests run
  against real Friday-the-13th fixtures in `tests/fixtures/thediscdb/`.
- `scripts/sync-thediscdb.sh` — blobless sparse JSON clone/refresh
  (Strategy 1 below).
- Pipeline: `lookup_with_status` tries the local mirror first, falls back
  to live GraphQL; a local hit reports `LookupStatus::Ok`.
- Preferences → "Disc catalogue (TheDiscDB)": status row + Download/Refresh
  button (runs the sync on a worker thread).
- Settings: `thediscdb_mirror` override; default
  `$XDG_CACHE_HOME/ripsaw/thediscdb` (`settings::thediscdb_mirror_root`).

**Verified end-to-end 2026-06-24:** with the GraphQL API still returning
403, a synced mirror identified the mounted Friday the 13th Part 3 disc
locally — title, year, TMDb 9728, IMDb tt0083972, cover URL, 13 titles
with the feature tagged Main.

**Index persistence (done 2026-06-24).** The hash index is persisted to
`<mirror>/hash-index.json`, keyed to the mirror's git HEAD (read straight
from `.git`, no subprocess). A cache hit is one file read instead of
walking ~4k disc files; measured **183 ms cold → 0.93 ms warm** on the
real mirror (369 KB index). It rebuilds automatically when a sync moves
HEAD, and falls back to an uncached walk for non-git mirrors.

Still open (Strategy 2): the on-demand-GitHub-fetch alternative. The
original scope follows.

---

## Original scope (2026-06-22)

### Why

TheDiscDB's hosted GraphQL endpoint (`https://thediscdb.com/graphql`) is
**unreliable** — observed returning `403 "Web App - Unavailable"` (an
Azure App Service that's been stopped), with the whole site down. While
it's down, *every* disc lookup fails. Commit `b02da5f` made that failure
visible (LookupStatus::Failed instead of a false "not in catalog"), but
the disc still isn't identified.

The underlying data, however, is **open, complete, and live on GitHub**:
[`TheDiscDb/data`](https://github.com/TheDiscDb/data), pushed daily. The
disc we were testing — Friday the 13th Part III (1982) — *is* catalogued
there, and its stored `ContentHash` matches what our hashing computes,
**byte for byte**. So a local data source would have identified the disc
correctly even with the site down. This is resilience + offline, not just
"avoid a network round-trip".

## Data shape (verified 2026-06-22)

Repo layout (the real catalogue is under `data/`, not the small top-level
`movie/`/`series/` sample dirs):

```
data/movie/<Title (Year)>/
  metadata.json          # ExternalIds {Tmdb, Imdb, Tvdb}, Title, Year, Type, Slug, Plot…
  tmdb.json, imdb.json   # raw provider dumps (not needed)
  cover.jpg              # image (not needed)
  <release-slug>/
    release.json         # Slug, Asin, Upc, RegionCode, Locale, Title, ImageUrl
    disc01.json          # Index, Slug, Name, Format, ContentHash, Titles[]
    disc01.txt, *-summary.txt, front.jpg   # not needed
data/series/<...>/       # same shape
data/sets/<...>/
```

`discNN.json` is the key file. Its `Titles[]` carry everything our
`Identity` / `TitleIdentity` needs:

```
Titles[i] = { Index, SourceFile, SegmentMap, Duration, Size,
              Item: { Title, Type, Season?, Episode?, Chapters[] },
              Tracks[] }
```

Note the JSON is **PascalCase** and **disc-centric**, whereas the GraphQL
response is camelCase and `mediaItems → releases → discs`-nested. So the
existing `parse_lookup_response` (camelCase → Identity) does **not** apply
to local files; a separate, simpler local mapper is needed (it's actually
easier — the file *is* the matched disc, no hash-filtering of siblings).

External IDs (`Tmdb`/`Imdb` for the `Identity`) live one level up in the
title's `metadata.json`, not in `discNN.json`.

## Data weight (full repo git tree, not truncated)

| Kind | Count | Size |
|---|---:|---:|
| `.json` (all) | 10,299 | 352 MB |
| `.txt` (disc summaries) | 8,664 | 625 MB |
| `.jpg` (covers/fronts) | 4,048 | 936 MB |
| **Total repo** | | **~1.0 GB** |

The lookup needs only `discNN.json` + `metadata.json` + `release.json` —
a subset of the 352 MB JSON, and far less if we keep just a hash index
plus fetch matched files on demand. The 625 MB of `.txt` and 936 MB of
images are irrelevant to identification.

There is **no prebuilt hash→disc index** in the repo (checked
`housekeeping/`, `tools/`); we build our own.

## Proposed design

A `LocalDiscDb` source that resolves a content hash to `Vec<Identity>`
from a local mirror, used as the primary path with the live GraphQL as a
fallback/refresh.

### Index

Walk every `discNN.json` once and build a compact index:

```
contentHash (upper-hex)  ->  relative path of the discNN.json
```

~10,299 discs × ~80 bytes ≈ under 1 MB, serialised to
`$XDG_CACHE_HOME/ripsaw/thediscdb/hash-index.json`. Rebuilt when the
mirror updates (cheap; it's a directory walk).

### Two mirror strategies (pick one; both feed the same index/lookup)

1. **JSON-subset sync (recommended).** A `scripts/sync-thediscdb.sh`
   (or in-app) `git clone --filter=blob:none --sparse` of `TheDiscDb/data`
   restricted to `*.json` (sparse-checkout patterns excluding `*.txt`,
   `*.jpg`). ~352 MB, refreshable with `git pull`. Fully offline after.
2. **On-demand GitHub fetch (lightest).** Ship/build only the hash index;
   at lookup time fetch the matched `discNN.json` + `metadata.json` +
   `release.json` from `raw.githubusercontent.com`. ~3 small GETs per
   hit, works when the *site* is down (GitHub raw is independent), but
   not fully offline and needs the index kept current. The index itself
   can be regenerated from the GitHub git tree API without a full clone.

A good shipping default: **build the hash index from the GitHub git-tree
API on first run (one API call lists all paths; the hash isn't in the
path, so we still must read each `discNN.json` to get it — so really
strategy 1's sparse clone is what populates the index cheaply).**
Practical recommendation: **strategy 1** — sparse-clone the JSON, build
the index, lookup fully locally; offer a "refresh catalogue" action.

### Lookup flow

```
content hash
  → LocalDiscDb.lookup(hash)               # index → disc json → Identity
      hit  → use it (offline, instant)
      miss → (optional) live GraphQL lookup, if reachable
  → live GraphQL                            # when no local mirror present
```

Keep the existing live client; add `LocalDiscDb` and a thin resolver that
prefers local. `LookupStatus` extends naturally: `Ok` from either source;
`Failed` only when *both* the local miss and the live attempt error.

### New code

- `src/identify/thediscdb_local.rs`: index build + `lookup(hash) ->
  Result<Vec<Identity>>`, with a PascalCase `disc*.json`/`metadata.json`
  → `Identity` mapper (mirror of `title_identity_from`, different casing).
  Golden-JSON tests using the real Friday-the-13th files as fixtures.
- `scripts/sync-thediscdb.sh`: sparse JSON clone + refresh.
- Settings: optional mirror path (default `$XDG_CACHE_HOME/ripsaw/
  thediscdb/data`), a "use local catalogue" toggle, and a "refresh" action
  in Preferences.
- Pipeline: try local before live; thread both into `LookupStatus`.

## Effort

| Task | Est. |
|---|---|
| `thediscdb_local.rs` mapper + index + tests (fixtures from the real files) | 1.5–2 d |
| `sync-thediscdb.sh` sparse-clone + refresh | 0.5 d |
| Pipeline integration (local-first, live fallback, status) | 0.5–1 d |
| Preferences UI (mirror path, toggle, refresh action) | 0.5–1 d |
| **Total** | **~3–4 d** |

## Open questions

- **Refresh cadence/UX.** Manual "refresh catalogue" button vs background
  `git pull` on launch. Manual is simplest and safest for v1.
- **Disk vs freshness.** 352 MB JSON mirror is the offline-complete
  option; the on-demand-fetch option trades offline for ~nil disk. Could
  support both via the settings toggle.
- **Submission flow.** We already stage TheDiscDB submissions (`submit*`);
  a local mirror is also the natural place to *preview* a submission
  against, and to detect "already catalogued" before submitting.
- **Licence/attribution.** `TheDiscDb/data` is open; confirm the licence
  permits redistribution if we ever ship a prebuilt index/snapshot rather
  than cloning on the user's machine.

## Recommendation

Worth doing — the outage proved the hosted endpoint can't be the only
path. Strategy 1 (sparse JSON clone + local hash index, live as fallback)
is the durable fix: offline, instant, outage-proof, ~3–4 days. Gate the
mirror behind an opt-in "download catalogue" action so users who don't
want 352 MB keep the live-only behaviour (now with honest error
reporting from commit `b02da5f`).
