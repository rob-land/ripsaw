# libmvc — architecture survey (2026-05-30)

Research-mode notes for the eventual MVC decoder. The earlier
`docs/mvc3d.md` argued for "implement Annex G on top of a base H.264
decoder" without committing to which decoder. This doc picks one.

## Progress log

- **2026-06-15 (a).** Added the base H.264 SPS parser
  (`parse_seq_parameter_set_data`) — scaling lists, POC, full VUI + HRD
  consumption, and derived cropped luma dimensions — plus
  `parse_subset_sps_rbsp` chaining it with the MVC extension. Removes the
  last "delegate to libavcodec" gap for geometry. Verified against the
  real-world mvcC fixture in `tests/mvcc_real_world.rs`.
- **2026-06-15 (b).** Added the PPS parser (`pps.rs`,
  `pic_parameter_set_rbsp` incl. the FMO slice-group map and the
  transform_8x8 / scaling-matrix extension behind `more_rbsp_data()`) and
  the slice-header parser (`slice_header.rs`, `slice_header()` for base
  *and* MVC dependent-view slices, walking pred_weight_table and
  dec_ref_pic_marking, reusing `ref_pic_list_modification` for the
  inter-view IDCs). Added `BitReader::more_rbsp_data()`. **The whole
  front-end parser now runs end-to-end on a real 3D BD** — `examples/
  parse_slices.rs` decodes 576 base I-slices + 576 MVC P-slices
  (view_id=1) from a Friday-the-13th-Part-3 MVC rip with zero errors.
  The pure-Rust front end (NAL → RBSP → SPS/PPS → slice header) is now
  **complete**. What remains is the decode core: CAVLC/CABAC residual
  parsing, inverse transform, intra/inter prediction, deblocking, DPB,
  and the inter-view reference injection (Annex G § 8.2/8.4) — the
  Option-B-vs-C decision in this doc applies to that core, not the
  front end.
- **2026-06-15 (c).** Scoped the base-frame injection seam — the central
  risk for any faster-than-ldecod decoder — in `docs/libmvc-injection.md`.
  Key de-risking experiment: on a real 3D BD, **libavcodec's base view is
  byte-identical to ldecod's internal inter-view reference** (25/25
  frames, positional). So a fast decoder can supply exactly the reference
  the dependent view needs; the hybrid is sound at the data level. The
  remaining unknown is the *hook*, not the data — recommended next PoC is
  to replace JM's base decode with the injected libavcodec frame and
  confirm `ViewId0001` is unchanged.
- **2026-06-16.** Ran that PoC (`scripts/ldecod-base-inject.patch`,
  documented in `docs/libmvc-injection.md` § 2b). Patched JM ldecod to
  overwrite the base view's reconstruction from a file in `exit_picture`.
  Result: injecting **libavcodec's** base → dependent `ViewId0001`
  **96/96 identical** to stock; injecting a **perturbed** base → 96/96
  dependent frames changed. **The base-frame injection architecture is
  empirically settled end-to-end** — libavcodec base + JM dependent
  decode = correct stereo. No architectural unknowns remain for the
  Option B hybrid; what's left is engineering (carve JM's dependent
  decode into a library, or finish the Rust dependent decoder on the
  completed front end).

## What's already built

`src/mvc/` covers all the bitstream parsing we'll need:

| File | Purpose | Status |
|---|---|---|
| `bitstream.rs` | Bit reader + Exp-Golomb (`read_ue`, `read_se`) | done |
| `annexb.rs` | Annex B byte stream splitter | done |
| `nal.rs` | NAL unit header (incl. MVC extension types 14, 15, 20) | done |
| `rbsp.rs` | Emulation-prevention-byte removal | done |
| `sps.rs` | base `seq_parameter_set_data()` (§ 7.3.2.1.1, incl. scaling lists / VUI / HRD, derived cropped dims) + `seq_parameter_set_mvc_extension()` + `parse_subset_sps_rbsp()` | done |
| `ref_pic_list_modification.rs` | MVC inter-view IDCs 4 and 5 | done |
| `sei.rs` | Frame-packing-arrangement SEI parser | done |
| `mvcc.rs` | mvcC config record (ISO/IEC 14496-15 § 7.4) | done |
| `mkv_extract.rs` | Matroska BlockAddition walker | done |
| `decoder.rs` | `MvcDecoder` trait stub | stub |
| `layout.rs` | YUV → packed-stereo composer | stub |

