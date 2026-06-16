#!/usr/bin/env bash
# Build the JM (JVT) H.264/MVC reference decoder `ldecod`, which the 3D
# convert pipeline shells out to. Idempotent: clone if missing, apply the
# patches Ripsaw needs, build, and symlink the binary onto PATH.
#
# Why the patches:
#   1. Modern GCC promotes JM's benign warnings to errors under JM's own
#      -Werror, so the stock tree won't compile. We flip -Werror to
#      -Wno-error in JM's CMake warning module.
#   2. ldecod aborts on MVC streams with "max_dec_frame_buffering larger
#      than MaxDpbSize" -- a too-strict DPB guard. For MVC the function
#      already returns the stream's own (larger) DPB value, so the guard
#      is pure refusal; we drop it so the dependent view decodes.
#   3. ldecod-mvcdep.patch -- the Option B "mvcdep" mode. When
#      RIPSAW_BASE_INJECT=<yuv> is set, the base view's macroblock decode,
#      deblocking and error concealment are skipped and its samples come
#      from the file instead (decoded by libavcodec). The dependent view
#      then decodes against that injected base, bit-exact, in ~0.65x the
#      time (no base reconstruct). See docs/libmvc-optionb-carve.md.
#
# Usage: scripts/build-ldecod.sh
# Env:   JM_ROOT (default ~/3rdparty/JM), PREFIX (default ~/.local)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
JM_ROOT="${JM_ROOT:-$HOME/3rdparty/JM}"
PREFIX="${PREFIX:-$HOME/.local}"
JM_REPO="https://vcgit.hhi.fraunhofer.de/jvet/JM.git"

if [ ! -d "$JM_ROOT" ]; then
  echo "==> cloning JM into $JM_ROOT"
  mkdir -p "$(dirname "$JM_ROOT")"
  git clone --depth 1 "$JM_REPO" "$JM_ROOT"
fi

cd "$JM_ROOT"

echo "==> patching JM (idempotent)"
# 1. -Werror -> -Wno-error
sed -i 's|list( APPEND _bb_warning_options "-Werror" )|list( APPEND _bb_warning_options "-Wno-error" )|g' \
  cmake/CMakeBuild/cmake/modules/BBuildEnv.cmake
# 2. Drop the MVC DPB abort in the decoder.
sed -i 's|error ("max_dec_frame_buffering larger than MaxDpbSize", 500);|/* ripsaw: relaxed for MVC -- use the stream'\''s larger DPB instead of aborting */;|g' \
  source/app/ldecod/mbuffer.c
# 3. mvcdep base-injection mode (git patch; apply only if not already in).
MVCDEP_PATCH="$SCRIPT_DIR/ldecod-mvcdep.patch"
if [ -f "$MVCDEP_PATCH" ]; then
  if git apply --reverse --check "$MVCDEP_PATCH" 2>/dev/null; then
    echo "    mvcdep patch already applied"
  else
    git apply "$MVCDEP_PATCH" && echo "    applied mvcdep patch"
  fi
fi

echo "==> building ldecod"
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release >/dev/null
cmake --build build --target ldecod -j"$(nproc)"

LDECOD="$(ls -t "$JM_ROOT"/bin/umake/*/x86_64/release/ldecod 2>/dev/null | head -n1)"
if [ -z "$LDECOD" ] || [ ! -x "$LDECOD" ]; then
  echo "build reported success but no ldecod binary was found" >&2
  exit 1
fi

mkdir -p "$PREFIX/bin"
ln -sf "$LDECOD" "$PREFIX/bin/ldecod"
echo "==> installed: $PREFIX/bin/ldecod -> $LDECOD"
"$PREFIX/bin/ldecod" 2>&1 | head -n1 || true
