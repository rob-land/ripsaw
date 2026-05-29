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

**We implement Annex G ourselves as a standalone library.** Earlier
plans considered the Britz FFmpeg fork as a runtime helper or a forward-
port target; both are dead ends (see § "Britz fork build spike" and
§ "Britz fork sample validation" below). The decoder we ship is new
code, written from the H.264 spec, reusing an off-the-shelf base-view
H.264 decoder for everything that isn't MVC-specific.

### Why a standalone library instead of patching FFmpeg

FFmpeg has no runtime plugin API for decoders — every supported codec
is built into `libavcodec` at FFmpeg compile time. The only ways to
"add a decoder to FFmpeg" are to patch FFmpeg's source tree (either
ours, locally, or upstream via review) or to vendor a fork. Both
couple our release cadence to FFmpeg's. Worse, the H.264 decoder in
FFmpeg has been refactored heavily since the era when MVC patches were
written; any forward-port is a rewrite.

A standalone library sidesteps all of this:

```
┌──────────────────────────────────────────┐
│  3drip GTK app (Rust)                    │
└────────────────────┬─────────────────────┘
                     │ FFI (cxx or bindgen)
                     ▼
┌──────────────────────────────────────────┐
│  libmvc                                  │
│  ┌────────────────────────────────────┐  │
│  │  base-view H.264 decoder           │  │  ← upstream libavcodec
│  │  (linked unmodified)                │  │     (LGPL, system or vendored)
│  └────────────────┬───────────────────┘  │
│                   ▼                      │
│  ┌────────────────────────────────────┐  │
│  │  Annex G additions (our code):     │  │  ← implemented from spec
│  │  - subset SPS / MVC SPS extension  │  │
│  │  - per-view DPB                    │  │
│  │  - inter-view reference list ctor  │  │
│  │  - inter-view prediction wiring    │  │
│  │  - view-id-aware picture output    │  │
│  └────────────────────────────────────┘  │
│                                          │
│  API: feed_packet(au) ->                 │
│         { left: Frame, right: Frame }    │
└──────────────────────────────────────────┘
```

Properties:

- **No FFmpeg version coupling.** Works against whatever system FFmpeg
  ships — we use the public `libavcodec` C API for the base view.
- **No upstream review required.** No patches to maintain.
- **MVC-only surface.** We implement just the Annex G delta —
  everything else (NAL parsing, slice header parsing, inverse
  transform, deblocking, intra/inter prediction, CABAC/CAVLC) is reused
  from a battle-tested decoder.
- **Bit-exact testable.** ITU-T H.264 § 8 and § G.8 say "all decoders
  shall produce numerically identical results"; we cross-check our
  output against the JM reference decoder (ldecod) on a fixed corpus.
- **Licence-clean.** GPL-3-or-later for our additions; linking against
  LGPL libavcodec is fine. Could also be relicensed LGPL later if we
  ever want to upstream the additions.

### Language

Either C or Rust works. C makes FFI to `libavcodec` trivial (it's a C
API). Rust gives us safer DPB management (the bug surface is largely
"out-of-bounds index into reference picture list") and matches the
rest of 3drip. Bias: **Rust**, with `unsafe` FFI to `libavcodec` for
the base-view decoder. Decision deferred to libmvc's own
sub-architecture doc; not on the critical path for now.

### The Rust-side abstraction stays simple

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

The implementation is a `LibmvcDecoder` that wraps our library; for
correctness testing we also keep a `JmReferenceDecoder` (shells to
ldecod) gated behind a `cfg(test)`-ish opt-in feature.

## Bitstream spec reference

The implementation reference is the August 2024 ITU-T H.264
recommendation (V15), specifically:

| Annex / Clause | What it covers | Why we need it |
|---|---|---|
| Main spec § 6–9 | Base H.264 decode | Reused from libavcodec — not our code |
| **Annex A** § A.2.5 | High profile family | Base view is High @ L4.1 on Blu-ray 3D |
| **Annex B** | Byte stream format | Demuxer concern; reused |
| **Annex G** | Multiview Video Coding (MVC) | **The actual delta we implement** |
| Annex G.10.1.2 | Stereo High profile | Profile we target |
| Annex G.13 | MVC SEI messages | Optional metadata (view scalability info, multiview scene info) |
| Annex G.14 | MVC VUI extension | Aspect / colour signalling per view |