The remaining work is **the decoder**: inverse transform + motion
compensation + deblocking + DPB management, for both views, with
inter-view prediction wiring between them.

## The fundamental tension

An MVC decoder needs to be able to **inject a reference frame from
the base view's reconstructed buffer into the dependent view's
RefPicList** (Annex G § 8.2). That is the entire delta versus plain
H.264 decode at the inter-prediction layer.

**No existing public-API H.264 decoder exposes that hook.** libavcodec,
openh264, libde265 -- all of them manage their own DPB internally and
treat the slice decoder as opaque. The decoder libraries we'd
naturally want to reuse are designed around "feed me a bitstream,
get back frames"; we need "feed me a bitstream PLUS a foreign
reference frame, get back frames".

So every path forward involves either patching the decoder, vendoring
it as source and modifying it, or porting from scratch. There is no
"clean external integration" option.

## Options surveyed

### A. Vendor + forward-port the Britz FFmpeg fork

History: `docs/mvc3d.md` § "Britz fork build spike" documents a
working build of FFmpeg 0.11 (2013) with Britz's MVC patches. The
MVC parsing + reference-management code is intact in `h264_mvc.c`;
the *output path* is a TODO comment from the original author so
the second view never reaches an `AVFrame`.

Plan: forward-port the MVC patches onto current FFmpeg 7.x, finish
the output side (expose `view_id` on `AVFrame` or as side data),
ship as an embedded helper library or a forked `libavcodec`.

Pros:
- Reuses libavcodec's heavily-optimised H.264 base decoder
  (SIMD, threading, hwaccel) for the base view.
- The MVC syntax tables exist already.
- All other Ripsaw codepaths already depend on FFmpeg.

Cons:
- libavcodec's H.264 decoder has been refactored substantially
  since 2013; forward-port is roughly a rewrite.
- Maintenance burden of a libavcodec fork forever.
- Output-path completion is itself a multi-week effort.
- Realistic effort: **2-4 weeks** for an h264-fluent contributor.

### B. Vendor JM (HHI reference), extract dep-view decode as a library

Plan: take JM's `mbuffer_mvc.c` + the macroblock decode primitives
it calls, expose them as a C library with an extra entry point that
accepts an externally-decoded base-view frame and a Subset SPS /
slice header pair. The base view is decoded by libavcodec at full
speed; the dep view runs through this trimmed-down JM library.

Pros:
- JM's MVC code is correct (it's the reference, by definition).
- The MVC delta is concentrated in `mbuffer_mvc.c` (856 lines) plus
  ~145 hooks across the rest of the codebase -- a tractable
  carve-out.
- Base view decode time -- the dominant cost -- moves to
  libavcodec immediately.

Cons:
- JM is single-threaded, no SIMD, reference-implementation speed.
  Even with base view offloaded, the dep view still runs at
  reference speed. Total throughput improvement maybe ~3-5x
  rather than the 10x+ we want.
- The "extract a sub-library" surgery is mechanical but tedious.
- License: JM is BSD-like (`COPYRIGHT_ISO_IEC.txt`,
  `COPYRIGHT_ITU.txt`), compatible with our GPL-3-or-later.

### C. Pure Rust port from spec

Plan: write the entire Annex G decoder in Rust, calling into a
fast base H.264 decoder (openh264 via the `openh264` crate, or
libavcodec via `ac-ffmpeg` / `rsmpeg`) for everything that isn't
MVC-specific.

Pros:
- Pure Rust API surface; no FFI mess.
- Long-term, the cleanest dependency story.

Cons:
- Same fundamental problem as Options A and B: the base decoder
  doesn't expose enough of its internals for us to inject
  foreign reference frames into the slice decode.
- The "MVC inter-view prediction is just regular inter-prediction
  with the base-view ref appended" (Annex G § 8.4) elegance
  collapses when the base decoder's slice path is private to that
  decoder.
- Realistic effort: **multi-month**.

### D. Sidestep: archive MV-HEVC, never decode MVC live

Plan documented in `docs/xreal.md` § Phased rollout. Ripsaw
transcodes MVC -> MV-HEVC at archive time (Phase C), the sister
player decodes MV-HEVC live (Phase D). For Linux + Xreal + Vision
Pro this is the entire use case.

In this world libmvc is **optional**, not blocking. Its purposes
shrink to:

