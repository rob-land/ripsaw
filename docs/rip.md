# Ripping: the `makemkvcon` driver

Ripsaw does not contain any decryption code. The MakeMKV engine (via the
`makemkvcon` CLI binary) handles AACS/BD+/CSS/AACS2 and stream
extraction. Ripsaw is purely an orchestrator.

## Startup health check

On application launch, `src/rip/makemkv.rs::probe()` runs:

```
makemkvcon --version
```

and parses the output. Three outcomes:

| Outcome | Action |
|---|---|
| Binary missing (`ENOENT`) | Show **Setup Required** page (see below). |
| Binary present, version OK | Continue to main window. |
| Binary present, version outdated | Show **MakeMKV out of date** page. MakeMKV's beta key expires periodically and old binaries can fail on newer discs. |

"Outdated" means: older than a cutoff version baked into Ripsaw (we will
bump it with releases) **or** the binary returns an error containing a
version-related signature when scanning a current disc.

## Setup Required page

When `makemkvcon` is missing or too old, the UI shows an AdwStatusPage
with three install-path buttons. We do not ship MakeMKV ourselves — it
is proprietary — but we automate the install paths:

1. **Install from official source (MakeMKV forum)**
   - Reference is the long-running forum thread:
     https://forum.makemkv.com/forum/viewtopic.php?f=3&t=224
   - That thread sometimes 404s or moves. We will mirror the *commands*
     it documents (download source tarballs from `www.makemkv.com/download/`
     for `makemkv-bin-X.Y.Z.tar.gz` and `makemkv-oss-X.Y.Z.tar.gz`,
     extract, `make && sudo make install`) into our own scripted helper
     so we are not coupled to the forum being online.
   - This is the only path that gets a *fresh* beta build.
2. **Install from distro repository**
   - Detect the distro (`/etc/os-release`) and offer the canonical
     install command for that distro (`dnf install makemkv` on Fedora
     via RPM Fusion non-free, `pacman -S makemkv` on Arch via AUR
     helper if present, `apt install makemkv-bin makemkv-oss` on Debian
     via an external PPA the user must add).
   - Distro packages lag the upstream beta; warn the user.
3. **Install from Flatpak**
   - `flatpak install flathub com.makemkv.MakeMKV` (when available).
   - Cross-sandbox invocation has additional complexity if Ripsaw is
     itself Flatpak — see [architecture](architecture.md#sandboxing--flatpak).

All three buttons run unprivileged commands and prompt for elevation via
`pkexec` only when needed. We never call `sudo` directly; if `pkexec` is
unavailable we degrade to printing the exact commands and asking the
user to run them in a terminal.

The Setup Required page also exposes a **Beta key** field. MakeMKV is
free during beta only with a beta key the user enters at
`~/.MakeMKV/settings.conf` (`app_Key = "T-…"`). Ripsaw writes this for
the user when they paste a key.

## Driving makemkvcon

`makemkvcon` operates in two phases.

### Phase 1: scan (`info`)

```
makemkvcon -r --noscan --messages=-stdout --progress=-stderr info disc:<n>
```

`-r` produces machine-readable records, one per line, in the form
`TYPE:ID,COL,VALUE,...`. We parse:

- `MSG` lines — status and error messages
- `DRV` lines — drive descriptions (disc inserted, label, type)
- `TINFO` / `SINFO` / `CINFO` — title, stream, content attributes
- `TCOUNT` — total title count

The scan populates: title index, duration, size, segments, languages,
codecs, source file (M2TS/VOB). This is also the input to the disc hash
(file sizes per title), although the canonical hash is taken from the
underlying VFS via direct UDF read where possible — see
[disc-hash](disc-hash.md).

### Phase 2: extract (`mkv`)

```
makemkvcon -r --noscan --messages=-stdout --progress=-stderr \
    mkv disc:<n> <title_index> <output_dir>
```

This is invoked once per selected title. `makemkvcon` writes a `.mkv`
file per title. We progress-track via `PRGV:current,total,max` lines and
update an AdwProgressBar.

We **always** ask MakeMKV to passthrough — no transcoding in
`makemkvcon`. Any codec conversion is a separate stage in our pipeline.

For 3D BDs, we ensure the MVC stream is selected (MakeMKV exposes it as
a separate track that must be ticked explicitly) — see [mvc3d](mvc3d.md).

## Drive detection

Use udisks2 over D-Bus to enumerate optical drives and watch for
media-insert events. This avoids polling `/dev/sr*` and gives us labels
("disc inserted", "tray open") for the UI. Sandbox-friendly via the
udisks2 portal in Flatpak.

## Cancellation

`makemkvcon` exits on SIGTERM. The job state machine sends SIGTERM and
waits up to 5s before sending SIGKILL. Partial output files are deleted
unless the user opts to keep them via a "keep partial output" toggle.

## Beta key lifecycle

The free beta key MakeMKV publishes rotates every ~30 days. Ripsaw will
*not* embed any key (legal and stability concerns). Behaviour:

- Detect "key expired" by parsing `MSG` codes from `makemkvcon`.
- Surface the user-visible MakeMKV forum URL where they fetch a new key.
- Provide a paste-target field; we write it to the user's MakeMKV
  config.

We never bundle, scrape, or auto-fetch the key.