We **do not** implement:

- Annex F (SVC), G.10.1.1 (Multiview High beyond two views), G.10.1.3
  (MFC High), Annex H (MVCD — multi-view + depth), Annex I (3D-AVC).
- These cover scalability, multi-view (N>2), and depth-augmented 3D
  features that are not used on commercial Blu-ray 3D.

### Annex G implementation surface

The clauses that map to actual code:

| Clause | Process | Code lives in |
|---|---|---|
| G.7.3.2.1.1 | Subset SPS RBSP syntax | `libmvc/parse/subset_sps.rs` |
| G.7.3.2.1.2 | MVC SPS extension | same |
| G.7.4.1 | NAL unit semantics (incl. types 14, 15, 20) | `libmvc/parse/nal.rs` |
| G.7.3.3 / G.7.4.3 | Slice header MVC extension | `libmvc/parse/slice_header.rs` |
| G.8.1 | Picture order count per view | `libmvc/dpb/poc.rs` |
| G.8.2 | Reference picture list construction (with inter-view refs) | `libmvc/dpb/ref_lists.rs` |
| G.8.3 | Per-view decoded reference picture marking | `libmvc/dpb/marking.rs` |
| G.8.4 | Inter-view prediction wiring (reuses § 8.4) | `libmvc/predict.rs` |
| G.8.5 | Sub-bitstream extraction | `libmvc/extract.rs` |
| G.10 | Profile / level constraints (Stereo High) | `libmvc/conformance.rs` |

The "reuses § 8.4" line is the key simplification: G.8.4 says
inter-view prediction *is* base inter-prediction with the inter-view
reference picture appended to the regular reference list. We don't
write a new motion-compensation engine — libavcodec already has one;
we just feed it an extended `RefPicList`.

### Conformance testing

ITU H.264 § G.8 mandates: "any decoding process that produces
identical results for the target output views to the process described
here conforms to the decoding process requirements." This is testable:

- Cross-check our YUV output against the JM reference decoder
  (`ldecod`) on a fixed corpus.
- Fixture inputs: extracted SSIF clips from physical 3D BDs + any
  `mvcC`-format MKV we have demuxer support for.
- Tolerances: bit-exact for luma; the spec mandates this, so anything
  else is a bug.

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
| ~~**0** (sketch)~~ | ✅ Done. Design doc + module stubs. |
| **1** (in progress) | `libmvc` skeleton: subset SPS / MVC SPS parse, NAL types 14/15/20 routing, per-view DPB scaffolding, golden-frame test harness against `ldecod`. Single SSIF input, raw YUV output of both views, no integration into 3drip yet. **Sub-progress:** bit reader + Exp-Golomb + RBSP extraction + NAL header parsing + Subset SPS MVC extension + minimal Matroska EBML walker + mvcC (MVCDecoderConfigurationRecord) reader (29 unit + 2 real-world integration tests). The real-world test extracts the mvcC BlockAddIDExtraData from `samples/3D_LR_Pattern.mkv` and confirms profile_idc 128 (Stereo High), level 41, and a type-15 Subset SPS NAL whose RBSP starts as expected. Remaining for phase 1: slice-header MVC extension, per-view DPB, inter-view prediction wiring against an upstream H.264 base-view decoder. |

### Build / tooling state (2026-05-28)

- **`ldecod`** built from `https://vcgit.hhi.fraunhofer.de/jvet/JM` at
  `/home/rob/3rdparty/JM` with `cmake -B build` + relaxed `-Werror`
  (modern GCC's `-Werror=maybe-uninitialized` etc. trip the legacy
  source; warnings are dropped to non-fatal via `CMAKE_C_FLAGS`).
  Binary at `bin/umake/gcc-15.2/x86_64/release/ldecod`. Wrapper at
  `scripts/ldecod` for convenience.
- **JVT MVC conformance bitstreams** are not staged. The samples in
  `samples/` (modern MakeMKV-produced 3D MKVs + several 3D BD ISOs)
  are real MVC content from production discs and cover the cases
  we'll meet in the wild. A formal JVT conformance set is worth
  adding for finer edge-case coverage but is not blocking phase-1.
