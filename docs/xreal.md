# Xreal One Pro — 3D playback target

Survey notes for the "sister project" idea: a Linux player that
drives Xreal One / One Pro AR glasses for 3D-movie playback off
Ripsaw's rips. Survey landed 2026-05-29; revisit if Xreal ships
a Linux SDK or the open-source pipeline meaningfully changes.

## How the glasses enter 3D mode

The Xreal One series are **plain USB-C DisplayPort-alt monitors
from the host's point of view**. There is no host-side signal that
triggers 3D mode -- no HDMI 1.4 frame-packing handshake, no EDID
mode flag, no SDK API. The mode is entered entirely on the device
by the user: double-click the red X button to open the OSD, then
*Spatial Screen → 3D Mode → Half SBS* or *Full SBS*. The mode can
also be assigned to the Quick Button.

The public XREAL SDK 3.1 (docs.xreal.com) is a Unity-only AR app
SDK for Nebula/Android hosts. No Linux SDK. No documented control
API for the 3D mode. **Uncertain** whether a private USB HID
protocol exists; nothing I found suggests one has been
reverse-engineered for the One series (XRLinuxDriver only handles
IMU input).

Source: <https://tutorials.xreal.com/docs/glasses/one-series/osd/3d-mode/>

## Stereo input formats the glasses accept

Only **Full-SBS** (3840×1080, no horizontal squeeze) and
**Half-SBS** (1920×1080, horizontally squeezed) are listed on the
One/One Pro OSD. The format help text mentions Top-and-Bottom but
it's not a selectable mode on the One series -- likely Beam Pro
only. **No HDMI 1.4 frame-packing, no frame-sequential, no
MV-HEVC/MVC pixel-level input.** The glasses split a normal
framebuffer; they do not decode stereo codecs themselves.

## Linux display path

The glasses present as a regular monitor with native EDID modes
1920×1080 and 3840×1080 ultrawide. KDE Plasma 6, GNOME 47, X11 all
pick them up via standard DRM/KMS. One stray report of an "EDID
block 0 checksum invalid" warning on Purism L5 firmware; survivable
but possibly relevant on stricter setups.

