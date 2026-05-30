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
