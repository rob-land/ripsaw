# Ripsaw

A Linux-native, GTK4 / libadwaita frontend around MakeMKV for ripping DVDs,
Blu-rays, and 4K UHD discs — with disc identification via
[TheDiscDB](https://thediscdb.com/), metadata enrichment via TMDB/IMDB,
Jellyfin/Plex/Kodi/Emby-compliant file naming, optional FFmpeg-based
transcoding with content-type presets, and 3D MVC Blu-ray conversion to
SBS/OU/etc.

The project name is read as "3D rip". The reverse-DNS application ID
(`land.rob.ripsaw`) is a placeholder and should be updated to an
`io.github.<owner>.Ripsaw` form once the canonical repo location is
decided.

## Status

**Working.** Disc identification, MakeMKV-driven ripping, Jellyfin/Plex/Kodi
naming, and the 3D MVC → SBS conversion pipeline are functional. The native 3D
decode path is bit-exact against the JM reference decoder on real 3D Blu-ray
content (both views) and is factored out as the standalone
[`libmvc`](https://github.com/rob-land/libmvc) crate, which ripsaw depends on.
See [`docs/`](docs/) for the per-subsystem design.

### Development

ripsaw pins `libmvc` to a released tag. To co-develop against a local `libmvc`
checkout, add an uncommitted `.cargo/config.toml`:

```toml
[patch."https://github.com/rob-land/libmvc.git"]
libmvc = { path = "../libmvc" }
```

## What this is not

- Not a re-implementation of MakeMKV. The MakeMKV engine remains required
  and is shelled out to via `makemkvcon`.
- Not a transcoder primarily — transcoding is opt-in via per-content-type
  presets; the default output is the source codec passed through into MKV.
- Not a media library manager — once a file lands in the configured
  library tree with a correct name, downstream Jellyfin/Plex/Kodi/Emby do
  the rest.

## Documentation map

| Doc | What's in it |
|---|---|
| [architecture](docs/architecture.md) | Module layout, threading model, IPC, data flow |
| [identify](docs/identify.md) | TheDiscDB lookup flow, fallback, manual entry, submission |
| [disc-hash](docs/disc-hash.md) | The TheDiscDB content-hash algorithm |
| [rip](docs/rip.md) | `makemkvcon` driver, startup check, install offer |
| [naming](docs/naming.md) | Jellyfin/Plex/Kodi/Emby schemes + extras subfolders |
| [transcode](docs/transcode.md) | FFmpeg pipeline and content-type presets |
| [mvc3d](docs/mvc3d.md) | 3D MVC decode pipeline and stereo layout conversion |
| [ui](docs/ui.md) | UI flow, libadwaita components, HIG notes |

## Building (future)

The project will use Meson as the top-level build, calling Cargo for the
Rust crate. The expected commands once code lands:

```sh
meson setup build
meson compile -C build
meson install -C build       # or: flatpak-builder
```

## Licence

[GPL-3.0-or-later](COPYING). The project is expected to link against
GPL-licensed FFmpeg builds (libx264 / libx265), which is incompatible
with a permissive licence on the application itself.

## External tool dependencies

| Tool | Why | Runtime check |
|---|---|---|
| `makemkvcon` | Disc decryption and stream extraction | Mandatory; see [rip](docs/rip.md) |
| `ffmpeg`     | Optional transcode, MVC frame ops | Optional |
| `mkvmerge` (mkvtoolnix) | MKV remux for naming/structure | Mandatory |
| MVC decoder (Britz FFmpeg fork or equivalent) | 3D MVC dependent-view decode | Optional, only for 3D BD |
