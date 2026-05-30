# HEVC-era 3D formats: MV-HEVC and 3D-HEVC

Scoping notes for whether Ripsaw should grow support for the
HEVC-generation 3D extensions alongside the existing H.264 MVC
pipeline. Survey landed 2026-05-29; revisit if FFmpeg / x265 status
changes.

## Bitstream summary

Both are HEVC v2+ extensions; both encode two (or more) views; the
relationship between them is parallel to H.264's `MVC` (multiview)
versus `MVC + depth` extensions:

- **MV-HEVC** (Annex G / F). High-level-syntax-only extension. Each
  view is a separate layer identified by `nuh_layer_id`; the base
  layer is a plain HEVC bitstream decodable by any HEVC decoder.
  The only new prediction is inter-view sample/motion prediction
  between reconstructed layers. No new block-level tools.
- **3D-HEVC** (Annex I). Sits on top of MV-HEVC and adds
  *block-level* coding tools for joint texture+depth (Multiview
  Video plus Depth, MVD): disparity-compensated prediction,
  depth-map-specific intra modes, motion-parameter inheritance from
  texture to depth, view synthesis prediction. Requires net-new
  decoder logic, not just layer multiplexing.

(Source: MERL TR2015-125 "Overview of Multiview and 3D Extensions
of HEVC"; Fraunhofer HHI 3D-HEVC project page.)

## Ecosystem usage

| Format | Where it's used in the wild |
|---|---|
| MV-HEVC | The live format. Apple Vision Pro spatial video; iPhone 15/16 Pro spatial recordings; Apple Immersive Video; Disney's digital 3D distribution (e.g. *Avatar*); NVIDIA Video Codec SDK 13.0 stereo encode (gaming / Steam capture). |
| 3D-HEVC | Essentially zero deployment. No commercial disc, no streaming service ships it. UHD Blu-ray spec excludes 3D entirely; existing 3D Blu-rays are still H.264 MVC at 1080p. 3D-HEVC remains a standards artefact. |

So when people talk about "HEVC 3D" in 2026 they almost always mean
**MV-HEVC via Vision Pro**, not 3D-HEVC.

## Open-source decode status

| Implementation | MV-HEVC | 3D-HEVC |
|---|---|---|
| HM / HTM reference (Fraunhofer HHI) | Works (HTM is the canonical encoder/decoder). | Works in HTM, frozen at HTM-16.x circa 2015; not maintained. |
| libavcodec (FFmpeg) | **Works today.** Anton Khirnov's patchset merged Sept 13 2024 (patchwork series 20240914). `view_ids` option, max two layers, in FFmpeg 7.1+. | Nothing. |
| libde265 | Nothing in changelog; HEVC v2 Main/RExt only. | Nothing. |
| libheif | Reads HEIF stills, video out of scope. Uncertain whether stereo HEIC images via `lhvC` are parsed. | Nothing. |

## Open-source encode status

- **x265**: MV-HEVC merged. `--multiview-config` switch, two layers,
  8-bit only as of writing (10-bit MV-HEVC encode remains a known
  gap). No 3D-HEVC support.
- **kvazaar**: nothing for either extension (uncertain but appears
  absent from repo/docs).
- **HM (reference)**: HM encodes plain HEVC; HTM fork encodes
  MV-HEVC and 3D-HEVC but at reference-software performance, i.e.
  unusably slow for real-length content. Same situation as JM
  ldecod is for MVC today.

## What Ripsaw would need to add

### MV-HEVC playback / repack to SBS

**Trivial to moderate.** FFmpeg ≥ 7.1 already demuxes Apple MOV,
parses `lhvC`, and decodes both views via the `hevc` decoder's
`view_ids=0,1` option. Ripsaw's pipeline can drop the JM ldecod
stage entirely for this format and pipe the two `AVFrame`s into the
existing SBS / HSBS / TAB / FrameSeq compose stage.

The only extra work is Apple atoms (`vexu`, `hfov`, `tapt`) -- not
needed to *decode*, but needed if Ripsaw ever wants to *produce*
Vision-Pro-playable output. For that path, vendor Bento4
`mp4edit` (already used by the brilly.tv guide below) or write
the atoms directly.

### MV-HEVC encode (for "rip 3D BD, transcode to Vision Pro")

**Moderate.** x265 with `--multiview-config` handles the MV-HEVC
bitstream production; you then need MP4Box for CTTS fixup, FFmpeg
to switch the sample entry to `hvc1`, and Bento4 `mp4edit` to
inject Apple's `vexu` / `hfov` / `lhvC` / `tapt` atoms extracted
from a reference Apple file. Whole chain is OSS. Currently
**8-bit only** because FFmpeg lacks 10-bit MV-HEVC encode -- not a
blocker for re-targeting H.264 MVC content (the source is 8-bit
1080p) but means parity with native Apple Immersive 10-bit HDR is
out for now.

### 3D-HEVC

**Hard, and probably not worth doing.** No FFmpeg / libavcodec /
libde265 support. Realistic options:

1. Vendor HTM as an external decoder à la today's JM ldecod path.
   Same architecture, same headaches, even less upstream interest.
2. Skip the format entirely.

Given there is no commercial content using 3D-HEVC anywhere, (2)
is the rational choice unless a research dataset comes into scope.

## Apple Vision Pro spatial-video specifics

- **Container.** ISOBMFF `.mov` (also `.mp4`), single track.
  - `hvc1` sample entry for base view.
  - `lhv1` / `lhvC` extension for the second layer.
  - Apple-specific atoms: `vexu` (view extended usage: baseline,
    field-of-view, projection), `hfov`, `tapt`.
- **Bitrates / resolutions.** iPhone 15/16 Pro: 1080p60, ~130 MB
  per minute. Apple Immersive Video: 4320×4320 per eye, 10-bit
  HDR, 90 fps, ~50 Mbps.
- **Existing Linux production chain** (brilly.tv guide): x265 →
  MP4Box → FFmpeg → Bento4 `mp4edit`. End-to-end OSS.
  SpatialMediaKit and Mike Swanson's `spatial` tool are macOS-only
  and not portable.

## Patent / licence status

- **MV-HEVC**: covered by the existing HEVC pools (MPEG LA, Access
  Advance, Velos Media). MPEG LA's terms: first 100k units/year
  royalty-free, then $0.20/unit capped at $25M/yr; the other two
  pools have separate programmes. No additional MV-HEVC-specific
  royalty above and beyond plain HEVC.
- **3D-HEVC**: uncertain. Falls under HEVC v2 nominally but no
  pool advertises a specific 3D-HEVC programme because nothing
  ships it.

## Bottom line for Ripsaw

- **MV-HEVC**: realistic feature add when there's appetite. The
  hard part of MVC decode (own walker + JM ldecod + custom
  profile XML to keep the MVC track) goes away because libavcodec
  already does the equivalent. Most of the work is plumbing
  through Ripsaw's existing UI and adding Apple-atom write
  support for Vision-Pro-targeted output.
- **3D-HEVC**: skip. No content, no decoder, no demand.
- **Side benefit**: if Ripsaw grows MV-HEVC encode (x265
  `--multiview-config` + Apple atoms), the same machinery
  enables "rip a 3D Blu-ray (H.264 MVC) and re-encode to Apple
  Vision Pro spatial video" as a single workflow. That's
  probably the single most user-valuable HEVC-3D feature this
  project could add, given Vision Pro is the largest active 3D
  consumer device.

## Sources

- [hevc.info – 3D-HEVC](http://www.hevc.info/3dhevc) *(host was
  down at survey time)*
- [hevc.info – MV-HEVC](http://hevc.info/mvhevc) *(host was down
  at survey time)*
- [Fraunhofer HHI – 3D-HEVC Extension](https://www.hhi.fraunhofer.de/en/departments/vca/research-groups/video-coding-technologies/research-topics/past-research-topics/3d-hevc-extension.html)
- [listenlink/3D-HEVC (HTM mirror)](https://github.com/listenlink/3D-HEVC)
- [listenlink/HM (HM mirror)](https://github.com/listenlink/HM)
- [FFmpeg patchwork – lavc/hevcdec: implement decoding MV-HEVC](https://patchwork.ffmpeg.org/project/ffmpeg/patch/20240914111036.17164-16-anton@khirnov.net/)
- [FFmpeg Codecs Docs – hevc view_ids](https://ffmpeg.org/ffmpeg-codecs.html)
- [MulticoreWare – MV-HEVC in x265](https://multicorewareinc.com/mv-hevc-extension-support-in-x265/)
- [Mike Swanson – MV-HEVC with x265 and NVIDIA](https://blog.mikeswanson.com/mv-hevc-with-x265-and-nvidia/)
- [Mike Swanson – Encoding Spatial Video](https://blog.mikeswanson.com/encoding-spatial-video/)
- [brilly.tv – Creating Spatial Video on Linux](https://brilly.tv/spatial-video-guide.html)
- [SpatialMediaKit (macOS)](https://github.com/sturmen/SpatialMediaKit)
- [NVIDIA – MV-HEVC in Video Codec SDK 13.0](https://developer.nvidia.com/blog/enabling-stereoscopic-and-3d-views-using-mv-hevc-in-nvidia-video-codec-sdk-13-0/)
- [MERL TR2015-125 – Overview of Multiview and 3D Extensions of HEVC](https://www.merl.com/publications/docs/TR2015-125.pdf)
- [strukturag/libde265](https://github.com/strukturag/libde265)
- [strukturag/libheif](https://github.com/strukturag/libheif)
- [ultravideo/kvazaar](https://github.com/ultravideo/kvazaar)
- [FlatpanelsHD – Why Apple's 3D format outshines 3D Blu-ray](https://www.flatpanelshd.com/focus.php?subaction=showfull&id=1761294912)
- [AVS Forum – Toward a 4K MV-HEVC Blu-ray of Avatar 2](https://www.avsforum.com/threads/maybe-at-last-we-are-approaching-to-a-3d-4k-mv-hevc-blu-ray-of-avatar-2.3311223/)
- [Wikipedia – Ultra HD Blu-ray](https://en.wikipedia.org/wiki/Ultra_HD_Blu-ray)
- [MPEG LA – HEVC licence briefing](https://www.mpegla.com/wp-content/uploads/HEVCweb.pdf)
- [Via LA – HEVC/VVC licensing](https://www.via-la.com/licensing-programs/hevc-vvc/)
