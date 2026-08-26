#!/usr/bin/env bash
# Regenerate the committed audio fixtures with ffmpeg.
#
# Every fixture carries the SAME 2-second stereo signal — left = 440 Hz (A4),
# right = 659.25 Hz (E5), -6 dB, 50 ms in/out fades — so:
#   * each format should sound identical (decode correctness), and
#   * played back-to-back the sample-exact formats (WAV/FLAC) are one continuous
#     chord (gapless).
# 44.1 kHz on purpose: playback on a 48 kHz device exercises the resampler too.
#
# Run:  bash testdata/generate.sh   (needs ffmpeg with libmp3lame + flac)
set -euo pipefail
cd "$(dirname "$0")"

GRAPH="aevalsrc=exprs=0.5*sin(2*PI*440*t)|0.5*sin(2*PI*659.25*t):s=44100:d=2:c=stereo,afade=t=in:d=0.05,afade=t=out:st=1.95:d=0.05"

ffmpeg -y -hide_banner -loglevel error -f lavfi -i "$GRAPH" -c:a pcm_s16le tone.wav
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a flac tone.flac
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a libmp3lame -b:a 192k tone.mp3
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a flac -f ogg tone.ogg
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a alac -movflags +faststart tone.m4a

# DSD: ffmpeg cannot encode PCM->DSD, so synthesize the 1-bit stream ourselves
# (2nd-order sigma-delta + Sony DSF container). Needs python3 + numpy.
python3 generate_dsd.py tone.dsf

echo "generated:"
ls -la tone.wav tone.flac tone.mp3 tone.ogg tone.m4a tone.dsf