- **Real-world fixture** at `tests/fixtures/mvc/3d_lr_pattern.mvcc.bin`
  (258 bytes) is the mvcC payload extracted from
  `3D_LR_Pattern.mkv`. The fixture lets the real-world integration
  test pass even when the sample collection is missing.
| **2** | `libmvc` correctness: ref-list construction (G.8.2), inter-view prediction wiring (G.8.4), bit-exact YUV match against `ldecod` on ≥3 SSIF fixtures spanning the corpus. |
| **3** | Integration: Rust `LibmvcDecoder` impl of `MvcDecoder` trait, SBS / Half-SBS composer. End-to-end "disc → SBS MKV" on one real 3D BD. |
| **4** | TAB / Half-TAB / Frame-Sequential / Interleaved layouts. Hardware-accelerated composition (VA-API). |
| **5** | Subtitle depth via BDSup2Sub++. `mvcC`-format MKV input support (un-pack BlockAdditions to elementary stream). |
| **6** | Optional: Wine+FRIM fallback (only if a user reports a sample we can't handle). Dolby Vision Profile 7 if it overlaps. |

Each phase ends with a regression test gate. Phase 2's bit-exact match
against ldecod is the hard correctness milestone; everything later is
plumbing.

## Risks

- **Implementation effort.** Realistic 3–6 weeks of focused work to
  reach phase-2 correctness for someone fluent in H.264 internals.
  This is the project's largest single subsystem by code volume.
  Mitigation: ldecod is the truth oracle; iteration is local, not
  upstream-review-bound.
- **Spec ambiguity in corner cases.** ITU specs are normative but
  occasionally under-specified for the edges (skipped pictures across
  IDRs, gaps in `frame_num`, exotic POC type combinations).
  Mitigation: bit-exact diff against `ldecod` catches drift before it
  ships.
- **Bitstream-conformance corpus.** We need real-world MVC samples
  with known-good decodes to drive the test suite. Source: physical
  3D Blu-ray SSIF clips + samples from the
  [Kodi 3D sample page](https://kodi.wiki/view/Samples#3D_Formats)
  that contain MVC. JVT conformance bitstreams are the gold standard
  for individual decode-process clauses.
- **Quality regressions in the composer.** SSIM/PSNR regression suite
  against a small set of reference SBS clips; CI runs it on every push
  touching `libmvc/` or `src/mvc/`.
- **Patent / licence.** MVC is patent-encumbered, similar to H.264.
  Risk is the same as shipping any H.264 build (e.g. x264). Our GPL-3-
  or-later application licence is compatible. libmvc itself will be
  licensed GPL-3-or-later to match.

## Vendored reference material

`third_party/ffmpeg-mvc-britz/` contains the Britz fork's MVC delta as
patches + the new `h264_mvc.{c,h}` files. After the build / sample-
validation spikes (below), Britz is **not** the implementation target
— we write from spec — but the algorithm in `h264_mvc.c` is one of two
freely-available worked examples of the MVC decode process (the other
being `ldecod`), and serves as a reference when the spec text is
ambiguous.

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

This downgrades option 2 from the original recommendation ("use Britz
as Phase 1 helper binary"). Britz still has value as **reference
material** for our own implementation — its `h264_mvc.{c,h}` files are
one of two freely-available worked examples of the MVC decode process
(the other being `ldecod`) — but it is not a usable building block on
its own.

Both Britz-as-helper-binary and Britz-as-forward-port-target are
abandoned in favour of the standalone `libmvc` approach described
above in § "Decoding strategy". The remaining work after this spike:

1. **Write `libmvc` from spec.** Annex G is detailed enough; see
   § "Bitstream spec reference" for the clause-to-code mapping.
2. **Cross-check correctness against the JM reference decoder
   (`ldecod`).** It emits both views, slowly but reference-correctly,
   and is the oracle for our bit-exact tests.
3. **Container support for `mvcC` BlockAdditions.** Modern MakeMKV
   emits `mvcC`-format MKVs, so we need either `mvcC` support in our
   demuxer or a `mkvextract`-based preprocessor that dumps the
   elementary AVC + MVC NALs to a single stream `libmvc` can consume.
