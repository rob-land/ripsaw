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

## Britz fork build spike (2026-05-28)

Cloned `github.com/Britz/FFmpeg @ mvc` (shallow, 45 MB). The `mvc`
branch has a single commit from **2013-02-21** — `initial MVC import to
git`. The fork is FFmpeg **0.11.1** (libavcodec 54.23), ~13 years and
~7 major versions behind current FFmpeg 7.x.

A minimal build against Fedora 43 / GCC 15 succeeds with three small
adjustments:

1. `--extra-cflags='-Wno-incompatible-pointer-types -Wno-int-conversion
   -Wno-implicit-function-declaration'` — modern GCC promotes the
   relaxed-C-rules diagnostics to errors by default; the fork's code
   pre-dates those tightenings.
2. `--disable-asm` — `libavcodec/x86/mathops.h` uses inline-asm `shr`
   forms current binutils rejects as operand-type ambiguous. Disabling
   asm trades performance for buildability; for our spike that's fine.
3. One-line patch to `libavcodec/h264_mvc.c`: insert
   `typedef unsigned int uint;` after the includes (the file uses the
   BSD `uint` shorthand which modern glibc no longer exposes via the
   includes already pulled in).

Result: `ffmpeg 0.11.1` and `ffprobe` binaries (2.4 MB each), with
`h264_mvc.o` linked into `libavcodec.a` and the MVC symbols
(`ff_h264_mvc_decode_nal_header`, `ff_h264_mvc_decode_sps`,
`ff_h264_mvc_decode_vui_parameters`, etc.) present. Build time ~30s on
a current desktop.

**What this does not prove:** end-to-end correctness on real MVC
streams. The fork ships no MVC test samples; we need an MVC bitstream
extracted via MakeMKV from a known 3D BD to validate.

**Verdict.** The decoder is *reachable* — we can produce a working
binary from the unmaintained 2013 fork with three small patches. It is
*not viable* as a long-term dependency: FFmpeg 0.11 is past every
distro's support horizon, has known unfixed CVEs, and is incompatible
with anything else in the pipeline that expects modern libav* APIs.

**Path forward, in order of attractiveness:**

1. **Forward-port the MVC patches onto current FFmpeg 7.x.** The MVC
   delta is contained: `libavcodec/h264_mvc.{c,h}` plus modifications
   to `h264.c`, `h264_refs.c`, `h264_ps.c`, and a few demuxer touches.
   The current FFmpeg h264 decoder structure has diverged but the
   conceptual changes (parse subset SPS, manage a second DPB, output
   paired frames) remain implementable. Realistic effort: 2–4 weeks of
   focused work by someone fluent in the h264 codec.
2. **Use the 2013 fork as an out-of-process binary (`threedrip-mvcdec`)
   for Phase 1.** Build it once with the spike patches, vendor under
   `third_party/ffmpeg-mvc-britz/` with the patch series, ship it as a
   private helper binary, validate against one real 3D BD, ship Phase 1
   SBS/HSBS output. Treat this as a six-month bridge while option 1 is
   pursued upstream.
3. **JM reference decoder (ldecod).** Slow but correct. Useful as a
   testing oracle to cross-check option 2's output and as the
   correctness baseline option 1 must match.

Recommendation: do option 2 to ship Phase 1, do option 1 in parallel
as the long-term dependency. Do not attempt to maintain a pinned 2013
ffmpeg as a production component.

## Britz fork sample validation (2026-05-28)

Tested the patched Britz binary against the [Kodi 3D
samples](https://kodi.wiki/view/Samples#3D_Formats) — 2 MKVs and 6 BD
ISOs. Two distinct format families surfaced, with different outcomes
for each.

### Modern MakeMKV `mvcC` MKVs — Britz cannot read

`3D_LR_Pattern.mkv` and `3D MVC Resolution test.mkv` were both produced
by MakeMKV v1.18.2. `mkvinfo` confirms the layout:

- Single video track, `Codec ID: V_MPEG4/ISO/AVC`, High @ L4.1, 1920×1080
- `Block addition mapping`, type `1836475203` (`mvcC` — MVC configuration record)
- `Video stereo mode: 13` (both eyes laced in one block, left first)

In this layout the dependent-view data is carried as Matroska
`BlockAddition` extensions, not packed inline in the H.264 elementary
stream. The Britz fork's matroska demuxer (FFmpeg 0.11, 2012) has no
notion of `mvcC` block additions — they didn't exist yet — so the
dependent view is invisible to it. Decode attempt fails immediately with
"Invalid data found when processing input."

This format is what *anyone* using current MakeMKV will produce, which
makes Britz directly incompatible with the typical 3drip workflow.

### Physical-BD SSIF — Britz parses, doesn't emit second view

Mounted `3DLR-patterns.iso` to inspect its `BDMV/STREAM/` layout:

- `00000.m2ts` (87 MB) — base view alone, decoded fine by stock ffmpeg
- `00000.ssif` (157 MB) — Stereoscopic Interleaved File: base + dependent views interleaved

`ffprobe` on the SSIF surfaces two video streams, the first with
`profile=unknown` and `width=height=0` — the dependent-view fingerprint
modern ffmpeg can't decode but can demux.

Britz on the SSIF:

- demuxes both streams
- logs `Sub SPS (1) activated.` and `decode MVC NAL unit with view 0` /
  `view 1` — the MVC parsing path is reached
- still emits **only the base view** through the CLI: a 10-second
  segment yields exactly 240 frames (24fps × 10s), not 480
- `-map 0:v:1` is rejected because Britz exposes only one logical
  video stream

Grepping the fork's `h264.c` reveals why:

```c
// JB  for mvc generate some different output!
// EDIT JB
if(h->is_mvc){
    av_log(avctx, AV_LOG_INFO, "outputting MVC view %d\n", h->view_id);
}
// END EDIT
```

The dependent-view output path is a TODO comment from the original
author. The decoder runs inter-view prediction and tags pictures with
`view_id` on the internal `Picture` struct
(`mpegvideo.h:148: int view_id; ///< H264 MVC view identifier`), but
`AVFrame` carries no view_id field and no muxer or filter graph in the
fork distinguishes the two views.

### Conclusion

The Britz fork is an **incomplete MVC decoder**, not just a stale one.
Even with a custom C wrapper using the internal `Picture` API, we would
have to *finish* the output side the original author marked as a TODO
in 2013 — implement view splitting, expose `view_id` on `AVFrame` (or
as side data), and probably restructure the picture-output queue to
emit both views per coded frame.

This downgrades option 2 from the original recommendation ("use Britz as
Phase 1 helper binary"). Britz still has value as **patch reference
material** for the forward-port, but it is not a usable building block
on its own.

Updated recommendation:

1. **Forward-port the MVC patches to current FFmpeg 7.x**, with the
   explicit scope of finishing the output path Britz left unfinished.
   Modern FFmpeg exposes `AVFrameSideData` for stereo metadata, which
   is the right place to land `view_id`.
2. **Cross-check correctness against the JM reference decoder
   (`ldecod`)** — it actually does emit both views, slowly but
   reference-correctly, and is a known-good oracle for any
   forward-ported decoder's output.
3. **Re-target our matroska handling for `mvcC` block additions**.
   Since modern MakeMKV emits `mvcC`-format MKVs, that's the
   container-side work we need regardless of decoder. We will need
   either to handle `mvcC` in the demuxer ourselves or to use
   `mkvextract` to dump the elementary stream + extension data and
   reassemble.
