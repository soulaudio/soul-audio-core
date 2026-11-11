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
# IEEE-float WAV golden — `tone.wav` re-encoded to 32-bit float. Shared by the
# WAV-float / CAF-float / AIFF-fl32 e2e tests: the decoder converts float to
# full-scale i32, and this file re-encodes the exact 16-bit signal, so the
# round-trip is verifiable (shift back by 16 ⇒ the original samples).
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_f32le tone-f32.wav
# AIFF (big-endian PCM) and AIFF-C `sowt` (little-endian PCM) — both lossless
# 16-bit, so each decodes bit-exact vs tone.wav (core + CLI e2e). `-f aiff` with a
# little-endian PCM codec makes ffmpeg emit AIFF-C with the `sowt` compression type.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_s16be tone-aiff.aiff
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_s16le -f aiff tone-aifc-sowt.aifc
# AIFF-C `fl32` (big-endian IEEE float) and `ulaw` (G.711 µ-law) — exercise the
# float and companding compressionType branches end-to-end. fl32 is bit-identical
# to tone-f32.wav; µ-law decode is a fixed table, so it matches ffmpeg's own
# decode (s16le golden) bit-exact.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_f32be -f aiff tone-aiff-fl32.aifc
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_mulaw -f aiff tone-aiff-ulaw.aifc
ffmpeg -y -hide_banner -loglevel error -i tone-aiff-ulaw.aifc -f s16le tone-aiff-ulaw.ref.pcm
# fl64 (BE 64-bit float, bit-identical to tone-f32.wav) and alaw (G.711 A-law,
# bit-exact vs ffmpeg) complete the AIFF float + companding matrices.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_f64be -f aiff tone-aiff-fl64.aifc
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_alaw -f aiff tone-aiff-alaw.aifc
ffmpeg -y -hide_banner -loglevel error -i tone-aiff-alaw.aifc -f s16le tone-aiff-alaw.ref.pcm
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
ffmpeg -y -hide_banner -loglevel error -i tone-aac.m4a -c:a copy -f matroska tone-aac.mka
ffmpeg -y -hide_banner -loglevel error -i tone-aac.mka -f s16le tone-aac.mka.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i tone-vorbis.ogg -c:a copy -f matroska tone-vorbis.mka
# Opus-in-WebM reference (ffmpeg's own decode) for the segmental-SNR e2e.
ffmpeg -y -hide_banner -loglevel error -i tone-opus.webm -f s16le -c:a pcm_s16le tone-opus.webm.ref.pcm

# Multi-cluster Matroska for the Cues-index seek test (decode.rs
# `matroska_cue_seek`). `-cluster_time_limit 250` forces ~9 clusters in the 2 s
# tone (vs the single cluster of the fixtures above), so a late seek has many
# clusters to skip — the whole point of a Cues index. ffmpeg writes a SeekHead +
# Cues by default for mkv/webm. Opus at 48 kHz and AAC resampled to 48 kHz so the
# ms-granular CueTime maps to a whole-sample (frame-aligned) landing; FLAC stays
# 44.1 kHz because it self-anchors from the frame header (sample-exact regardless).
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a libopus -b:a 96k -cluster_time_limit 250 -f webm tone-opus-cues.webm
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a libopus -b:a 96k -cluster_time_limit 250 -f matroska tone-opus-cues.mka
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a flac -cluster_time_limit 250 -f matroska tone-flac-cues.mka
ffmpeg -y -hide_banner -loglevel error -i tone.wav -ar 48000 -c:a aac -b:a 192k -cluster_time_limit 250 -f matroska tone-aac-cues.mka

# WavPack (.wv) lossless — stereo (joint + cross decorrelation), mono, and a
# 32-bit INT32_DATA stream (ffmpeg promotes the 16-bit source to s32 with 16
# trailing zero bits). Each decodes bit-exact: stereo vs tone.wav, mono/int32 vs
# the ffmpeg s16le/s32le reference decodes.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a wavpack tone.wv
ffmpeg -y -hide_banner -loglevel error -i tone.wav -ac 1 -c:a wavpack tone-mono.wv
ffmpeg -y -hide_banner -loglevel error -i tone-mono.wv -f s16le tone-mono.wv.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i tone.wav -af aformat=sample_fmts=s32 -c:a wavpack tone-24.wv
ffmpeg -y -hide_banner -loglevel error -i tone-24.wv -f s32le tone-24.wv.ref.pcm
# Genuine 24-bit-magnitude WavPack: synthesize the tone directly at 24-bit so the
# content actually uses ~24 significant bits (INT32_INFO zeros=8, peak ≈ 2^30) —
# unlike tone-24.wv, whose 16-bit source gives zeros=16 (16-bit magnitude). This
# exercises the large-residual entropy path + the zeros=8 int32 restore.
ffmpeg -y -hide_banner -loglevel error -f lavfi -i "$GRAPH" -c:a pcm_s24le tone24.wav
ffmpeg -y -hide_banner -loglevel error -i tone24.wav -c:a wavpack tone-wv24.wv
ffmpeg -y -hide_banner -loglevel error -i tone-wv24.wv -f s32le tone-wv24.wv.ref.pcm
rm -f tone24.wav

