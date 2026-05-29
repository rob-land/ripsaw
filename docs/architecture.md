# Architecture

Ripsaw is a single GTK4/libadwaita application that orchestrates external
command-line tools (`makemkvcon`, `ffmpeg`, `mkvmerge`, and an MVC decoder)
and queries remote HTTP/GraphQL services (TheDiscDB, TMDB) to drive a
disc-to-media-library pipeline.

## Layering

```
┌────────────────────────────────────────────────────────────┐
│                   UI layer (libadwaita)                    │
│  AdwApplication · AdwWindow · stack of AdwNavigationPages  │
├────────────────────────────────────────────────────────────┤
│              Application services (Rust async)             │
│  Job manager · Settings · Library config · Tool detection  │
├──────────────┬─────────────┬──────────────┬────────────────┤
│  identify::  │   rip::     │ transcode::  │     mvc::      │
│  disc-hash   │  makemkv    │   ffmpeg     │  decode + SBS  │
│  thediscdb   │   driver    │   presets    │   /TAB / OU    │
│   tmdb       │             │              │                │
├──────────────┴─────────────┴──────────────┴────────────────┤
│                       naming::                             │
│  jellyfin · plex · kodi · emby · extras layout             │
├────────────────────────────────────────────────────────────┤
│           External tools (subprocess via tokio)            │
│        makemkvcon  ffmpeg  mkvmerge  mvc-decoder           │
└────────────────────────────────────────────────────────────┘
```

The UI never blocks. All disc/file/network work runs on a Tokio runtime
on background threads; results post back to the GTK main loop via a
`glib::MainContext::spawn_local` consumer reading from an
`async_channel::Receiver`.

## Module layout

```
src/
├── main.rs              · entry point, logging init, runs the AdwApplication
├── lib.rs               · re-exports modules for tests
├── application.rs       · AdwApplication subclass + GAction wiring
├── window.rs            · main window + navigation stack
├── ui/                  · per-page widget code (paired with .blp in data/resources/ui)
├── identify/
│   ├── disc_hash.rs     · TheDiscDB content-hash (MD5 over file sizes)
│   ├── thediscdb.rs     · GraphQL client
│   ├── tmdb.rs          · TMDB REST client
│   └── submit.rs        · contribution submission for unknown discs
├── rip/
│   ├── makemkv.rs       · makemkvcon driver: scan / mkv / version check
│   ├── makemkv_install.rs · "MakeMKV missing or too old" install helper
│   └── drive.rs         · optical drive detection (udisks2)
├── transcode/
│   ├── ffmpeg.rs        · ffmpeg subprocess driver, progress parse
│   └── presets.rs       · per-content-type encoding presets
├── mvc/
│   ├── decoder.rs       · MVC dependent-view decode (Britz ffmpeg fork or alt)
│   └── layout.rs        · SBS / HSBS / TAB / HTAB / Frame-Sequential composers
├── naming/
│   ├── jellyfin.rs      · `Movie (Year) [imdbid-tt…]/Movie (Year).mkv`
│   ├── plex.rs          · `Movie (Year) {imdb-tt…}.mkv`
│   ├── kodi.rs          · `Movie (Year)/Movie (Year).mkv`
│   ├── emby.rs          · same as Jellyfin in practice; aliased
│   └── extras.rs        · extras/behindthescenes/deletedscenes/featurettes subfolders
└── settings.rs          · GSettings-backed user config
```

## Job model

A *Job* is the end-to-end recipe for one disc, captured as a serialisable
struct:

```rust
struct Job {
    id: Uuid,
    disc_signature: DiscSignature,        // hash + makemkv-scan summary
    identity: Option<Identity>,           // populated by identify::
    selections: Vec<TitleSelection>,      // which titles, what role (main/extra/...)
    output: OutputPlan,                   // naming scheme + target dir
    transcode: Option<TranscodePreset>,
    mvc: Option<MvcOutputPlan>,           // 3D layout, half/full, etc.
    state: JobState,                      // pending → ripping → transcoding → naming → done | failed
}
```

Jobs are persisted as JSON under `$XDG_DATA_HOME/ripsaw/jobs/<uuid>.json`
so that the app can resume after a crash/reboot, and so the user can audit
what was done. The Job state machine advances by running stages
sequentially; each stage is a `tokio::task::JoinHandle`.

## Threading & async

- **GTK main thread**: all widget access. Stays responsive.
- **Tokio multi-thread runtime** (separate thread pool): owns subprocess
  pipes, HTTP clients, hashing, file I/O.
- **Bridge**: `async_channel::unbounded()` for "domain events"
  (job progress, identification result, tool detection result). UI
  consumes via `glib::MainContext::spawn_local` and updates widgets.
- No `Arc<Mutex<UiState>>` patterns. Domain state lives in the Tokio side
  and is communicated as events; the UI mirrors it.

## Configuration

- GSettings for user preferences (default naming scheme, default library
  root, default transcode preset, TMDB API key handling).
- Per-job overrides live in the Job JSON, never in GSettings.
- TMDB API key: prompted on first identification attempt; stored via
  libsecret (`oo7` crate), not in GSettings.

## Sandboxing & Flatpak

The app targets Flatpak. Key portal needs:
- `--device=all` for optical drive access (or finer-grained
  `--filesystem=/dev/sr0`).
- `--filesystem=host` opt-in for the chosen library directory; the
  preferred form is using the FileChooser portal and persisting the
  selected directory via Document portal.

Tool execution outside the sandbox is done via the Flatpak host-command
spawn — `makemkvcon` cannot reasonably be bundled because of its
licensing and beta-key lifecycle, so users install it on the host and
Ripsaw invokes it via `flatpak-spawn --host`.

## Logging

`tracing` with `EnvFilter`. Logs go to stderr; the UI exposes a "Show log
output" pane that subscribes to a `tracing` layer that pipes into the UI
via the same async channel.

## Testing strategy

- Unit tests for naming schemes (table-driven: input → expected path).
- Unit tests for disc-hash (golden file: known sizes → known MD5).
- Integration tests for `makemkvcon` output parsing using captured
  fixtures under `tests/fixtures/makemkv/`.
- The UI layer is not unit-tested; smoke tests run the app under a
  headless Wayland compositor when CI supports it.
