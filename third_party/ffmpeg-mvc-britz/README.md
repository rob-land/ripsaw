# Britz FFmpeg MVC fork — vendored reference material

This directory captures the Britz fork's MVC delta, extracted on
2026-05-28 from `github.com/Britz/FFmpeg @ mvc` (single commit
`1217f9077`, dated 2013-02-21, atop FFmpeg 0.11.2 commit `69cc119d6`).

The Britz fork is **not used at runtime**. See `docs/mvc3d.md` for the
sample-validation findings that ruled it out as a direct dependency.
The material here is preserved as reference input for a forward-port
rewrite against current FFmpeg.

## Layout

```
third_party/ffmpeg-mvc-britz/
├── README.md              this file
├── new-files/
│   ├── h264_mvc.c         1,284 lines — the MVC decoder algorithm itself
│   └── h264_mvc.h         242 lines  — its public surface (within libavcodec)
└── patches/
    ├── h264.c.patch       hooks into the legacy monolithic h264 decoder
    ├── h264.h.patch
    ├── h264_parser.c.patch
    ├── h264_ps.c.patch    sub-SPS parsing (the MVC parameter set extension)
    ├── h264_refs.c.patch  inter-view reference picture handling
    ├── h264_sei.c.patch
    ├── mpegvideo.h.patch  adds `view_id` field to `Picture`
    ├── vaapi_h264.c.patch hardware-decode wiring
    └── Makefile.patch     build glue (`OBJS += h264_mvc.o`)
```

## Why the patches do not apply to current FFmpeg

A `git apply --check` of every patch fails. The fundamental reason is
that **`libavcodec/h264.c` does not exist anymore**. Modern FFmpeg
(7.x) has split the monolithic 2012-era decoder into:

- `h264dec.c` — codec init, packet decode entry
- `h264_slice.c` — per-slice decode loop
- `h264_picture.c` — DPB / picture lifecycle
- `h264_mb.c`, `h264_mb_template.c` — macroblock decode
- `h2645_parse.c`, `h2645_sei.c` — H.264/H.265 shared parsing
- and more

The other files Britz touched (`h264.h`, `h264_ps.c`, `h264_refs.c`,
`mpegvideo.h`, `vaapi_h264.c`) also exist in current FFmpeg but with
substantially different structure. The patches fail at the first hunk
in each file.

## How this material is used

After a closer reading of ITU-T H.264 Annex G (the MVC specification),
the project's MVC strategy moved from "forward-port Britz" to "write
`libmvc` from spec, reuse upstream libavcodec for base H.264". See
`docs/mvc3d.md` § "Decoding strategy" and § "Bitstream spec reference"
for the current plan.

In that plan, this directory is **reference material**, not a
patch series we apply. The two useful ways to consult it:

- **`new-files/h264_mvc.c`** is one of only two freely-available
  worked examples of the MVC decode process (the other being `ldecod`
  from the JM reference codebase). Where the spec is ambiguous, cross-
  reading Britz's interpretation against `ldecod`'s is the fastest way
  to resolve.
- The patches show **which existing libavcodec call sites needed
  hooks for MVC** back in the 2012 codebase. Although the file names
  no longer match modern FFmpeg, the *conceptual* hook points are
  stable: subset SPS parsing in the parameter-set layer, view-id on
  the picture struct, ref-list construction in the DPB, slice-header
  bits for MVC in the slice layer.

Britz's `h264_mvc.c` itself stops short of finishing the output side
(see `docs/mvc3d.md` § "Britz fork sample validation" for the
`// JB for mvc generate some different output!` TODO). Our `libmvc`
will not replicate that limitation.

## Licensing

The Britz fork is LGPL v2.1+ (same as upstream FFmpeg 0.11). Code
ported from `h264_mvc.{c,h}` into a forward-ported decoder retains
that licence and must be marked accordingly in the new file headers.
Ripsaw's GPL-3-or-later application licence is compatible with linking
against LGPL libav* libraries.
