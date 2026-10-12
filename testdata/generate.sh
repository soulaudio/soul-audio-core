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
# AIFF (big-endian PCM) and AIFF-C `sowt` (little-endian PCM) — both lossless
# 16-bit, so each decodes bit-exact vs tone.wav (core + CLI e2e). `-f aiff` with a
# little-endian PCM codec makes ffmpeg emit AIFF-C with the `sowt` compression type.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_s16be tone-aiff.aiff
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_s16le -f aiff tone-aifc-sowt.aifc
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a libmp3lame -b:a 192k tone.mp3
# MPEG Layer II (the MP3 decoder also handles L2/L1; this gives L2 a playback-tier
# render golden, not just the kjmp2 decode vectors). No `tone.mp1` — ffmpeg has no
# MPEG Layer I encoder, so the L1 playback fixture reuses the committed conformance
# vector (copied to `tone-layer1.mp1`; see below).
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a mp2 -b:a 192k tone.mp2
cp ../crates/soul-audio-core/testdata/layer1-32k-stereo.mp1 tone-layer1.mp1
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a flac -f ogg tone.ogg
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a alac -movflags +faststart tone.m4a

# AAC-LC: two framings of the same lossy encode. `tone-aac.m4a` is deliberately
# NOT named `tone.m4a` — that name is already the lossless ALAC fixture above
# (same .m4a container, different codec); reusing it would collide with the
# lossless ALAC goldens.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a aac -b:a 192k -movflags +faststart tone-aac.m4a
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a aac -b:a 192k -f adts tone.aac

# Vorbis (libvorbis, quality 5 ~ 160 kbps): the from-scratch Vorbis I decoder's
# stereo golden (CLI `real_vorbis_renders_stereo_tones`, `xtask qa-audio`).
# NOT `tone.ogg` — that name is already the FLAC-in-Ogg fixture above.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a libvorbis -q:a 5 tone-vorbis.ogg

# Dense Vorbis: two independent pink-noise channels at high quality (q8) → a
# heavy, decorrelated residue payload — the #34 fast-Huffman decode hot path (a
# pure tone's residue is misleadingly light). Synthetic, no copyright; 2 s.
ffmpeg -y -hide_banner -loglevel error -filter_complex "anoisesrc=d=2:c=pink:seed=1:a=0.5[l];anoisesrc=d=2:c=pink:seed=2:a=0.5[r];[l][r]join=inputs=2:channel_layout=stereo[a]" -map "[a]" -c:a libvorbis -q:a 8 tone-vorbis-dense.ogg

# HE-AAC (SBR): aac_mf at a low bitrate emits explicit HE-AAC for MP4. Exercises
# the SBR decode path end-to-end in the playback tier (the ADTS-implicit-SBR path
# is covered by the core sbr.aac golden). NOTE: aac_mf emits plain LC for `-f
# adts`, so there is no ADTS SBR fixture here.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a aac_mf -b:a 32k -movflags +faststart tone-sbr.m4a

# Tagged FLAC (title/artist/album): used by the library-scan Some(tag) test.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -t 0.2 -c:a flac \
  -metadata title="Soul Test Track" -metadata artist="Test Artist" -metadata album="Test Album" tagged.flac

# Opus (48 kHz internal): exercises the last-Ogg-page-granule duration path.
# There is otherwise NO .opus fixture (tone.ogg is FLAC-in-Ogg).
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a libopus -b:a 96k tone.opus

# Matroska / WebM: remux the EXISTING Opus and FLAC streams with `-c:a copy`, so
# the contained frames are byte-identical to tone.opus / tone.flac. That lets the
# Matroska decode be checked bit-exact (FLAC) / same-SNR (Opus) against the Ogg
# path — proving the EBML demux + codec seam, not a fresh encode.
ffmpeg -y -hide_banner -loglevel error -i tone.opus -c:a copy -f webm tone-opus.webm
ffmpeg -y -hide_banner -loglevel error -i tone.flac -c:a copy -f matroska tone-flac.mka
# Opus-in-WebM reference (ffmpeg's own decode) for the segmental-SNR e2e.
ffmpeg -y -hide_banner -loglevel error -i tone-opus.webm -f s16le -c:a pcm_s16le tone-opus.webm.ref.pcm

# WavPack (.wv) lossless — stereo (joint + cross decorrelation), mono, and a
# 32-bit INT32_DATA stream (ffmpeg promotes the 16-bit source to s32 with 16
# trailing zero bits). Each decodes bit-exact: stereo vs tone.wav, mono/int32 vs
# the ffmpeg s16le/s32le reference decodes.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a wavpack tone.wv
ffmpeg -y -hide_banner -loglevel error -i tone.wav -ac 1 -c:a wavpack tone-mono.wv
ffmpeg -y -hide_banner -loglevel error -i tone-mono.wv -f s16le tone-mono.wv.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i tone.wav -af aformat=sample_fmts=s32 -c:a wavpack tone-24.wv
ffmpeg -y -hide_banner -loglevel error -i tone-24.wv -f s32le tone-24.wv.ref.pcm

# FLAC with an embedded cover picture: exercises embedded-art extraction. A tiny
# solid-red 16x16 PNG is attached as the front cover.
ffmpeg -y -hide_banner -loglevel error -f lavfi -i color=c=red:s=16x16:d=1 -frames:v 1 _cover.png
ffmpeg -y -hide_banner -loglevel error -i tone.wav -i _cover.png -t 0.2 \
  -map 0:a -map 1:v -c:a flac -c:v copy -disposition:v:0 attached_pic \
  -metadata title="Arted Track" arted.flac
rm -f _cover.png

# DSD: ffmpeg cannot encode PCM->DSD, so synthesize the 1-bit stream ourselves
# (2nd-order sigma-delta + Sony DSF container). Needs python3 + numpy.
python3 generate_dsd.py tone.dsf                      # DSD64  DSF (0.5 s)
python3 generate_dsd.py tone-dsd128.dsf 5644800       # DSD128 DSF (0.5 s)
python3 generate_dsd.py tone-dsd256.dsf 11289600 0.1  # DSD256 DSF (0.1 s, size)
python3 generate_dsd.py tone-dsd512.dsf 22579200 0.1  # DSD512 DSF (0.1 s, size)
python3 generate_dsd.py tone.dff                      # DSD64  DSDIFF/raw (0.5 s)

echo "generated:"
ls -la tone.wav tone-aiff.aiff tone-aifc-sowt.aifc tone.flac tone-flac.mka tone.wv tone-mono.wv tone-24.wv tone.mp3 tone.ogg tone.m4a tone-aac.m4a tone.aac tone-vorbis.ogg tone.dsf tagged.flac tone.opus tone-opus.webm arted.flac tone-dsd128.dsf tone-dsd256.dsf tone-dsd512.dsf tone.dff tone-sbr.m4a tone-vorbis-dense.ogg tone.mp2 tone-layer1.mp1
