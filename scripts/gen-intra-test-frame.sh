#!/usr/bin/env bash
# Generate a small, self-contained H.264 intra frame for validating the
# libmvc decoder against JM ldecod end-to-end (docs/libmvc-poc.md).
#
# The base-view Blu-ray slice we first validated against is all I_8x8 luma
# with deblocking disabled — it can't exercise the rest of the intra decoder.
# This produces a 128x96 High-profile CABAC IDR (8x8 transform on, deblock
# on, QP 28) that uses I_4x4 / I_8x8 / I_16x16 macroblocks, chroma residual,
# and an active in-loop filter — a complete intra-decode target.
#
# Outputs (under $OUT, default ~/mvc-test):
#   test.h264              the stream
#   test_postdeblock.yuv   JM's final (deblocked) YUV  — ldecod -o
#   test_predeblock.bin    JM's pre-deblock reconstruction — DUMP_RECON hook
#   trace_dec.txt          per-element CABAC trace
#
# Needs: ffmpeg (libx264), and ldecod-trace built with the DUMP_RECON hook
# (scripts/build-ldecod-trace.sh + the image.c exit_picture dump).
set -euo pipefail
OUT="${OUT:-$HOME/mvc-test}"
LDECOD="${LDECOD:-$HOME/.local/bin/ldecod-trace}"
mkdir -p "$OUT"
cd "$OUT"

ffmpeg -y -f lavfi -i "testsrc=size=128x96:rate=1:duration=1" -frames:v 1 \
  -c:v libx264 -profile:v high -qp 28 -x264-params "cabac=1:keyint=1" \
  -pix_fmt yuv420p -bsf:v h264_mp4toannexb -f h264 test.h264

DUMP_RECON="$OUT/test_predeblock.bin" "$LDECOD" -i test.h264 -o test_postdeblock.yuv >/dev/null

echo "wrote: test.h264 test_postdeblock.yuv test_predeblock.bin trace_dec.txt"
python3 - <<'PY'
pre=open('test_predeblock.bin','rb').read(); post=open('test_postdeblock.yuv','rb').read()
d=sum(1 for a,b in zip(pre,post) if a!=b)
print(f"deblock changes {d} of {len(pre)} samples")
PY
