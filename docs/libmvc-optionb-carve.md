# Option B — JM dependent-view carve: engineering scope

Status: scoping doc (2026-06-16). Builds on `docs/libmvc-injection.md`
(injection proven end-to-end; ~1.6× payoff with HW encode) and
`docs/libmvc.md` (Option B chosen as the eventual decoder path).

## Goal

Replace the single "ldecod decodes both views" step with:

```
  base NALs ───▶ libavcodec (fast)  ──▶ base YUV ┐
  dependent NALs ─────────────────────────────────┼─▶ mvcdep (JM-derived,
                                                   │   dependent-only decode
                                                   │   with base injected)
                                                   ▼
                                            dependent YUV ─▶ compose ─▶ encode
```

The measured win comes from **not running JM's base-view macroblock
decode** (4.98 s / 400 frames) and substituting libavcodec's base
(0.39 s). Crucially, the injection PoC in `docs/libmvc-injection.md` § 2b
left JM's base decode *in place* (it overwrote the result), so it saved
nothing — the carve's entire value is skipping that base MB decode while
keeping the dependent decode correct.

## Where the speedup actually lives (the surgery)

JM's per-AU flow (`source/app/ldecod/image.c`):

```
decode_one_frame()                         // 807
  while header != EOS:
    read_new_slice()                       // 1341  parse slice header  (CHEAP — keep)
    decode_slice()                         // 731
      if entropy_coding: init_contexts()   // 733   CABAC init
      decode_one_slice()                   // 746 -> 2562  MB decode     (EXPENSIVE — skip for base)
  ...deblock...
  exit_picture()                           // 1875  finalize + store in DPB
```

`currSlice->view_id` is 0 for base, 1 for dependent (image.c:277). The
carve is **not** "extract `mbuffer_mvc.c`"; it is three localized edits:

1. **Skip base MB decode.** In `decode_slice` (line ~733–746), when
   `currSlice->view_id == 0`, skip `init_contexts`/`cabac_new_slice` and
   the `decode_one_slice()` call. `read_new_slice()` still runs, so POC,
   `frame_num`, and the reference-marking process (sliding window / MMCO)
   for the base view stay exactly correct — that state is what the
   dependent view's inter-view + temporal ref lists depend on.
2. **Skip base deblocking** (it would filter garbage and be overwritten).
3. **Fill base pixels from the injected frame** in `exit_picture` — the
   existing PoC hook (`scripts/ldecod-base-inject.patch`) becomes the
   *sole* source of base pixels rather than an overwrite. It already
   targets the right spot (just before `pad_dec_picture` + DPB store).

Everything else — base DPB management, inter-view list construction in
`mbuffer_mvc.c` (`get_inter_view_pic`, `init_lists_*_slice_mvc`,
`append_interview_list`), and the entire dependent-view decode — runs
unchanged. The PoC already proved the dependent decode is bit-exact when
the base picture is externally supplied; this just stops paying to
decode the base twice.

### Why skipping base MB decode is safe (profile assumption)

Stereo High (profile_idc 128, what Blu-ray 3D uses) inter-view
prediction is **sample-only**: the dependent view has its own motion
vectors into the base *picture*; it never derives MVs/modes from the base
view's macroblock data. (The perturbation control in
`docs/libmvc-injection.md` § 2b confirmed only base *pixels* affect the
dependent output.) So the base view's per-MB buffers are not needed once
its reconstructed samples are present. **MFC / 3D-AVC profiles do use
base MB data and are explicitly out of scope.**

## Realization: subprocess, not a linked library

Two ways to package it:

| | Subprocess (`mvcdep`, patched ldecod) | Extracted C lib + Rust FFI |
|---|---|---|
| Effort | ~1 week | ~3–4 weeks |
| JM global state (`p_Vid` everywhere, file I/O) | left as-is | must be untangled into a re-entrant API |
| Integration | matches today's `ldecod` subprocess in `runner.rs` | bindgen/cxx, lifetime/ownership of DPB buffers across FFI |
| Overhead | base YUV over a pipe (~3 MB/frame) | zero-copy in-proc |

