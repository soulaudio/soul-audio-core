#!/usr/bin/env bash
# Fetch the ISO/IEC MPEG audio conformance vectors (.bit + reference .pcm) used
# by tests/mp3_conformance.rs. They are ISO-copyright, so they are NOT committed
# (see .gitignore); this pulls the set the minimp3 project ships for conformance
# (our Layer III decoder is a minimp3 port). Run from anywhere:
#   bash testdata/fetch-mp3-conformance.sh
set -euo pipefail
cd "$(dirname "$0")"
DIR="mp3-conformance-local"
BASE="https://raw.githubusercontent.com/lieff/minimp3/master/vectors"
mkdir -p "$DIR"
# Layer III (ISO full accuracy) + Layer I/II (kjmp2 limited accuracy). Add more
# names from https://github.com/lieff/minimp3/tree/master/vectors as desired.
VECTORS="ILL2_layer3 M2L3_compl24 M2L3_noise ILL2_layer1 \
         ILL2_center2 ILL2_dual ILL2_mono ILL2_samples ILL2_scf63 ILL2_multilingual"
for n in $VECTORS; do
  for ext in bit pcm; do
    curl -fsS -m 60 -o "$DIR/$n.$ext" "$BASE/$n.$ext" \
      && echo "  $n.$ext" \
      || echo "  SKIP $n.$ext (not found)"
  done
done
echo "Fetched into testdata/$DIR/ (gitignored). Run: cargo test -p soul-audio-core --test mp3_conformance"
