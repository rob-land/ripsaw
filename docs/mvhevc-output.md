# MV-HEVC output branch — scope

Status: scoping doc (2026-06-17). Companion to `docs/xreal.md` (which
chose MV-HEVC as the canonical archival format) and
`docs/libmvc-optionb-carve.md`. Grounded in a real encode/decode PoC on
this host.

## Goal & topology

Add MV-HEVC as a convert output **alongside** FSBS, branching from the
same two-view intermediate — **not** by re-encoding the FSBS file.

The convert pipeline already decodes to two separate raw-YUV views
(`runner.rs:171–172`: `output_ViewId0000.yuv` / `…0001.yuv`). FSBS is
just `hstack` + encode of those two views. MV-HEVC feeds the *same two
views* straight into a multiview HEVC encoder as base + dependent layers:

```
decode → two raw YUV views → ┬ hstack → encode      = FSBS  (today)
                             └ multiview encode      = MV-HEVC (this scope)
```

Re-encoding FSBS→MV-HEVC would be a lossy double-encode and topologically
backwards (you'd have to split the packed frame back into views). The
expensive, hard part — decoding MVC into two correct views — is shared
and already done (ldecod / mvcdep).

## PoC results (2026-06-17)

Encoded two real decoded views (Friday the 13th Part 3 clip, 96 frames
1920×1080) to MV-HEVC and attempted a decode round-trip.

**Encode — works.** Requires a custom **x265 built with
`-DENABLE_MULTIVIEW=ON`** (the cmake option defaults *OFF*, so Fedora's
`x265-libs` — and therefore ffmpeg's `libx265` — has no multiview;
ffmpeg's single-input `libx265` wrapper can't drive it anyway). The
standalone binary, driven by a config file:

```
# mv.txt — one line per view, filename via --input, NO bare filename
--input o_ViewId0000.yuv --input-res 1920x1080 --fps 24
--input o_ViewId0001.yuv --input-res 1920x1080 --fps 24
```
```
x265 --num-views 2 --multiview-config mv.txt \
     --input-res 1920x1080 --fps 24 --preset medium --crf 22 -o out.265
```

Result: `tools: multi-view`, two per-view rate reports — **base 219 kb/s,
dependent only 33 kb/s** (inter-view prediction makes the second layer
tiny). A NAL scan confirms two layers (`nuh_layer_id` 0: 104 NALs, 1: 98
NALs). Encode ran at ~32 fps (software, medium, 1080p).

Two parser gotchas, both real:
- The per-view input is given as `--input <file>`, *not* a bare filename
  (the sample `multiview.txt` in the x265 tree is misleading).
- The config parser locates options with `strchr(line, '-')`, so **any
  `-` in the path before the options breaks it** (our `mktemp` dir
  `rsmvh-aBQ0` did). Use dash-free paths, or run with `cwd` set and
  relative filenames.

**Decode round-trip — RESOLVED (2026-07-27, different host).** Re-run
on the desktop with **ffmpeg 7.1.5** against a fresh 240-frame
x265-multiview encode (LR_Pattern views decoded by libmvc):

- **Raw `.265`, ffmpeg-muxed MP4, and ffmpeg-muxed MKV all decode both
  views**: `-view_ids 0` → 240 left, `1` → 240 right, `-1` → 480
  interleaved. No `SPS 1 does not exist`.
- **The June failure was mkvmerge**: muxing the same `.265` with
  `mkvmerge` and decoding with `-view_ids -1` yields only 240 frames
  plus SPS errors — it drops the enhancement-layer signaling. Archive
  rule: **mux MV-HEVC with ffmpeg (raw → MP4 → MKV works), never
  mkvmerge.** (The 8.1.1-vs-7.1.5 ffmpeg delta on the other host is
  unexcluded as a second factor, but mkvmerge alone reproduces the
  failure here.)
- **How views surface**: NOT as two streams. `-view_ids -1` produces
  **one** video stream of alternating left/right frames with stereo3d
  "frame alternate" side data. The playback graph is
  `-view_ids -1` (input option) + `stereo3d=al:sbsl` filter → packed
  full-SBS in a single decode (240 composed frames verified). A
  dual-input `[0:v][1:v]hstack` with per-input `-view_ids 0`/`1` also
  works at 2× decode cost.
- **Detection gotcha**: ffprobe reports plain profile "Main" for the
  multiview file — no "Multiview" string, no side data. The reliable
  probe is a 1-frame decode with `-view_ids 1`: mono prints "View ID 1
  not present in VPS", multiview is silent. (ripplay now does this.)

## Implications

- **Encode side is feasible and de-risked** — x265 multiview produces a
  genuine, compact 2-layer stream from the views we already have.
- **The playback story is the unproven part**, which is awkward because
  realtime playback is the *entire reason* MV-HEVC was chosen
  (`docs/xreal.md`). Before committing MV-HEVC as the canonical archival
  format we must verify an end-to-end *decode* on at least one real
  target:
  - Linux/ffmpeg: find the container/muxing that makes `-view_ids` work
    (likely MP4 with proper `hvcC`+`lhvC`, possibly a newer/patched
    ffmpeg, or muxing the layers as the spec wants). Until then, "archive
    MV-HEVC, play on Linux" is unproven.
  - Apple: MV-HEVC is the spatial-video format; VideoToolbox decodes it
    in hardware. This is the most likely-to-just-work target and worth
    testing a generated file against (QuickTime / Vision Pro), but needs
    the right `.mov` packaging (Phase E `vexu` atoms).

## Ripsaw integration (when greenlit)

1. `OutputFormat::MvHevc` (new variant in `convert/format.rs`); it has no
   single-frame "compose" — it skips `hstack` entirely.
2. `runner.rs`: when `format == MvHevc`, after the two `ViewIdNNNN.yuv`
   land, write a multiview config and invoke the x265 binary directly
   (not the ffmpeg encode path), then mux to the chosen container.
3. `scripts/build-x265.sh` — clone + `-DENABLE_MULTIVIEW=ON` build +
   install, mirroring `build-ldecod.sh`; resolve the binary like
   `resolve_ldecod_path` ($RIPSAW_X265 / built path / PATH), with a
   clear "MV-HEVC needs a multiview-enabled x265" error when absent.
4. Encoder backend: x265 multiview is **software only** here (no
   `--multiview` in VAAPI/QSV; NVENC MV-HEVC needs an Ada+ NVIDIA GPU,
   absent on this Intel host). So MV-HEVC won't benefit from the HW-encode
   default — it's a slower, software path, traded for a much smaller file
   and the future playback story.

## Effort & sequencing

| Task | Est. |
|---|---|
| **Verify decode round-trip on a real target** (Linux container fix or Apple) — *gates everything* | 1–3 d, uncertain |
| `build-x265.sh` (multiview) + binary resolution | 0.5 d |
| `OutputFormat::MvHevc` + runner branch (config + x265 invoke + mux) | 1–2 d |
| Container/atom packaging for the chosen playback target | 1–3 d |
| **Total** | **~1 wk, gated on the decode verification** |

## Recommendation

~~Do not build the MV-HEVC output branch until the decode round-trip is
verified.~~ **Verified 2026-07-27** (see PoC results above): encode +
Linux ffmpeg decode of both views works end-to-end when ffmpeg does the
muxing; mkvmerge is the one tool that breaks the layer signaling. The
MV-HEVC output branch is unblocked. Implementation notes for when it's
built: mux with ffmpeg only, and remember downstream detection needs
the 1-frame `-view_ids 1` probe (ffprobe alone can't tell MV-HEVC from
plain Main-profile HEVC).