The subprocess wins decisively: the current pipeline already shells out
to `ldecod`, JM's heavy global state makes a clean library API a project
in itself, and the pipe/handoff overhead is negligible against a
multi-minute convert. **Recommend the subprocess (`mvcdep`) approach;**
defer a true library to Option A territory if we ever want SIMD/threading
on the dependent view too.

### Base-frame feed

`ffmpeg` decodes the base view to a fifo/pipe (raw `yuv420p`); `mvcdep`
reads one base frame per base AU in `exit_picture`. The two processes run
concurrently, so base decode overlaps dependent decode for free.

**Order.** `exit_picture` sees base AUs in *decode* order; ffmpeg emits
*display* order. For this corpus they coincide (no B-frames in the base
view — `docs/libmvc-injection.md` § 2b, 96/96 positional). For a B-frame
base GOP they would not. Mitigation: tag each injected frame with its
POC (ffmpeg `-fflags +bitexact`, or decode-order output via
`-flags +low_delay` / reordering off), and have `mvcdep` match by POC
instead of read-sequential. This is the **main open correctness item**;
everything else is proven.

## Ripsaw integration (`src/convert/runner.rs`)

Today `run_mvc_pipeline` does: extract Annex B → `ldecod` (both views) →
ffmpeg compose+encode. New path for `MvcWithBlockAdditions` /
`MvcInlineLaced` when `mvcdep` is available:

1. Extract Annex B (unchanged — `mvc::mkv_extract`).
2. Spawn `ffmpeg` base-decode → fifo `base.yuv`.
3. Spawn `mvcdep -f cfg` with `RIPSAW_BASE_INJECT=base.yuv` → dependent
   YUV (and a passthrough base YUV for compose, which is just the fifo
   tee'd or libavcodec's frames reused).
4. Compose (`hstack`/etc.) + encode — unchanged.

Resolution mirrors `resolve_ldecod_path`: prefer `mvcdep` on PATH /
`$RIPSAW_MVCDEC`, fall back to the existing both-views `ldecod` path so
nothing regresses when `mvcdep` isn't built.

## Validation plan

The injection PoC already validated "external base → correct dependent".
The carve adds one new risk: *skipping base MB decode must not perturb
base DPB/POC/marking state.* Test exactly as the PoC did:

- `mvcdep` dependent output (`ViewId0001`) **bit-exact** vs stock
  `ldecod`'s `ViewId0001`, across the sample corpus (not just one clip).
- Spot-check streams with: multiple SPS/PPS, MMCO in the base view,
  long-term references, scene cuts (IDR mid-stream), and — if any exists
  in the corpus — a B-frame base GOP (the order item above).
- Re-time end-to-end to confirm the predicted ~1.6× (HW encode) lands.

## Effort & sequencing

| Task | Est. |
|---|---|
| `mvcdep` patch: skip base MB decode + deblock; injection as sole base source | 2–3 d |
| Base-frame feed via fifo + POC-matched ordering | 1–2 d |
| `build-ldecod.sh` → also build/install `mvcdep`; patch file in-repo | 0.5 d |
| `runner.rs` hybrid path + `mvcdep` resolution + fallback | 1–2 d |
| Corpus bit-exactness validation + retiming | 1–2 d |
| **Total** | **~1–1.5 wk** |

**Payoff (measured, `docs/libmvc-injection.md` § 5b/5c):** ~1.6×
end-to-end with HW encode, ~1.4× with x264. Hard-capped near ~1.9× by the
dependent-view JM decode, which this does not accelerate.

## Decision gate

Do the carve **after** HW encode is the convert default (VAAPI/QSV
auto-detect) — that is the cheaper 1.44× and it makes convert
decode-bound, which is the regime where this carve returns ~1.6× rather
than ~1.4×. If the appetite is for >2×, the carve is instead a stepping
stone toward accelerating the *dependent* view (SIMD, or the Rust
dependent decoder on the now-complete front end) — that is where the
remaining headroom is, and it reuses everything here.