Head-tracking / virtual-monitor compositing is handled by
[wheaney/XRLinuxDriver](https://github.com/wheaney/XRLinuxDriver) +
[wheaney/breezy-desktop](https://github.com/wheaney/breezy-desktop).
**For movie playback you don't need either** -- they're for
desktop overlay, not 3D video.

## The Jellyfin-Android-vs-VLC convergence quirk (root cause)

This is a known, open Jellyfin issue, not a Xreal issue:
[jellyfin/jellyfin-android #1861](https://github.com/jellyfin/jellyfin-android/issues/1861).

Jellyfin Android draws its ExoPlayer SurfaceView into the in-app
Activity on the phone's panel; Android then **mirrors** that
activity to the external 3840×1080 with letterbox/scale rather
than rendering natively at the external mode. The horizontal
scaling collapses each FSBS half onto the panel's 16:9, and the
mirror back to 3840×1080 stretches it -- so left/right pixels
no longer align with the glasses' 3840-wide split, and the eyes
never converge.

VLC for Android uses Android's `Presentation` API (or sets the
SurfaceView at the external display's native size), so the FSBS
frame reaches the glasses at 3840×1080 pixel-for-pixel and the
split works.

**For Linux this is moot.** A Linux laptop driving the glasses
over USB-C DP-alt connects directly; there's no phone panel in
the loop; mpv / VLC fullscreen on the Xreal output renders at
3840×1080 natively. The Jellyfin-Android bug never applies.

## OSS 3D playback on Linux

| Player / feature | State |
|---|---|
| mpv `--video-stereo-mode` / FFmpeg `stereo3d` | SBS ↔ TaB ↔ anaglyph etc. **Doesn't drive HDMI 1.4 frame-packing modes.** Open since 2015 ([mpv#1945](https://github.com/mpv-player/mpv/issues/1945)) |
| VLC stereoscopic output (GSoC 2011) | Renders FSBS to framebuffer; no native HDMI frame-packing emitter |
| DRM/KMS `DRM_MODE_FLAG_3D_FRAME_PACKING` | Exists in the kernel API but no Wayland compositor or mainstream player wires it through end-to-end |
| Wayland frame-packing protocol | Uncertain; I searched and found nothing |
| libavcodec MVC decode | Nothing (second view dropped) |
| libavcodec MV-HEVC decode | Landed in FFmpeg 7.1, decodes both layers |
| MV-HEVC stereoscopic *rendering* | Nothing today; you decode and stitch yourself |

## Best feed format for these glasses

Because the One series only splits a framebuffer, the host always
has to emit **FSBS at 3840×1080**. Frame-packing buys nothing
(glasses don't accept it). MV-HEVC compressed-stereo buys nothing
at playback time (no Linux player composes it stereoscopically yet).

**Practical sweet spot: FSBS in H.265 Main 10 at ~16-22 Mbps.**
- Double the 2D pixel count, but HEVC's inter-redundancy across
  the SBS halves recovers most of the bitrate cost.
- Every Linux GPU decodes it.
- Direct match to the glasses' native input.

MV-HEVC is **~30%** smaller for the same quality but requires a
decode-then-stitch step at playback. Worth doing as an
*ingest* path (so future Apple Vision Pro spatial-video sources
can be remuxed to FSBS once), not as a *delivery* path.

## Implications for Ripsaw + sister player

1. **Ripsaw should keep FSBS as the canonical 3D output.** It's
   the format the glasses actually want. HSBS stays as a
   bandwidth-constrained option but is the wrong default.
2. **Default codec for the 3D output should be x265 / HEVC** (not
   x264) once the convert pipeline learns to switch encoders.
   For an Xreal target the FSBS 3840×1080 canvas benefits from
   HEVC's inter-half prediction. Add this as a `convert` option
   alongside the existing FullSbs/HalfSbs format selector.
3. **The "sister player" is small.** Launch `mpv --fs
   --fs-screen=<xreal>` (or VLC equivalent) directly on the
   DP-alt output. No special protocol glue. The whole player
   sub-project might be a 200-line GTK4 launcher that lists
   the user's library and starts mpv with the right
   `--fs-screen` flag, plus an on-screen reminder of the
   double-X-button → 3D mode dance.
4. **MV-HEVC ingest is the optional fourth phase.** FFmpeg 7.1+
   decodes both layers; we'd extract them at convert time and
   compose to FSBS like the current MVC path, just skipping
   ldecod entirely.

## Strategic shift: MV-HEVC as the canonical archival format (2026-05-29 update)

After the initial "FSBS is the right output" finding above, a
follow-up discussion reframed the storage question.

The Xreal-only world says: "the glasses only take a split
framebuffer, archive FSBS once, done." That's true for the
delivery format. But for the **archive** format, FSBS is wasteful:
it doubles the canvas, locks you to the SBS-split delivery layout,
and forces a re-encode if you ever want the same content on a
Vision Pro or any other 3D target.

A cleaner model: **archive MV-HEVC, compose FSBS at playback time
for the Xreal**. The sister player owns the realtime stitching;
the library only holds one copy per movie; other 3D targets
(Vision Pro, future hardware) can consume the same archive with
trivial container repackaging.

### Why MV-HEVC archival beats FSBS archival

| Property                       | FSBS (current today)                    | MV-HEVC (target)                                  |
| ---                            | ---                                     | ---                                               |
| Storage cost                   | 2x pixel canvas; HEVC across SBS halves | ~30% smaller; native inter-view prediction        |
| Apple Vision Pro target        | Re-encode required                      | Apple-atom inject; no recompress                  |
| Plex/Jellyfin compatibility    | Plays as 2D ultrawide on non-3D players | Plays as 2D 1080p (base view) on non-3D players   |
| Xreal target via sister player | Already FSBS-ready, trivial passthrough | Decode + hstack at playback time                  |
| Future-proofing                | SBS is a 2010s-era hack                 | MV-HEVC is the current spec; HEVC v2 Annex G      |

### Decode-speed reality check

What gates the "compose at playback" idea is the realtime decode
budget. Numbers from the LR_Pattern test:

- **H.264 MVC live decode via JM ldecod**: ~21 fps on 1920x1080
  both-views. That's ~6x too slow for 24p playback. Not realtime.
  Live MVC playback is gated on `libmvc` (the long-term decoder
  scoped in `docs/mvc3d.md`).
- **HEVC MV-HEVC live decode via FFmpeg 7.1+**: realtime on any
  modern CPU. Anton Khirnov's `view_ids` patchset is software-
  decoding both layers; HEVC at 1080p doubled is well within
  software-decode budget on any laptop chip from the last 5
  years. **Not measured yet on this hardware** -- worth a 30-line
  proof-of-concept before committing to the architecture.
- **Encode side**: x265 has `--multiview-config` for MV-HEVC
  output, 8-bit only as of this writing. NVIDIA Video Codec SDK
  13.0 also encodes MV-HEVC if the user has an Ada-or-newer GPU.

The implication: archive MV-HEVC, decode in realtime in the
sister player, no `libmvc` needed for the Xreal flow.

### Phased rollout

**Phase A (landed 2026-05-29).** FSBS-from-MVC pipeline works
end-to-end. `MvcWithBlockAdditions` and `MvcInlineLaced` both
route through `extract_to_annex_b` (or `mkvextract`) → JM ldecod
→ ffmpeg hstack → libx264. Output is a Xreal-ready FSBS MKV
today. This stays as the default until Phase C ships.

**Phase B.** Add HEVC encode option to the FSBS output. Same
compose chain, swap libx264 for libx265. Add to title list
page's Output Options group as `Encoder: H.264 / H.265`.
~30% smaller files for the same quality on the FSBS canvas;
no change to the rest of the pipeline.

**Phase C.** Add MV-HEVC as a fourth Output Format. The two-view
YUV pipeline already produces ldecod's `output_ViewId0000.yuv`
and `output_ViewId0001.yuv` per-frame. Replace the final ffmpeg
hstack step with x265 `--multiview-config <yaml>` plus the
appropriate input wiring (probably needs the YAML to list both
view files as inputs). Output is an MKV/MOV with one base +
one enhancement layer; standard players still see a 2D 1080p
base view. Validation: re-decode the resulting file with
FFmpeg 7.1+ `view_ids=0,1` and confirm both views come back.

**Phase D.** Build the sister player (working name: `ripplay`,
parallel to `ripsaw`). Tiny GTK4 + Adwaita app:

- Library browser (reuses Ripsaw's `library_root` setting -- so
  the sister project depends on `ripsaw` the library, not the
  binary).
- Per-file 3D format detection: MV-HEVC vs FSBS-already-packed
  vs MVC vs 2D.
- "Watch on Xreal" button picks the right playback mode.
- For MV-HEVC: invoke mpv with a `--lavfi-complex` graph that
  decodes both views and hstacks to 3840x1080, or shell ffmpeg
  → fifo → mpv if `--lavfi-complex` doesn't expose `view_ids`.
- For FSBS files: trivial passthrough, mpv fullscreen on the
  Xreal output.
- For MVC files: error message until libmvc exists ("re-archive
  as MV-HEVC for live playback").
- On-screen reminder of the Xreal OSD path (double-X →
  Spatial Screen → 3D Mode → Full SBS) on first launch.

Probable size: 500-1500 lines depending on whether the player
shells to mpv or uses `libmpv` via the [mpv-rs](https://crates.io/crates/libmpv) bindings.

**Phase E.** Apple Vision Pro atom injection. For each MV-HEVC
archive, generate a `.mov` variant with the `vexu`/`hfov`/
`lhvC`/`tapt` atoms set for direct Vision Pro consumption. No
recompress. Bento4 `mp4edit` is the off-the-shelf option; a
small Rust impl is also realistic (the atoms are well-documented
in ISOBMFF terms).

**Phase F (long-term).** `libmvc`. With MV-HEVC archival in
place, libmvc is no longer correctness-required for any
downstream target -- but it enables three things that are
otherwise blocked:

1. Live MVC playback in the sister player (no archive
   transcode needed; just point at the rip).
2. Faster MV-HEVC archival (libmvc + x265 vs ldecod + x265 --
   ldecod is the throughput bottleneck today, running at maybe
   5-10x slower than the encoder).
3. Stream-direct-from-disc-no-rip-file workflow: Ripsaw could
   skip the makemkvcon rip phase entirely for transient
   "watch this disc" playback.

### Sister-player technical hooks to verify

Before committing to Phase D, prototype these:

1. **Does `mpv --lavfi-complex` actually expose FFmpeg's HEVC
   `view_ids` option?** If yes, the player is a one-liner mpv
   invocation. If no, we need to pipe ffmpeg's decode output
   through a fifo to mpv. Use one of Apple's published spatial
   video samples (the iPhone 15 Pro spatial recordings or the
   Vision Pro sample reels) as the test corpus.

2. **Realtime MV-HEVC decode on the user's hardware**: confirm
   24p playback runs without dropped frames at 1080p doubled.

3. **Xreal OSD interaction with `mpv --fs`**: confirm that
   fullscreen on the Xreal output puts a 3840x1080 framebuffer
   on the glasses pixel-for-pixel, and that the user's
   double-X-button → 3D Mode flow then splits correctly.

### Open questions for future sessions

- Whether mpv's `--lavfi-complex` supports the `view_ids`
  selector. Strong "uncertain" -- not in the docs I read.
- Whether x265 MV-HEVC base+enhancement encode is robust enough
  in practice (vs the reference HM/HTM encoder) for our typical
  3D BD source bitrate budget (~16-22 Mbps total).
- Whether the Apple Vision Pro atom set is stable / documented
  enough to write our own injector vs vendoring Bento4. brilly.tv's
  guide says it works; SpatialMediaKit's source on macOS is a
  reference.
- 8-bit MV-HEVC encode in x265 is the current state of the art --
  3D BD source is 8-bit so this is fine for our flow, but it
  forecloses HDR delivery to Vision Pro if/when that becomes
  interesting.
- Whether the sister player should also handle the Vision-Pro-
  atom injection or whether that's a separate Ripsaw export step.
  Suggest: keep it in Ripsaw as another Output Options choice
  ("Output: Apple Vision Pro spatial video"), since it's a
  one-time transformation per archive.

## Sources

- [Xreal One Series tutorial — 3D Mode](https://tutorials.xreal.com/docs/glasses/one-series/osd/3d-mode/)
- [Xreal One Series tutorial — UltraWide Mode](https://tutorials.xreal.com/docs/glasses/one-series/osd/ultrawide-mode/)
- [docs.xreal.com — XREAL SDK 3.1 (Unity, no Linux)](https://docs.xreal.com/)
- [wheaney/XRLinuxDriver](https://github.com/wheaney/XRLinuxDriver)
- [wheaney/breezy-desktop](https://github.com/wheaney/breezy-desktop)
- [jellyfin/jellyfin-android #1861 — fullscreen doesn't expand to external display](https://github.com/jellyfin/jellyfin-android/issues/1861)
- [features.jellyfin.org — Support External Displays on Android Client](https://features.jellyfin.org/posts/2060/support-external-displays-on-android-client)
- [wiki.videolan.org — Stereoscopic Video GSoC 2011](https://wiki.videolan.org/SoC_2011/Stereoscopic_Video/)
- [mpv-player/mpv #1945 — HDMI 1.4 frame packing](https://github.com/mpv-player/mpv/issues/1945)
- [kernel.org — DRM KMS docs (frame-packing flag)](https://docs.kernel.org/gpu/drm-kms.html)
- [9to5linux — FFmpeg 7.1 MV-HEVC](https://9to5linux.com/ffmpeg-7-1-peter-released-with-full-native-vvc-decoder-mv-hevc-decoder-and-more)
