# 3D MVC pipeline

This is the riskiest subsystem. Linux has no maintained MVC
(Multiview Video Coding, the H.264 extension Blu-ray 3D uses) decoder
in the upstream FFmpeg. BD3D2MK3D — the canonical 3D BD converter —
runs on Windows and depends on the proprietary `FRIMSource.dll`
AviSynth filter for MVC decode. We must build the decoder pipeline
ourselves.

## What MakeMKV produces

When the user ticks the MVC track during a 3D Blu-ray rip, MakeMKV
emits an MKV containing **two video tracks**:

- the AVC base view (compatible with any H.264 decoder; this is the
  2D view)
- the MVC dependent view, marked as `Mpeg4-MVC-3D`, which encodes
  inter-view residuals relative to the base view

Both tracks share frame timing. Decoding the dependent view requires
the base view as a reference — they cannot be decoded independently.

## Decoding strategy

Three candidate decoders, in declining order of preference:

1. **Britz FFmpeg fork (`mvc` branch)**
   - <https://github.com/Britz/FFmpeg/tree/mvc>
   - Modifies `libavcodec/h264dec.c` to handle dependent views; exposes
     them via the standard demuxer.
   - Status: stale (last update mid-2010s) but builds against modern
     ffmpeg with patches.
   - Plan: bundle a pinned commit + a small patch series in
     `third_party/ffmpeg-mvc/` and build it as a *separate binary*
     `threedrip-mvcdec` shipped alongside the main app. The main app
     spawns it via subprocess.
   - This isolates the GPL-only / unmaintained code from the main
     binary and keeps 3drip itself buildable against stock ffmpeg.

2. **JM reference decoder (ldecod)**
   - The H.264 reference implementation supports MVC natively.
   - Pros: actively-maintained-ish, reference-grade correctness.
   - Cons: very slow (orders of magnitude slower than libavcodec).
   - Used as a fallback / correctness oracle in tests.

3. **Wine + FRIM**
   - Last-resort fallback. Implementable behind a feature flag.
   - User installs Wine + FRIM; we pipe via AviSynth+ for Linux
     (`avisynth-plus` package) or fall through to native if it ever
     ships.
   - Not on the v1 critical path; documented for completeness.

The decoder is abstracted behind a trait:

```rust
// src/mvc/decoder.rs
pub trait MvcDecoder: Send {
    fn decode(&mut self, src: &Path) -> impl Stream<Item = StereoFrame>;
}

pub struct StereoFrame {
    pub pts: i64,
    pub left:  yuv::Frame,
    pub right: yuv::Frame,
}
```

Implementations: `BritzFfmpegDecoder`, `JmReferenceDecoder`,
`WineFrimDecoder`. Selection is at runtime based on what is installed;
the user picks in settings when more than one is available.

## Output layouts

Once we have a `Stream<StereoFrame>`, the layout composer builds the
output frame and pipes raw frames into x264/x265 (or AV1, where the
encoder supports stereo packing metadata).

| Layout | Frame composition | Use case |
|---|---|---|
| Full-SBS | 3840×1080 (left │ right) | TVs/projectors with frame-packing support, no quality loss |
| Half-SBS | 1920×1080 (downsampled 960 │ 960) | most 3D TVs, broad compatibility |
| Full-TAB | 1920×2160 (left over right) | rare; some 3D projectors |
| Half-TAB | 1920×1080 (downsampled 540 │ 540) | common alternative to HSBS |
| Frame-Sequential | 1920×1080 @ 2×fps | active-shutter; size-efficient |
| Over/Under | alias for TAB | UI convenience |
| Interleaved | 1920×1080 odd/even line pack | passive 3D displays |

Composer code lives in `src/mvc/layout.rs`; each layout is a pure
function over `(StereoFrame) -> ComposedFrame` plus a `(width, height,
fps)` descriptor that drives the ffmpeg input pipe.

## Quality goals

Per the original spec — keep image quality as high as possible,
especially for Full-SBS. Three rules:

1. **Never re-encode the base view to feed the composer.** Decode →
   compose → encode once. Encoding twice (e.g. decode → encode left,
   decode → encode right) doubles loss.
2. **Use full chroma resolution end-to-end.** Source MVC is 4:2:0 8-bit;
   the composer must not silently downconvert. For 4K-3D (rare, but
   exists) preserve 10-bit.
3. **Match source frame rate exactly.** Most 3D BDs are 24p; deliver
   24p. Frame-Sequential output doubles the rate to 48p (or 2×source).

## Subtitle plane offsets

3D BD subtitles use the *3D-Planes* (Offset Sequences) stored in the
MVC stream. Each subtitle track carries a plane number; the player
applies an x-axis offset per frame to render the subtitle at the right
depth. BD3D2MK3D's `Mkv3DPlanesHelp` text explains the mechanic.

For SBS/TAB output we must bake the depth offset into the subtitle
render — there is no plane metadata in a flat-stereoscopic file.
Approach:

- Convert PGS to image-based BD-SUP3D using the stream's plane number.
- Composite the rendered subtitle bitmap into the left and right halves
  at the appropriate offset, derived from the per-frame plane value.
- Optionally hardcode subtitles into the video, which is the highest-
  compatibility approach but irreversible.

BDSup2Sub++ (Java) is the canonical tool for this conversion and has a
Linux build. We will shell to it when available; otherwise fall back to
copying subs through without depth.

## Implementation phasing

| Phase | Scope |
|---|---|
| **0** (sketch) | This document + module stubs. No 3D code runs. |
| **1** | `BritzFfmpegDecoder` bundled + builds; Full-SBS and Half-SBS layouts only. PGS passed through without depth. |
| **2** | TAB / Half-TAB / Frame-Sequential layouts. Subtitle depth via BDSup2Sub++. |
| **3** | JM reference fallback. Hardware-accelerated composition (vaapi vpp_qsv overlay). |
| **4** | Wine+FRIM fallback. Dolby Vision Profile 7 (MEL/FEL) where it overlaps with MVC discs. |

Phase 1 ships only after we have a Britz fork tree that builds against
the current stable ffmpeg and a regression test that decodes a known
3D BD sample to bit-identical YUV against a Windows FRIM reference.

## Risks

- **Bundled MVC decoder bit-rots.** The Britz fork is unmaintained.
  Plan: vendor it under `third_party/`, pin the commit, and treat
  decoder upgrades as discrete tasks per ffmpeg major bump.
- **Quality regressions are hard to catch.** Need an SSIM/PSNR
  regression suite against a small set of reference clips; CI will run
  it on every push touching `src/mvc/` or `third_party/ffmpeg-mvc/`.
- **Patent/licence.** MVC is patent-encumbered, similar to H.264. The
  decoder bundle inherits whatever risk x264 builds already carry; the
  project's GPL-3 licence is compatible with our use.
