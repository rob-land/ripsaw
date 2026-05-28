# Output naming

The naming layer takes (Identity, TitleSelection, output root, scheme)
and produces a target path. Schemes are first-class — switching scheme
in settings changes only the path-formatting logic, not the
identification or rip flow.

User picks one scheme as the "default" in settings, with a per-job
override exposed in the UI before the rip starts.

## Supported schemes

### Jellyfin

Reference: <https://jellyfin.org/docs/general/server/media/movies>

Movies:

```
<root>/Movies/
└── Avatar (2009) [imdbid-tt0499549]/
    ├── Avatar (2009).mkv
    ├── poster.jpg                          (optional, from TheDiscDB imageUrl)
    ├── trailers/
    │   └── Theatrical Trailer.mkv
    ├── behindthescenes/
    │   └── Making of Avatar.mkv
    ├── deleted/
    ├── featurettes/
    ├── interviews/
    ├── scenes/
    ├── shorts/
    └── extras/                             (catch-all for unclassified extras)
```

For 4K/2D/3D variants in the same folder, Jellyfin's
[multiple versions](https://jellyfin.org/docs/general/server/media/movies/#multiple-versions-of-a-movie)
convention suffixes the file: `Avatar (2009) - 4K.mkv`,
`Avatar (2009) - 3D.mkv`. 3drip uses these suffixes when emitting more
than one main-feature variant per title.

Series:

```
<root>/Shows/
└── The Expanse (2015) [imdbid-tt3230854]/
    └── Season 01/
        ├── The Expanse (2015) S01E01.mkv
        └── The Expanse (2015) S01E02.mkv
```

Multi-episode files use `S01E01-E02`.

### Plex

Reference: <https://support.plex.tv/articles/naming-and-organizing-your-movie-media-files/>

Movies:

```
<root>/Movies/
└── Avatar (2009) {imdb-tt0499549}/
    └── Avatar (2009) {imdb-tt0499549}.mkv
```

Plex's extras subfolders are case-sensitive and use the same set as
Jellyfin (`Behind The Scenes`, `Deleted Scenes`, `Featurettes`, `Interviews`,
`Scenes`, `Shorts`, `Trailers`, `Other`). 3drip emits both file *and*
folder name with the IDs in the form Plex prefers (`{imdb-…}`,
`{tmdb-…}`, `{tvdb-…}`).

### Kodi

Reference: <https://kodi.wiki/view/Naming_video_files/Movies>

Kodi is the least IDs-friendly — its scrapers do the matching from
filenames. The convention is just title + year:

```
<root>/Movies/
└── Avatar (2009)/
    ├── Avatar (2009).mkv
    ├── Avatar (2009)-trailer.mkv
    └── extras/
        └── Making of Avatar.mkv
```

Kodi recognises trailer files by the `-trailer` filename suffix; other
extras live in an `extras/` subfolder. We optionally write an `.nfo`
sidecar with the TMDB/IMDB IDs since Kodi's scrapers will consume that.

### Emby

Effectively identical to Jellyfin's scheme today (Jellyfin forked from
Emby). 3drip aliases Emby to the Jellyfin emitter; the option is exposed
separately to be future-proof.

## Module layout

```
naming/
├── mod.rs        · trait Scheme { fn movie_path(...); fn series_path(...); fn extras_dir(...); }
├── jellyfin.rs   · impl Scheme
├── plex.rs       · impl Scheme
├── kodi.rs       · impl Scheme
├── emby.rs       · re-exports Jellyfin
└── extras.rs     · TheDiscDB role → scheme-specific folder name
```

The `Scheme` trait is a pure function over inputs — no I/O — so the
implementations are exhaustively unit-tested.

## ID priority

For schemes that support an ID in the filename:

1. IMDB ID (most stable)
2. TMDB ID (universal across film + TV)
3. TVDB ID (TV only; if scheme prefers it, e.g. Plex)

If none are available, the ID segment is omitted entirely; the file
still has title + year and is matchable by scrapers.

## Sanitisation

Filename construction applies:

- Strip control characters and the POSIX path separator
- Replace platform-illegal characters (`:`, `?`, `*`, `"`, `<`, `>`, `|`)
  with a configurable replacement (default: space)
- Collapse whitespace
- Trim trailing dots and spaces (Windows-share friendliness, since users
  frequently host libraries on SMB)

A `naming::sanitise` helper is shared across all schemes and tested
against a corpus of awkward real-world titles
(`Lockout: M.A.X.`, `Wall·E`, `[REC]³ Génesis`, `S.W.A.T.`).

## Extras detection from TheDiscDB

`item.type` is normalised to one of:

```
main | trailer | behindthescenes | deletedscene |
featurette | interview | scene | short | other
```

`extras.rs` maps that enum to per-scheme folder names. Episodes have a
separate path (`series_path`).

## Conflict resolution

When the target file already exists:

- Default: refuse with an explanatory error, surface in UI.
- Override (per-job): append ` (1)`, ` (2)`, … until free.
- Never silently overwrite.

## Library config

Settings stores the root path per content type:

```toml
movies_root = "/home/$USER/Media/Movies"
shows_root  = "/home/$USER/Media/Shows"
```

These are picked via the FileChooser portal (Flatpak-friendly) and
persisted via the Document portal so the sandbox retains access.
