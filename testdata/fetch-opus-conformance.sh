#!/usr/bin/env bash
# Fetch the IETF Opus test vectors (RFC 8251) used by the full-corpus mode of
# tests/opus_conformance.rs. ~75 MB download, ~300 MB extracted, so they are NOT
# committed (see .gitignore) — this pulls them on demand, mirroring
# fetch-mp3-conformance.sh. Run from anywhere:
#   bash testdata/fetch-opus-conformance.sh
#
# Then (release, or it takes minutes rather than seconds):
#   cargo test -p soul-audio-core --release --test opus_conformance \
#       full_corpus -- --ignored --nocapture
#
# The suite also honours OPUS_VECTORS_DIR if you already have the vectors
# elsewhere; without either it skips with a message.
set -euo pipefail
cd "$(dirname "$0")"

DIR="opus-conformance-local"
URL="https://opus-codec.org/docs/opus_testvectors-rfc8251.tar.gz"

if [ -f "$DIR/testvector01.bit" ]; then
  echo "Vectors already present in testdata/$DIR/ — nothing to do."
  exit 0
fi

mkdir -p "$DIR"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Fetching $URL (~75 MB) …"
curl -fSL --retry 3 -m 900 -o "$tmp/v.tar.gz" "$URL"

echo "Extracting …"
tar xzf "$tmp/v.tar.gz" -C "$tmp"
# The tarball contains an `opus_newvectors/` directory; flatten it so the test
# finds `testvectorNN.bit` directly under $DIR (and so OPUS_VECTORS_DIR can
# point at either shape).
src="$tmp/opus_newvectors"
[ -d "$src" ] || src="$tmp"
mv "$src"/testvector*.bit "$src"/testvector*.dec "$DIR"/

n=$(ls "$DIR"/testvector*.bit 2>/dev/null | wc -l)
echo "Fetched $n vector(s) into testdata/$DIR/ (gitignored)."
[ "$n" -eq 12 ] || { echo "WARNING: expected 12 vectors, got $n" >&2; exit 1; }
echo "Run: cargo test -p soul-audio-core --release --test opus_conformance full_corpus -- --ignored --nocapture"
