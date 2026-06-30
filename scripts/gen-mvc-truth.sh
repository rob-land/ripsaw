#!/usr/bin/env bash
# Generate a REAL MVC (3D Blu-ray) decode target + per-view ground truth for
# the libmvc dependent-view arc (docs/libmvc-inter.md). Unlike the synthetic
# single-view ffmpeg streams (gen-intra-test-frame.sh), this needs the actual
# 3D disc — ffmpeg has no MVC encoder, so a real NAL-20 dependent view can only
# come from disc.
#
# Test disc: Friday the 13th Part 3 (3D BD). The short MVC clip is title 4
# (00012.mpls, 4 s) under --minlength=0. Stereo High, 1920x1080, 2 views.
#
# Pipeline:
#   1. MakeMKV (keep-MVC profile) rips the title, RETAINING the MVC
#      dependent-view track (+sel:mvcvideo).
#   2. mkvextract pulls the raw H.264 elementary stream (base NAL 1/5 +
#      subset-SPS NAL 15 + dependent-view NAL 20, interleaved per access unit).
#   3. JM ldecod with DecodeAllLayers=1 decodes BOTH views and writes per-view
#      YUV: <out>_ViewId0000.yuv (base) and _ViewId0001.yuv (dependent).
#
# DecodeAllLayers=1 is the flag that makes ldecod decode the dependent view;
# without it ldecod prints "Found SVC extension NALU (20). Ignoring." The
# build (scripts/build-ldecod.sh) carries the MVC/DPB patches.
#
# Outputs under $OUT (default ~/mvc-rip):
#   full.h264             the whole title's elementary stream
#   au0.h264              just the first access unit (base IDR + dep anchor)
#   <name>_ViewId0000.yuv base view, all frames (1920x1080, decode order)
#   <name>_ViewId0001.yuv dependent view, all frames
set -euo pipefail
OUT="${OUT:-$HOME/mvc-rip}"
LDECOD="${LDECOD:-$HOME/.local/bin/ldecod}"
PROFILE="$(cd "$(dirname "$0")/.." && pwd)/data/makemkv/keep-mvc.mmcp.xml"
TITLE="${TITLE:-4}"
mkdir -p "$OUT"
cd "$OUT"

# 1. rip (keep MVC). --minlength=0 so the 4 s clip isn't filtered.
rm -f ./*.mkv
makemkvcon -r --minlength=0 --profile="$PROFILE" mkv disc:0 "$TITLE" "$OUT" >/dev/null
MKV="$(ls -t "$OUT"/*.mkv | head -n1)"
echo "ripped: $MKV"

# 2. extract the H.264 elementary stream (track 0 carries both views).
mkvextract "$MKV" tracks 0:full.h264 >/dev/null
echo "extracted: full.h264 ($(stat -c%s full.h264) bytes)"

# 3a. isolate the first access unit (base IDR slices + dependent anchor slices),
#     bounded by the 2nd access-unit delimiter (NAL type 9).
python3 - <<'PY'
d=open('full.h264','rb').read(); i=0; n=len(d); auds=[]
while i<n-4:
    if d[i]==0 and d[i+1]==0 and (d[i+2]==1 or (d[i+2]==0 and i+3<n and d[i+3]==1)):
        o=3 if d[i+2]==1 else 4
        if (d[i+o]&0x1f)==9: auds.append(i)
        i+=o
    else: i+=1
# AU0 = [0, 2nd AUD); include one more AU so ldecod finalises AU0 cleanly.
end = auds[2] if len(auds)>2 else n
open('au0.h264','wb').write(d[:end])
print(f"au0.h264: {end} bytes ({len(auds)} AUDs)")
PY

# 3b. JM decode, both views.
cat > dec.cfg <<EOF
InputFile = "$OUT/full.h264"
OutputFile = "$OUT/mvc.yuv"
WriteUV = 1
FileFormat = 0
RefOffset = 0
POCScale = 2
DisplayDecParams = 0
ConcealMode = 0
RefPOCGap = 2
POCGap = 2
Silent = 1
IntraProfileDeblocking = 1
DecFrmNum = 0
DecodeAllLayers = 1
EOF
"$LDECOD" -d dec.cfg >/dev/null 2>&1 || true   # truncated ES exits nonzero after writing
echo "wrote per-view ground truth:"
ls -la "$OUT"/mvc_ViewId0000.yuv "$OUT"/mvc_ViewId0001.yuv 2>/dev/null || true
