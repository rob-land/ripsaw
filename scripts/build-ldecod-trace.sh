#!/usr/bin/env bash
# Build a TRACE-enabled JM ldecod and install it as `ldecod-trace`, for the
# libmvc per-syntax-element validation (docs/libmvc-poc.md § Validation).
#
# JM's TRACE=1 makes the decoder write trace_dec.txt — one line per decoded
# syntax element — which the libmvc macroblock decoder is diffed against
# (src/mvc/trace.rs). This JM revision never opens the decoder trace file
# (build-ldecod.sh's mvcdep patch adds the fopen, guarded by #if TRACE, so
# it's inert in the normal build); here we compile with -DTRACE=1.
#
# JM hardcodes its binary output path (bin/umake/<gcc>/release/ldecod)
# regardless of build dir, so a trace build would clobber the production
# (mvcdep) ldecod. This script builds the trace variant, copies it aside as
# `ldecod-trace`, then rebuilds the production TRACE=0 binary to restore it.
#
# Run scripts/build-ldecod.sh first (clone + patches + production build).
# Usage: scripts/build-ldecod-trace.sh
# Env:   JM_ROOT (default ~/3rdparty/JM), PREFIX (default ~/.local)
set -euo pipefail

JM_ROOT="${JM_ROOT:-$HOME/3rdparty/JM}"
PREFIX="${PREFIX:-$HOME/.local}"

if [ ! -d "$JM_ROOT/.git" ]; then
  echo "JM not found at $JM_ROOT — run scripts/build-ldecod.sh first." >&2
  exit 1
fi
cd "$JM_ROOT"

echo "==> building TRACE=1 ldecod"
cmake -S . -B build-trace -DCMAKE_BUILD_TYPE=Release -DCMAKE_C_FLAGS="-DTRACE=1" >/dev/null
cmake --build build-trace --target ldecod -j"$(nproc)"

BIN="$(ls -t "$JM_ROOT"/bin/umake/*/x86_64/release/ldecod 2>/dev/null | head -n1)"
mkdir -p "$PREFIX/bin"
cp "$BIN" "$PREFIX/bin/ldecod-trace"
echo "==> installed $PREFIX/bin/ldecod-trace"

echo "==> restoring production (TRACE=0) ldecod"
cmake --build build --target ldecod -j"$(nproc)" >/dev/null
echo "==> done. Production ldecod and ldecod-trace are now separate."
