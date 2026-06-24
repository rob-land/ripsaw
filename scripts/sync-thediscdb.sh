#!/usr/bin/env bash
# Sync a local mirror of TheDiscDB's open catalogue (TheDiscDb/data) so
# Ripsaw can identify discs offline and when the hosted GraphQL endpoint
# is down. See docs/thediscdb-local.md.
#
# Only the JSON metadata is fetched (the lookup needs nothing else): a
# blobless, sparse checkout that excludes the per-disc .txt summaries and
# the cover images (~1.5 GB of the ~1.9 GB repo). The result is the
# `data/` tree under $MIRROR, which is exactly what Ripsaw indexes.
#
# Usage:   scripts/sync-thediscdb.sh
# Env:     MIRROR  (default: $XDG_CACHE_HOME/ripsaw/thediscdb,
#                   else ~/.cache/ripsaw/thediscdb)
set -euo pipefail

REPO="https://github.com/TheDiscDb/data.git"
CACHE_BASE="${XDG_CACHE_HOME:-$HOME/.cache}"
MIRROR="${MIRROR:-$CACHE_BASE/ripsaw/thediscdb}"

if [ -d "$MIRROR/.git" ]; then
  echo "==> refreshing existing mirror at $MIRROR"
  git -C "$MIRROR" sparse-checkout reapply
  git -C "$MIRROR" pull --ff-only
else
  echo "==> cloning JSON subset of $REPO into $MIRROR"
  mkdir -p "$MIRROR"
  # Blobless partial clone: history without file contents; blobs are
  # fetched on demand for the sparse paths only.
  git clone --filter=blob:none --no-checkout "$REPO" "$MIRROR"
  git -C "$MIRROR" sparse-checkout init --no-cone
  # Include every .json anywhere; exclude the heavyweight txt + images.
  git -C "$MIRROR" sparse-checkout set --no-cone \
    '/**/*.json' \
    '!/**/*.txt' \
    '!/**/*.jpg' \
    '!/**/*.jpeg' \
    '!/**/*.png' \
    '!/**/*.webp'
  git -C "$MIRROR" checkout
fi

discs=$(find "$MIRROR/data" -name 'disc*.json' 2>/dev/null | wc -l | tr -d ' ')
echo "==> mirror ready: $MIRROR ($discs disc records)"