# Apple CAF: lpcm (big-endian PCM) and alac (Apple Lossless). Both lossless →
# bit-exact vs tone.wav.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_s16be -f caf tone.caf
# CAF lpcm little-endian (clears the big-endian flag → bit-exact vs tone.wav) and
# IEEE-float (sets the float flag → bit-identical to tone-f32.wav). Cover CAF's
# byte-order and float format-flag branches.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_s16le -f caf tone-caf-le.caf
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a pcm_f32le -f caf tone-caf-f32.caf
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a alac -f caf tone-alac.caf

# FLAC with an embedded cover picture: exercises embedded-art extraction. A tiny
# solid-red 16x16 PNG is attached as the front cover.
ffmpeg -y -hide_banner -loglevel error -f lavfi -i color=c=red:s=16x16:d=1 -frames:v 1 _cover.png
ffmpeg -y -hide_banner -loglevel error -i tone.wav -i _cover.png -t 0.2 \
  -map 0:a -map 1:v -c:a flac -c:v copy -disposition:v:0 attached_pic \
  -metadata title="Arted Track" arted.flac
rm -f _cover.png

# DSD: ffmpeg cannot encode PCM->DSD, so synthesize the 1-bit stream ourselves.
# DSD64 fixtures use the local 2nd-order modulator (generate_dsd.py; needs python3
# + numpy). The DSD128/256 fixtures instead come from `deltarec` — a real 7th-order
# sigma-delta PCM->DSD encoder (0 dBFS -> 50% modulation, the SACD convention, so
# the recovered PCM sits ~6 dB down) — run on THIS repo's own license-safe tone, so
# the playback decimator is validated against a genuine modulator, not a toy pattern.
# They exercise the adapter's adaptive 3rd decimation stage that lands 128/256 on
# 44.1 kHz. deltarec: https://…  (build separately; must be on PATH to regenerate).
python3 generate_dsd.py tone.dsf                      # DSD64  DSF (0.5 s)
python3 generate_dsd.py tone-dsd512.dsf 22579200 0.1  # DSD512 DSF (0.1 s, size)
python3 generate_dsd.py tone.dff                      # DSD64  DSDIFF/raw (0.5 s)
if command -v deltarec >/dev/null 2>&1; then
  # Short slices keep the 1-bit fixtures small (~280 KB each).
  ffmpeg -y -hide_banner -loglevel error -i tone.wav -t 0.2 _dsdsrc128.wav
  ffmpeg -y -hide_banner -loglevel error -i tone.wav -t 0.1 _dsdsrc256.wav
  deltarec _dsdsrc128.wav -o tone-dsd128.dsf -r 128 -b 44100 -q  # DSD128 DSF (0.2 s)
  deltarec _dsdsrc256.wav -o tone-dsd256.dsf -r 256 -b 44100 -q  # DSD256 DSF (0.1 s)
  rm -f _dsdsrc128.wav _dsdsrc256.wav
else
  echo "warning: deltarec not on PATH — keeping committed tone-dsd128/256.dsf" >&2
fi