1. Speeding up the MVC -> MV-HEVC archive step (today it's
   ldecod-throughput-bottlenecked).
2. Live playback of MVC archives the user hasn't transcoded yet
   (a convenience).
3. Stream-direct-from-disc (no rip file).

None of those are critical-path.

## Recommendation

**Pursue Option D as the primary strategy; defer Option B as the
"speed up MVC -> MV-HEVC archival" Phase F work.**

Concretely:

- Land Phase B (HEVC encode option) and Phase C (MV-HEVC archive
  output) on the existing ldecod backend, accepting the
  ldecod-throughput bottleneck (~5 minutes per 4 minutes of source
  material) for the one-time archival step.
- Ship the sister player (Phase D) so live playback works at
  realtime against MV-HEVC archives.
- Defer libmvc itself until either (a) the archival step's
  ldecod speed becomes a real pain, or (b) a "watch this disc
  without transcoding" workflow becomes interesting.

When we do build libmvc, **start with Option B** (carve dep-view
decode out of JM, base view from libavcodec). It's the lowest-risk
path to a working hybrid; Option A's forward-port can come after
if we want libavcodec's SIMD + threading on the dep view too.

## What to do *now* (this research session)

Given the recommendation defers actual libmvc implementation:

1. **Document the architecture decision** (this file).
2. **Update `docs/mvc3d.md` § Decoding strategy** to reflect the
   choice. (See follow-up commit.)
3. **Build a small `libmvc` probe** -- a CLI that runs the
   existing `src/mvc/sps.rs` parser on a real Subset SPS extracted
   from `samples/3D_LR_Pattern.mkv` and prints the parsed view
   IDs, anchor refs, non-anchor refs, level constraints. This
   verifies the parsers are correct against a real bitstream and
   gives us an artefact to point at when libmvc resumes.
4. **Save the strategic decision as a memory** so future sessions
   don't re-litigate it.

## Helpful crates found during the survey

- [`h264-reader` 0.8.0](https://crates.io/crates/h264-reader) -- a
  Rust H.264 bitstream parser. When libmvc work resumes the base
  SPS / PPS / slice-header parse can be delegated here rather than
  reimplemented; our `src/mvc/sps.rs` (Subset SPS MVC extension)
  layers on top.
- [`openh264` 0.9.3](https://crates.io/crates/openh264) -- idiomatic
  bindings to Cisco's BSD-licensed H.264 decoder. Same DPB-injection
  problem as libavcodec; not a magic bullet but a cleaner C API
  surface if the Option B JM-derived approach proves too brittle.
- [`mp4parse` 0.17.0](https://crates.io/crates/mp4parse) -- ISO BMFF
  parser. Useful for the Apple Vision Pro atom injection step
  (`vexu`/`hfov`/`lhvC`/`tapt`) in Phase E of `docs/xreal.md`.

## FFmpeg 7.1 MV-HEVC support verified on this host (2026-05-30)

`ffmpeg -h decoder=hevc` on the developer's Fedora system reports:

  -view_ids          Array of view IDs that should be decoded and output;
                     a single -1 to decode all views
  -view_ids_available  Array of available view IDs
  -view_pos_available  Array of view positions, as AVStereo3DView

Confirms Anton Khirnov's MV-HEVC patchset (FFmpeg 7.1, Sept 2024)
is present in the distro build. The Phase D sister player can
rely on FFmpeg's native MV-HEVC decoder without depending on a
pinned ffmpeg fork. Test corpus needed when prototyping: Apple's
published spatial-video samples or iPhone 15/16 Pro recordings.

## Open questions for whenever libmvc resumes

- Does libavcodec's H.264 `AVCodecContext` expose enough through its
  public API (or stable internal API) to inject our DPB? The
  `AV_PIX_FMT_DRM_PRIME` / hwaccel paths might be a wedge.
- Is there a maintained MVC patch series for FFmpeg anywhere (e.g.
  a fork of the Britz patches that someone else has forward-ported
  since 2013)? Quick search showed nothing in this survey but
  worth a periodic re-check.
- Does NVIDIA's NVDEC ever expose an MVC profile via VDPAU /
  NVDEC SDK? Last we checked, no. NVDEC supports H.264 base + High
  profiles but not Annex G; same for VAAPI and QSV.
- If we go Option B and have to ship a small JM-derived C
  sub-library: does the Fedora / Arch / Flatpak packaging story
  work? JM's license is permissive but unpopular.
