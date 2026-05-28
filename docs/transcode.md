# Transcoding pipeline

Transcoding is **opt-in**. The default flow is "rip the source codec
straight into MKV" with no quality loss. Users can attach a transcode
preset to a job; when present, the rip stage emits an intermediate MKV
which is then fed to FFmpeg.

## Why opt-in by default

- Lossless source preservation is the safer choice for archival.
- Modern players handle the source codecs (H.264, H.265, VC-1, AC3, DTS,
  TrueHD) natively.
- Transcoding doubles the disc-to-library time and is the most common
  source of "looks worse than the disc" complaints.

The transcode toggle is per-job and surfaced clearly: "Rip as-is" vs
"Rip and transcode with preset …".

## Preset library

Presets are TOML files shipped under `data/presets/`. Each captures the
intent for one content kind:

```toml
# data/presets/live-action-1080p.toml
name      = "Live-action 1080p"
target    = "x265"           # x264 | x265 | av1 | passthrough
crf       = 18
preset    = "slow"
tune      = "film"
hdr10     = "preserve"        # preserve | tonemap | strip
audio     = "passthrough"     # passthrough | aac:192k | opus:192k | ac3:640k
subtitles = "passthrough"

[notes]
description = "General-purpose live action. CRF 18 / x265 slow is visually transparent in side-by-side blind tests on a 1080p source."
```

Shipped baseline set:

| Preset | Encoder | CRF | Tune | Notes |
|---|---|---|---|---|
| `live-action-1080p` | x265 | 18 | film | balanced quality/size |
| `live-action-4k-hdr` | x265 (10-bit) | 20 | film | HDR10 preserve |
| `animation` | x265 | 19 | animation | tuned for cel/CG art |
| `high-action` | x265 | 16 | film | grain-heavy / fast-motion films |
| `sd-tv` | x264 | 19 | film | DVD-source TV, deinterlace |
| `archival-lossless` | x264 | 0 (lossless) | n/a | size-blind, perfect |
| `passthrough` | n/a | n/a | n/a | remux only; no re-encode |

Custom user presets live in `$XDG_DATA_HOME/threedrip/presets/` and the
UI offers a "duplicate to edit" action.

## Pipeline

```
        ┌─────────────────────┐
        │ Source MKV from rip │
        └──────────┬──────────┘
                   ▼
   ┌────────────────────────────┐
   │ probe with ffprobe -show_* │
   └──────────┬─────────────────┘
              ▼
   ┌────────────────────────────┐
   │ filter graph composition   │   (deinterlace if SD/interlaced;
   │  per preset                │    HDR tonemap if SDR target; etc.)
   └──────────┬─────────────────┘
              ▼
   ┌────────────────────────────┐
   │ ffmpeg invocation          │
   │  -hide_banner -progress …  │
   └──────────┬─────────────────┘
              ▼
   ┌────────────────────────────┐
   │ on success: mkvmerge remux │   (attach chapter + cover from disc;
   │  with metadata             │    strip intermediate streams)
   └──────────┬─────────────────┘
              ▼
        ┌─────────────┐
        │ naming step │
        └─────────────┘
```

`transcode::ffmpeg::run(preset, source, dest)` is the sole entry point.
Progress is parsed from `-progress pipe:1` lines (`out_time_ms`,
`fps`, `bitrate`) and posted to the UI.

## HDR

For HDR10 sources targeting an HDR10 output: copy static metadata
(`MaxCLL`, `MaxFALL`, mastering display) and the HEVC bitstream's
side-data through.

For HDR10 → SDR tonemap (presets that opt into it): use ffmpeg's
`zscale=tin=smpte2084:t=bt709,tonemap=hable:desat=0`. Bake this into
the preset's filter graph; do not expose it as a one-off CLI flag.

Dolby Vision passthrough is the eventual goal but depends on
`dovi_tool` and patched ffmpeg — out of scope for v1, tracked as a
follow-on.

## Audio

Default is passthrough. Conversion paths (when a preset asks):

| Source | Target | Tool |
|---|---|---|
| DTS-HD MA / TrueHD | AC3 5.1 @ 640k | ffmpeg `ac3` encoder |
| DTS-HD MA / TrueHD | AAC LC 5.1 @ 384k | ffmpeg `aac` |
| DTS-HD MA / TrueHD | Opus 5.1 @ 256k | ffmpeg `libopus` |

We never strip the original lossless track unless the preset is
explicitly size-targeted; the default is "keep original + add converted
track so all players work".

## Subtitles

Subtitle handling is **always passthrough**. The PGS streams MakeMKV
extracts from Blu-ray are not re-encoded; converting PGS → SRT is a
manual OCR step the user can perform later with a dedicated tool.

## Hardware encoding

Off by default. Hardware encoders (VA-API, NVENC, QSV) achieve much
lower quality at the same bitrate; for archival work CPU encoding is
the right default. We will expose a hardware-encode opt-in once the
preset library is mature, gated behind a "Faster but lower quality"
toggle.

## Cancellation

Like `makemkvcon`, ffmpeg exits on SIGTERM. Output file is deleted on
cancel.