# ---------------------------------------------------------------------------
# Broadband STRESS signals (tests/decode.rs `stress_*`). The 2-tone `tone.wav`
# is near-silent in most bands and compresses trivially, so it never exercises
# subframe-type/Rice selection (lossless) or bit-allocation/short-block/reservoir
# (lossy). These demanding signals do.
#
# complex.wav: 3 s stereo — L a 50 Hz->18 kHz linear chirp (sweeps every band),
# R pink noise (broadband, high-entropy, L!=R stressing stereo decorrelation).
# Lossless (FLAC/ALAC/WavPack) must decode it bit-exact; MP3/Vorbis/Opus must
# hold SNR vs ffmpeg on it.
ffmpeg -y -hide_banner -loglevel error -filter_complex \
"aevalsrc=exprs='0.6*sin(2*PI*(50*t+2991.67*t*t))':s=44100:d=3[c];anoisesrc=d=3:c=pink:seed=7:a=0.4[n];[c][n]join=inputs=2:channel_layout=stereo[a]" \
-map "[a]" -c:a pcm_s16le complex.wav
ffmpeg -y -hide_banner -loglevel error -i complex.wav -c:a flac complex.flac
ffmpeg -y -hide_banner -loglevel error -i complex.wav -c:a alac -movflags +faststart complex-alac.m4a
ffmpeg -y -hide_banner -loglevel error -i complex.wav -c:a wavpack complex.wv
ffmpeg -y -hide_banner -loglevel error -i complex.wav -c:a libmp3lame -b:a 192k complex.mp3
ffmpeg -y -hide_banner -loglevel error -i complex.mp3 -f s16le complex.mp3.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i complex.wav -c:a libvorbis -q:a 5 complex-vorbis.ogg
ffmpeg -y -hide_banner -loglevel error -i complex-vorbis.ogg -f s16le complex-vorbis.ogg.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i complex.wav -c:a libopus -b:a 128k complex.opus
ffmpeg -y -hide_banner -loglevel error -i complex.opus -f s16le complex.opus.ref.pcm
# chirp.wav: L=R full-band chirp, no noise -> forces AAC EIGHT_SHORT transient
# windows WITHOUT triggering PNS (which is decoder-specific random fill and
# un-matchable to ffmpeg's PRNG on pure noise). The AAC stress gate uses this.
ffmpeg -y -hide_banner -loglevel error -filter_complex \
"aevalsrc=exprs='0.6*sin(2*PI*(50*t+2991.67*t*t))|0.6*sin(2*PI*(50*t+2991.67*t*t))':s=44100:d=3[a]" \
-map "[a]" -c:a pcm_s16le chirp.wav
ffmpeg -y -hide_banner -loglevel error -i chirp.wav -c:a aac -b:a 192k -movflags +faststart chirp-aac.m4a
ffmpeg -y -hide_banner -loglevel error -i chirp-aac.m4a -f s16le chirp-aac.m4a.ref.pcm
ffmpeg -y -hide_banner -loglevel error -c:a aac_fixed -i chirp-aac.m4a -f s16le chirp-aac.m4a.fixed.pcm

# Full-scale (0 dBFS) edge signals — the lossless integer predictors/entropy
# coders are stressed hardest at the top of the sample range (largest residuals,
# where an unguarded i32 multiply/add would overflow). fullscale.wav: independent
# L/R white noise clipped to ±full-scale. antiphase.wav: R = −L white noise, the
# mid=0/side=max extreme of FLAC mid-side / WavPack joint-stereo decorrelation.
# All must decode bit-exact.
ffmpeg -y -hide_banner -loglevel error -filter_complex \
"anoisesrc=d=2:c=white:seed=11:a=1.0[l];anoisesrc=d=2:c=white:seed=22:a=1.0[r];[l][r]join=inputs=2:channel_layout=stereo[a]" \
-map "[a]" -c:a pcm_s16le fullscale.wav
ffmpeg -y -hide_banner -loglevel error -i fullscale.wav -c:a flac fullscale.flac
ffmpeg -y -hide_banner -loglevel error -i fullscale.wav -c:a wavpack fullscale.wv
ffmpeg -y -hide_banner -loglevel error -i fullscale.wav -c:a alac -movflags +faststart fullscale-alac.m4a
ffmpeg -y -hide_banner -loglevel error -filter_complex \
"anoisesrc=d=2:c=white:seed=33:a=1.0[m];[m]asplit=2[l][r0];[r0]volume=-1[r];[l][r]join=inputs=2:channel_layout=stereo[o]" \
-map "[o]" -c:a pcm_s16le antiphase.wav
ffmpeg -y -hide_banner -loglevel error -i antiphase.wav -c:a flac antiphase.flac
ffmpeg -y -hide_banner -loglevel error -i antiphase.wav -c:a wavpack antiphase.wv

# Config-matrix coverage — every other fixture is 44.1 kHz MPEG-1 stereo / 16-bit,
# so these reach the decode paths real-world files use that nothing else does:
# MP3 MPEG-2 LSF (22 kHz) + MPEG-2.5 (11 kHz) + mono (halved granules, LSF
# scalefactor tables, mono path); 24-bit FLAC (hi-res wide-sample path); AAC at
# 48 kHz (a different scalefactor-band table). Lossless = bit-exact; lossy = SNR
# vs ffmpeg decoding the same bitstream.
ffmpeg -y -hide_banner -loglevel error -i complex.wav -ar 22050 -c:a libmp3lame -b:a 128k mp3-mpeg2-22k.mp3
ffmpeg -y -hide_banner -loglevel error -i mp3-mpeg2-22k.mp3 -f s16le mp3-mpeg2-22k.mp3.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i complex.wav -ar 11025 -c:a libmp3lame -b:a 64k mp3-mpeg25-11k.mp3
ffmpeg -y -hide_banner -loglevel error -i mp3-mpeg25-11k.mp3 -f s16le mp3-mpeg25-11k.mp3.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i complex.wav -ac 1 -c:a libmp3lame -b:a 128k mp3-mono.mp3
ffmpeg -y -hide_banner -loglevel error -i mp3-mono.mp3 -f s16le mp3-mono.mp3.ref.pcm
ffmpeg -y -hide_banner -loglevel error -filter_complex \
"aevalsrc=exprs='0.6*sin(2*PI*(50*t+2991.67*t*t))':s=44100:d=3[c];anoisesrc=d=3:c=pink:seed=7:a=0.4[n];[c][n]join=inputs=2:channel_layout=stereo[a]" \
-map "[a]" -c:a pcm_s24le complex24.wav
ffmpeg -y -hide_banner -loglevel error -i complex24.wav -c:a flac complex24.flac
ffmpeg -y -hide_banner -loglevel error -i complex24.wav -c:a alac -movflags +faststart complex24-alac.m4a
ffmpeg -y -hide_banner -loglevel error -i chirp.wav -ar 48000 -c:a aac -b:a 192k -movflags +faststart chirp-aac-48k.m4a
ffmpeg -y -hide_banner -loglevel error -i chirp-aac-48k.m4a -f s16le chirp-aac-48k.m4a.ref.pcm

echo "generated:"
ls -la tone.wav tone-f32.wav tone-aiff.aiff tone-aifc-sowt.aifc tone-aiff-fl32.aifc tone-aiff-fl64.aifc tone-aiff-ulaw.aifc tone-aiff-alaw.aifc tone.flac tone-flac.mka tone-aac.mka tone-vorbis.mka tone.wv tone-mono.wv tone-24.wv tone-wv24.wv tone.caf tone-caf-le.caf tone-caf-f32.caf tone-alac.caf tone.mp3 tone.ogg tone.m4a tone-aac.m4a tone.aac tone-vorbis.ogg tone.dsf tagged.flac tone.opus tone-opus.webm arted.flac tone-dsd128.dsf tone-dsd256.dsf tone-dsd512.dsf tone.dff tone-sbr.m4a tone-vorbis-dense.ogg tone.mp2 tone-layer1.mp1 \
  tone-opus-cues.webm tone-opus-cues.mka tone-flac-cues.mka tone-aac-cues.mka \
  complex.wav complex.flac complex-alac.m4a complex.wv complex.mp3 complex-vorbis.ogg complex.opus chirp.wav chirp-aac.m4a \
  fullscale.wav fullscale.flac fullscale.wv fullscale-alac.m4a antiphase.wav antiphase.flac antiphase.wv \
  mp3-mpeg2-22k.mp3 mp3-mpeg25-11k.mp3 mp3-mono.mp3 complex24.wav complex24.flac complex24-alac.m4a chirp-aac-48k.m4a

# Real-music fixtures. realmusic.wav is a committed 4 s CD-quality clip of the
# CC0 / public-domain "Open Goldberg Variations" (Kimiko Ishizaka, Bach BWV 988,
# Aria) — a real recording no ffmpeg synth can reproduce, so it is committed and
# the codec variants below are derived from it. Genuine musical structure (note
# onsets → short blocks, harmonics, decay tails, stereo imaging) that no tone/
# chirp/noise fixture covers. Lossless = bit-exact; lossy = SNR vs ffmpeg.
if [ -f realmusic.wav ]; then
  ffmpeg -y -hide_banner -loglevel error -i realmusic.wav -c:a flac realmusic.flac
  ffmpeg -y -hide_banner -loglevel error -i realmusic.wav -c:a alac -movflags +faststart realmusic-alac.m4a
  ffmpeg -y -hide_banner -loglevel error -i realmusic.wav -c:a wavpack realmusic.wv
  ffmpeg -y -hide_banner -loglevel error -i realmusic.wav -c:a libmp3lame -b:a 192k realmusic.mp3
  ffmpeg -y -hide_banner -loglevel error -i realmusic.mp3 -f s16le realmusic.mp3.ref.pcm
  ffmpeg -y -hide_banner -loglevel error -i realmusic.wav -c:a aac -b:a 192k -movflags +faststart realmusic-aac.m4a
  ffmpeg -y -hide_banner -loglevel error -i realmusic-aac.m4a -f s16le realmusic-aac.m4a.ref.pcm
  ffmpeg -y -hide_banner -loglevel error -i realmusic.wav -c:a libvorbis -q:a 5 realmusic-vorbis.ogg
  ffmpeg -y -hide_banner -loglevel error -i realmusic-vorbis.ogg -f s16le realmusic-vorbis.ogg.ref.pcm
  ffmpeg -y -hide_banner -loglevel error -i realmusic.wav -c:a libopus -b:a 128k realmusic.opus
  ffmpeg -y -hide_banner -loglevel error -i realmusic.opus -f s16le realmusic.opus.ref.pcm
fi
