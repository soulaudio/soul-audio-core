//! Integration tests exercising the public decode API end-to-end.

use soul_audio_core::demux::ogg_packets::OggPackets;
use soul_audio_core::formats::opus::toc::{self, Mode};
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{DecodeError, Decoder, SeekableDecoder, SliceSource, open};

/// A minimal valid 16-bit mono WAV carrying `samples`.
fn wav16(samples: &[i16]) -> Vec<u8> {
    let mut data = Vec::new();
    for s in samples {
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&2u16.to_le_bytes()); // stereo
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&176_400u32.to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

#[test]
fn open_probes_and_decodes_wav_end_to_end() {
    // Interleaved stereo: L0,R0, L1,R1 …
    let samples = [10i16, -10, 20, -20, 30, -30];
    let bytes = wav16(&samples);
    let mut dec = open(SliceSource::new(&bytes)).expect("probe + construct");
    assert_eq!(dec.stream_info().channels, 2);
    assert_eq!(dec.stream_info().sample_rate, 44_100);

    let mut out = [0i32; 16];
    let n = dec.decode_into(&mut out).unwrap();
    assert_eq!(n, samples.len());
    for (got, want) in out[..n].iter().zip(samples.iter()) {
        assert_eq!(*got, i32::from(*want));
    }
    assert_eq!(dec.decode_into(&mut out).unwrap(), 0); // end of stream
}

#[test]
fn open_rejects_unknown_container() {
    let result = open(SliceSource::new(b"XXXXYYYY"));
    assert_eq!(result.err(), Some(DecodeError::Unsupported));
}

#[test]
fn open_decodes_flac_in_ogg_matching_ffmpeg() {
    // Real ffmpeg-encoded FLAC-in-Ogg; reference = ffmpeg's own decode.
    let ogg = include_bytes!("../testdata/flac-in-ogg.ogg");
    let mut dec = open(SliceSource::new(ogg)).expect("probe + construct Ogg-FLAC");
    assert_eq!(dec.stream_info().sample_rate, 44_100);
    assert_eq!(dec.stream_info().channels, 1);
    let mut out = Vec::new();
    let mut buf = [0i32; 4096];
    loop {
        let n = dec.decode_into(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out.len(), 13230);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &s in &out {
        h ^= u64::from(s as u32);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    assert_eq!(h, 0xc9ba_54f0_06b2_9f75);
}

#[test]
fn ogg_flac_reports_duration_but_rejects_seek() {
    // Streaming Ogg-FLAC: the source is not seekable, so seek returns Unsupported.
    // (This fixture's STREAMINFO declares no total, so duration is unknown — a
    // valid streaming case; total_frames just must not panic.)
    let ogg = include_bytes!("../testdata/flac-in-ogg.ogg");
    let mut dec = open(SliceSource::new(ogg)).expect("open ogg-flac");
    assert_eq!(dec.total_frames(), None);
    assert_eq!(dec.seek_to(1000), Err(DecodeError::Unsupported));
    assert_eq!(dec.seek_to_time(0.1), Err(DecodeError::Unsupported));
}

#[test]
fn open_surfaces_flac_stream_info() {
    // fLaC + STREAMINFO (44.1 kHz stereo 16-bit) with no audio frames.
    let mut si = [0u8; 34];
    let v: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36);
    si[10..18].copy_from_slice(&v.to_be_bytes());
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"fLaC");
    bytes.push(0x80);
    bytes.extend_from_slice(&[0, 0, 34]);
    bytes.extend_from_slice(&si);

    let mut dec = open(SliceSource::new(&bytes)).expect("probe + construct");
    assert_eq!(dec.stream_info().sample_rate, 44_100);
    assert_eq!(dec.stream_info().channels, 2);
    // No frames follow the metadata, so the FLAC codec reports a clean end of
    // stream (frame-level decode is covered by the unit tests in `formats::flac`).
    assert_eq!(dec.decode_into(&mut [0i32; 4]), Ok(0));
}

/// Decode a full stream through the public `open` API and return interleaved
/// PCM together with the decoder's final reported channel count.
///
/// Runs on a generous stack: an `AnyDecoder` is large by value (no-alloc), and
/// the HE-AAC (SBR) decoder in particular carries ~250 KiB of QMF/HF state plus
/// per-frame DSP scratch — more than the default test-thread stack. This mirrors
/// the CLI's own big-stack loader thread (see `soul-audio-cli` / MEMORY.md).
///
/// The channel count is read *after* draining the stream so it reflects any
/// mid-stream promotion — HE-AACv2 Parametric Stereo starts on a mono core and
/// promotes `stream_info().channels` 1→2 once the first PS frame's `apply` runs
/// (reconciliation R6).
fn decode_all_with_channels(bytes: &[u8]) -> (Vec<i32>, u16) {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(scope, || {
                let mut dec = open(SliceSource::new(bytes)).expect("probe + construct");
                let mut out = Vec::new();
                let mut buf = [0i32; 4096];
                loop {
                    let n = dec.decode_into(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                let channels = dec.stream_info().channels;
                (out, channels)
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked")
    })
}

/// Decode a full stream through the public `open` API and return interleaved PCM.
fn decode_all(bytes: &[u8]) -> Vec<i32> {
    decode_all_with_channels(bytes).0
}

fn fnv1a(samples: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &s in samples {
        h ^= u64::from(s as u32);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// AAC-LC conformance vs ffmpeg (Task 8). AAC is LOSSY, so — unlike the lossless
// ALAC/FLAC goldens — this is a tolerance gate, not a bit-exact one: two
// spec-conformant decoders differ only by IMDCT/window float rounding and track
// closely. Metric = segmental SNR of our decode vs ffmpeg's own s16le decode,
// aligned over the AAC encoder-priming delay.
// ---------------------------------------------------------------------------

/// Interpret a little-endian signed-16-bit PCM blob (ffmpeg's `-f s16le` output)
/// as interleaved `i32` samples — the same representation our decoder emits.
fn load_s16le(bytes: &[u8]) -> Vec<i32> {
    bytes
        .chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect()
}

/// Find the per-channel priming offset (in interleaved samples, a multiple of
/// `channels`) that best aligns `ours` to `refr`, and return that offset with
/// its segmental SNR in dB.
///
/// AAC encoders prepend priming/decoder-delay samples; ffmpeg's MP4 decode trims
/// them via the edit list while our raw decode keeps them, so `ours` leads
/// `refr` by up to a few frames. We slide `ours` forward by `0..=3072`
/// per-channel frames and, for each, compute
/// `SNR = 10·log10( Σ ref² / Σ (ref − ours)² )` over the overlap, keeping the
/// maximum (best alignment).
fn best_align_segsnr(ours: &[i32], refr: &[i32], channels: usize) -> (usize, f64) {
    let mut best = (0usize, f64::NEG_INFINITY);
    for off_frames in 0..=3072usize {
        let off = off_frames * channels;
        if off >= ours.len() {
            break;
        }
        let overlap = (ours.len() - off).min(refr.len());
        // Require a substantial overlap so a tiny tail can't win trivially.
        if overlap < refr.len() / 2 {
            continue;
        }
        let mut sig = 0f64;
        let mut err = 0f64;
        for i in 0..overlap {
            let r = refr[i] as f64;
            let o = ours[off + i] as f64;
            sig += r * r;
            let d = r - o;
            err += d * d;
        }
        let snr = if err <= 0.0 {
            f64::INFINITY
        } else {
            10.0 * (sig / err).log10()
        };
        if snr > best.1 {
            best = (off_frames, snr);
        }
    }
    best
}

/// MP4/.m4a AAC-LC (`aac.m4a`) decoded via `open()` matches ffmpeg's own decode
/// (`aac.m4a.ref.pcm`, s16le) within tolerance.
///
/// Measured segmental SNR = 55.3 dB at a 1-frame (1024-sample) priming offset;
/// the bar is 45 dB (~10 dB margin; cf. the design spec's "target segmental SNR
/// well above ~40 dB" and the ALAC SFDR-bar convention of calibrating from the
/// measured value). The fixtures retain the encoder's PNS (frame 0 uses noise
/// codebook 13), which this v1 renders as SILENT — a documented Phase-1
/// limitation. PNS noise is non-reproducible across decoders (ffmpeg synthesizes
/// it with its own PRNG), so those silenced bands account for essentially all of
/// the gap below the ~75 dB a PNS-free encode scores; whole-signal SNR still
/// clears the bar comfortably.
#[test]
fn open_decodes_aac_mp4_matching_ffmpeg_within_tolerance() {
    let m4a = include_bytes!("../testdata/aac.m4a");
    let refr = load_s16le(include_bytes!("../testdata/aac.m4a.ref.pcm"));
    let dec = open(SliceSource::new(m4a)).expect("probe + construct AAC/MP4");
    assert_eq!(dec.stream_info().sample_rate, 44_100);
    assert_eq!(dec.stream_info().channels, 2);
    let ours = decode_all(m4a);
    let (off, snr) = best_align_segsnr(&ours, &refr, 2);
    assert!(
        snr > 45.0,
        "AAC/MP4 decode SNR vs ffmpeg too low: {snr:.1} dB (offset {off} frames)"
    );
}

/// Raw ADTS `.aac` AAC-LC (`aac.aac`) decoded via `open()` matches ffmpeg's own
/// decode (`aac.aac.ref.pcm`, s16le) within tolerance.
///
/// Measured segmental SNR = 55.3 dB at a 0-frame offset (raw ADTS has no edit
/// list, so ffmpeg keeps the priming too and the streams align at the front);
/// bar = 45 dB. See the MP4 test above for the calibration rationale and the
/// PNS-silence note (the same fixture content, ADTS-framed).
#[test]
fn open_decodes_aac_adts_matching_ffmpeg_within_tolerance() {
    let aac = include_bytes!("../testdata/aac.aac");
    let refr = load_s16le(include_bytes!("../testdata/aac.aac.ref.pcm"));
    let dec = open(SliceSource::new(aac)).expect("probe + construct AAC/ADTS");
    assert_eq!(dec.stream_info().sample_rate, 44_100);
    assert_eq!(dec.stream_info().channels, 2);
    assert_eq!(dec.total_frames(), None); // ADTS is streaming; no declared length
    let ours = decode_all(aac);
    let (off, snr) = best_align_segsnr(&ours, &refr, 2);
    assert!(
        snr > 45.0,
        "AAC/ADTS decode SNR vs ffmpeg too low: {snr:.1} dB (offset {off} frames)"
    );
}

/// HE-AAC (AAC-LC + SBR) conformance vs ffmpeg for both signalling paths
/// (`sbr.m4a` = explicit HE-AAC in MP4, `sbr.aac` = implicit SBR in raw ADTS).
///
/// This is the regression gate for the SBR HF-reconstruction fidelity fix: both
/// QMF banks must use FFmpeg's/faad2's **exact absolute-phase convention**
/// (analysis `cos/sin(π·(k+½)·(2n−0.5)/64)`; synthesis `−cos/−sin(π·(k+½)·
/// (2n+1)/128)`), derived from faad2's `dct4_kernel` form. An earlier
/// self-consistent-but-wrong pair (analysis `2n−96.5`, synthesis `+(2n−63)`)
/// round-trips and matches FFmpeg on the low-band passthrough, but SBR HF
/// generation copies `X_low[p]` up to band `k`, and the ISO patch is only
/// correct in FFmpeg's convention — so a mismatched convention decorrelates the
/// high band (and, downstream, the noise/sine added in that domain). Matching
/// both banks to FFmpeg makes the whole HE-AAC decode near-bit-exact vs ffmpeg:
/// measured segmental SNR jumps from ~11.8 dB (old convention) to ~104 dB.
///
/// The 90 dB bar (≈14 dB below the ~104 dB measured) is deterministic IEEE-754
/// f32 QMF math against the s16le reference; it is far above the ~25-40 dB a
/// conformant HE-AAC decode needs, and decisively distinguishes the fix from any
/// per-band phase regression (which drops SNR by tens of dB).
#[test]
fn open_decodes_he_aac_sbr_matching_ffmpeg() {
    for (label, data, refpcm) in [
        (
            "sbr.m4a",
            include_bytes!("../testdata/sbr.m4a").as_slice(),
            load_s16le(include_bytes!("../testdata/sbr.m4a.ref.pcm")),
        ),
        (
            "sbr.aac",
            include_bytes!("../testdata/sbr.aac").as_slice(),
            load_s16le(include_bytes!("../testdata/sbr.aac.ref.pcm")),
        ),
    ] {
        // (Up-front rate/channel reporting is covered by `formats::aac`'s own
        // tests; here the SNR vs the 2x-rate ffmpeg reference implicitly gates
        // that SBR activated — a non-SBR 22050 decode scores near-zero against
        // the 44100 reference.)
        open(SliceSource::new(data)).expect("probe + construct HE-AAC");
        let ours = decode_all(data);
        let (off, snr) = best_align_segsnr(&ours, &refpcm, 2);
        assert!(
            snr > 90.0,
            "{label}: HE-AAC SBR SNR vs ffmpeg too low: {snr:.2} dB (offset {off} frames) \
             — the QMF phase-convention fix should keep this near-bit-exact"
        );
    }
}

/// Structural sanity for the SBR decode, independent of ffmpeg (a backstop the
/// conformance gate above can't provide if the reference PCM were ever wrong):
/// the output must be at the **2× SBR rate** (per-channel length within a frame
/// of ffmpeg's 2× reference), non-silent, entirely within the i16 range, and not
/// pinned to full-scale (a clipped/garbage decode).
#[test]
fn sbr_output_is_2x_and_non_silent() {
    let aac = include_bytes!("../testdata/sbr.aac");
    let ours = decode_all(aac); // interleaved i32, 2 channels
    let refr = load_s16le(include_bytes!("../testdata/sbr.aac.ref.pcm"));
    let ours_per_ch = ours.len() / 2;
    let ref_per_ch = refr.len() / 2;
    assert!(
        (ours_per_ch as i64 - ref_per_ch as i64).abs() <= 4096,
        "SBR output length {ours_per_ch}/ch not ~2× (ffmpeg ref {ref_per_ch}/ch)"
    );
    let rms = (ours.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / ours.len() as f64).sqrt();
    assert!(rms > 1.0, "SBR output silent (rms {rms:.2})");
    assert!(
        ours.iter().all(|&s| (-32768..=32767).contains(&s)),
        "SBR output outside i16 range"
    );
    assert!(
        !ours.iter().all(|&s| s.abs() >= 32767),
        "SBR output fully clipped (garbage)"
    );
}

/// MPEG-1/2 **Layer II** golden vectors. References are decoded by kjmp2 (Martin
/// Fiedler, the decoder this port follows); output is bit-exact against it. The
/// set spans allocation tables A/B/C/D, mono + stereo, and 32/44.1/48 kHz.
#[test]
fn open_decodes_layer2_matching_kjmp2() {
    struct Case {
        data: &'static [u8],
        rate: u32,
        ch: u16,
        samples: usize,
        fnv: u64,
    }
    let cases = [
        // mono 44.1 kHz 128 kbps → table B
        Case {
            data: include_bytes!("../testdata/l2_m44_128.mp2"),
            rate: 44_100,
            ch: 1,
            samples: 13_824,
            fnv: 0xda7b_4ad1_b2ea_fcf1,
        },
        // stereo 44.1 kHz 128 kbps → table A
        Case {
            data: include_bytes!("../testdata/l2_s44_128.mp2"),
            rate: 44_100,
            ch: 2,
            samples: 27_648,
            fnv: 0x5f56_1272_433b_0ecd,
        },
        // mono 44.1 kHz 48 kbps → table C (sblimit 8)
        Case {
            data: include_bytes!("../testdata/l2_m44_48.mp2"),
            rate: 44_100,
            ch: 1,
            samples: 13_824,
            fnv: 0x1175_5393_00f7_6d13,
        },
        // mono 32 kHz 48 kbps → table D (sblimit 12)
        Case {
            data: include_bytes!("../testdata/l2_m32_48.mp2"),
            rate: 32_000,
            ch: 1,
            samples: 10_368,
            fnv: 0x7096_2bc4_c107_79fe,
        },
        // stereo 48 kHz 256 kbps → table A
        Case {
            data: include_bytes!("../testdata/l2_s48_256.mp2"),
            rate: 48_000,
            ch: 2,
            samples: 29_952,
            fnv: 0x9f6c_9a7f_4b09_77aa,
        },
    ];
    for c in cases {
        let dec = open(SliceSource::new(c.data)).expect("probe + construct MP2");
        assert_eq!(dec.stream_info().sample_rate, c.rate);
        assert_eq!(dec.stream_info().channels, c.ch);
        let pcm = decode_all(c.data);
        assert_eq!(pcm.len(), c.samples, "sample count");
        assert_eq!(fnv1a(&pcm), c.fnv, "bit-exact vs kjmp2");
    }
}

/// MPEG-1 **Layer I** decode. Reference is a real ISO Layer I conformance vector
/// (`mp1-sample.mp1` from the FFmpeg samples server: 32 kHz stereo, 48 audio
/// frames after a leading warm-up frame). Two checks:
///
/// 1. A regression hash locking this decoder's exact (deterministic) output.
/// 2. A tolerance check against ffmpeg's own decode over a 16-frame warm window:
///    every sample is within 128 of ffmpeg. (Bit-exact is impossible — ffmpeg
///    uses a higher-precision fixed-point filterbank; the whole-signal max
///    deviation measured 83.) The window skips the first output frame, where
///    ffmpeg's decoder-delay convention discards a frame that this decoder — like
///    kjmp2 for Layer II — emits, so the two are offset by exactly one frame.
#[test]
fn open_decodes_layer1_conformance_vector() {
    let mp1 = include_bytes!("../testdata/layer1-32k-stereo.mp1");
    let dec = open(SliceSource::new(mp1)).expect("probe + construct MP1");
    assert_eq!(dec.stream_info().sample_rate, 32_000);
    assert_eq!(dec.stream_info().channels, 2);

    let pcm = decode_all(mp1);
    assert_eq!(pcm.len(), 49 * 384 * 2, "49 frames × 384 samples × stereo");
    assert_eq!(fnv1a(&pcm), 0xf57c_6f7c_2fec_67f7, "regression hash");

    // ffmpeg reference window = its output frames 1..=16 (interleaved i16). Our
    // output leads ffmpeg by one frame, so ffmpeg frame f aligns to our f+1:
    // window[k] (= ffmpeg sample 768+k) matches our sample 1536+k.
    let ff: &[u8] = include_bytes!("../testdata/layer1-32k-stereo.ffref16.pcm");
    let window: Vec<i32> = ff
        .chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect();
    let mut maxdiff = 0i32;
    for (k, &want) in window.iter().enumerate() {
        maxdiff = maxdiff.max((pcm[1536 + k] - want).abs());
    }
    assert!(
        maxdiff <= 128,
        "within tolerance of ffmpeg (maxdiff={maxdiff})"
    );
}

// ---------------------------------------------------------------------------
// AAC Parametric Stereo (HE-AACv2, Phase 3) — local-only conformance scaffold
// (Task 0). The fixture is license-restricted (Coding Technologies aacPlus
// DecoderCheck) and gitignored, so it is NEVER committed and NEVER embedded
// via `include_bytes!` (that would fail the build on a clean checkout). It is
// read at runtime with `std::fs::read`; absence (CI / any clean checkout) is
// a no-op skip, not a failure. See `testdata/README.md` for provenance and
// `.superpowers/sdd/ps-reference.md` §8 for the seam this exercises.
// ---------------------------------------------------------------------------

/// Load the gitignored local PS fixture at test runtime. `None` (→ the caller
/// skips) when either file is absent — the CI / clean-checkout case. NEVER
/// `include_bytes!` (the files are not committed).
fn ps_local_fixture() -> Option<(Vec<u8>, Vec<i32>)> {
    let dir = env!("CARGO_MANIFEST_DIR");
    let m4a = std::fs::read(format!("{dir}/testdata/ps-local.m4a")).ok()?;
    let refpcm = std::fs::read(format!("{dir}/testdata/ps-local.ref.pcm")).ok()?;
    Some((m4a, load_s16le(&refpcm))) // load_s16le already exists in this file
}

/// De-interleave channel `ch` of `channels`-interleaved PCM into a mono vec.
fn deinterleave(pcm: &[i32], channels: usize, ch: usize) -> Vec<i32> {
    pcm.iter().skip(ch).step_by(channels).copied().collect()
}

/// Segmental SNR (dB) of `ours[off..]` against `refr` over their overlap:
/// `10·log10( Σ ref² / Σ (ref − ours)² )`. Both are mono (single-channel)
/// here; `off` is the per-channel priming offset already found on the
/// interleaved pair, applied identically to each channel.
fn seg_snr_at(ours: &[i32], refr: &[i32], off: usize) -> f64 {
    if off >= ours.len() {
        return f64::NEG_INFINITY;
    }
    let overlap = (ours.len() - off).min(refr.len());
    let mut sig = 0f64;
    let mut err = 0f64;
    for i in 0..overlap {
        let r = refr[i] as f64;
        let o = ours[off + i] as f64;
        sig += r * r;
        let d = r - o;
        err += d * d;
    }
    if err <= 0.0 {
        f64::INFINITY
    } else {
        10.0 * (sig / err).log10()
    }
}

/// Per-channel segmental-SNR bar for the local HE-AACv2/PS conformance gate.
///
/// Measured against the ffmpeg reference (`ps-local.ref.pcm`) at the shared
/// priming offset of 384 per-channel frames, the two channels score **L =
/// 33.55 dB, R = 34.53 dB** — SBR is already near-bit-exact vs ffmpeg here and
/// PS layers table-driven f32 DSP (hybrid filterbank → decorrelation → 2×2
/// complex mixing) on top, so the fidelity stays high. The bar sits **~5.5 dB
/// below** the lower-scoring channel (33.55), the same calibrate-from-measured
/// discipline as the AAC-LC gate (55.3 → 45 dB) and the SBR gate (~104 → 90 dB):
/// comfortably clears a conformant decode yet still drops tens of dB — failing
/// the gate — under any per-channel PS regression (a wrong mixing matrix,
/// dropped decorrelation, or a mono-collapsed channel).
const PS_SNR_BAR: f64 = 28.0;

/// Local-only HE-AACv2/PS conformance: each channel of our decode matches
/// ffmpeg's own stereo decode (`ps-local.ref.pcm`, s16le) within tolerance.
///
/// Skip-if-absent: the fixture is license-restricted and gitignored (never
/// committed / never `include_bytes!`d), so this is a no-op pass on CI and any
/// clean checkout, and a real gate in a local dev checkout.
///
/// One shared priming offset is found on the interleaved pair, then L and R are
/// de-interleaved and scored **independently** at that offset — a one-channel
/// bug (e.g. a mono-collapsed right channel) cannot average out against a good
/// channel, because *both* must clear `PS_SNR_BAR` on their own.
#[test]
fn ps_local_matches_ffmpeg_per_channel() {
    let Some((data, refr)) = ps_local_fixture() else {
        eprintln!("skip: ps-local.* fixture absent (license-restricted, local-only)");
        return;
    };
    let (ours, channels) = decode_all_with_channels(&data); // interleaved i32
    // The mono core is promoted to stereo once the first PS frame's `apply`
    // runs (reconciliation R6).
    assert_eq!(
        channels, 2,
        "PS must promote the decoded stream to 2 channels (R6)"
    );
    // One shared priming offset (a property of the mono core + envelope
    // timeline), found on the interleaved pair.
    let (off, _combined) = best_align_segsnr(&ours, &refr, 2);
    for ch in 0..2 {
        let o = deinterleave(&ours, 2, ch);
        let r = deinterleave(&refr, 2, ch);
        let snr = seg_snr_at(&o, &r, off);
        assert!(
            snr > PS_SNR_BAR,
            "PS ch{ch} segmental SNR vs ffmpeg too low: {snr:.2} dB \
             (bar {PS_SNR_BAR} dB, shared offset {off} frames)"
        );
    }
}

/// Local-only structural stereo gate for the PS decode, independent of the
/// per-sample ffmpeg match above (a backstop if the reference PCM were ever
/// wrong): the output must report 2 channels, be ~2× the SBR rate (per-channel
/// length within a frame of ffmpeg's 2× reference), non-silent, within the i16
/// range, and — the discriminating check — the two channels must **genuinely
/// differ**. `RMS(L−R)/RMS(L)` measured ≈ 0.80 (ffmpeg ref ≈ 0.808); a
/// mono-duplicated / PS-silently-disabled decode would pass every other check
/// but collapse this ratio to ~0. Skip-if-absent (see above).
#[test]
fn ps_local_output_is_stereo_and_channels_differ() {
    let Some((data, refr)) = ps_local_fixture() else {
        eprintln!("skip: ps-local.* fixture absent");
        return;
    };
    let (ours, channels) = decode_all_with_channels(&data);
    assert_eq!(channels, 2, "PS output must be stereo");

    let ours_per_ch = ours.len() / 2;
    let ref_per_ch = refr.len() / 2;
    assert!(
        (ours_per_ch as i64 - ref_per_ch as i64).abs() <= 4096,
        "PS output length {ours_per_ch}/ch not within a frame of ffmpeg ref {ref_per_ch}/ch"
    );

    let rms = (ours.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / ours.len() as f64).sqrt();
    assert!(rms > 1.0, "PS output silent (rms {rms:.2})");
    assert!(
        ours.iter().all(|&s| (-32768..=32767).contains(&s)),
        "PS output outside i16 range"
    );

    // The stereo image must be real, not mono duplicated across both channels.
    let l = deinterleave(&ours, 2, 0);
    let r = deinterleave(&ours, 2, 1);
    let n = l.len().min(r.len());
    let rms_l = (l[..n].iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / n as f64).sqrt();
    let rms_diff = (l[..n]
        .iter()
        .zip(&r[..n])
        .map(|(&a, &b)| ((a - b) as f64).powi(2))
        .sum::<f64>()
        / n as f64)
        .sqrt();
    let width = if rms_l > 0.0 { rms_diff / rms_l } else { 0.0 };
    assert!(
        width > 0.01,
        "PS channels barely differ: RMS(L-R)/RMS(L) = {width:.3} (measured ≈0.80) \
         — a mono-duplicated / PS-silently-disabled regression"
    );
}

// ---------------------------------------------------------------------------
// Vorbis conformance vs ffmpeg (Task 7). Vorbis is LOSSY, but this golden decodes
// the SAME stream ffmpeg's own decoder does, so — unlike a cross-encoder compare —
// a spec-correct IMDCT/window/overlap tracks ffmpeg to near float precision and
// should score very high. A MONO, all-long-block fixture isolates the IMDCT +
// floor + residue (no coupling / type-2 residue — those get their golden later).
// ---------------------------------------------------------------------------

/// Decode a whole Vorbis stream through the public consumer-dispatch path
/// (`VorbisDecoder::new` — Vorbis is not routed through `open`, needing a
/// caller-provided arena) on a generous stack: the decoder carries large fixed
/// DSP scratch (spectrum/residue/overlap) by value (no-alloc), more than the
/// default test-thread stack. Mirrors the crate's big-stack loader pattern.
fn decode_vorbis_all(bytes: &'static [u8]) -> Vec<i32> {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let storage = vec![0u8; 128 * 1024];
            let mut dec =
                VorbisDecoder::new(SliceSource::new(bytes), storage).expect("open mono vorbis");
            let mut out = Vec::new();
            let mut buf = [0i32; 4096];
            loop {
                let n = dec.decode_into(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked")
}

/// Goertzel single-bin power detector: cheap enough for a unit test, and
/// precise enough to tell "440 Hz dominates" from "659.25 Hz dominates"
/// without pulling in an FFT crate. Returns an unnormalized power value —
/// only meaningful compared against another call over a same-length signal.
fn goertzel_power(samples: &[i32], sample_rate: f64, target_hz: f64) -> f64 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (0.5 + (n as f64) * target_hz / sample_rate).floor();
    let w = 2.0 * std::f64::consts::PI * k / n as f64;
    let coeff = 2.0 * w.cos();
    let (mut q1, mut q2) = (0.0f64, 0.0f64);
    for &s in samples {
        let q0 = coeff * q1 - q2 + f64::from(s);
        q2 = q1;
        q1 = q0;
    }
    q1 * q1 + q2 * q2 - q1 * q2 * coeff
}

/// The first end-to-end correct-PCM milestone: our from-scratch Vorbis decode of
/// a mono libvorbis fixture matches ffmpeg's own decode (`tone-vorbis-mono.ref.pcm`,
/// s16le) to high fidelity. Both decode the SAME encoded stream, so the residual
/// is only IMDCT/window float rounding; the bar is set well above a merely
/// "conformant" decode (see the assertion) and a scaling/window/overlap bug would
/// drop it by tens of dB. The first audio packet emits zero samples (it primes the
/// overlap), so `best_align_segsnr` slides over that priming start offset.
///
/// Measured segmental SNR = 107.7 dB at a 0-frame offset (both decoders share the
/// same libvorbis IMDCT absolute scale, so the streams align at the front and the
/// residual is pure f32 rounding); the bar is 60 dB — far above a merely conformant
/// decode, and the `n/2`-too-small IMDCT-normalization bug this milestone fixed
/// scored 0.0 dB, so any regression of that class drops straight through the bar.
#[test]
fn open_decodes_vorbis_mono_matching_ffmpeg() {
    let ogg = include_bytes!("../testdata/tone-vorbis-mono.ogg");
    let refr = load_s16le(include_bytes!("../testdata/tone-vorbis-mono.ref.pcm"));
    let ours = decode_vorbis_all(ogg);
    let (off, snr) = best_align_segsnr(&ours, &refr, 1);
    assert!(
        snr > 60.0,
        "Vorbis mono decode SNR vs ffmpeg too low: {snr:.1} dB (offset {off} frames)"
    );
}

/// Task 8: block switching (short blocks, long↔short transitions, priming).
/// `tone-vorbis-blocks.ogg` is a 440 Hz tone plus a sharp 3 kHz click every
/// 0.25 s, encoded so libvorbis emits short blocks for the transients — a
/// standalone structural check
/// (`formats::vorbis::tests::block_switching_fixture_contains_both_block_sizes`)
/// confirms 81 long + 46 short blocks actually appear (bs0=256, bs1=2048), so
/// this golden exercises long→short, short→long, and sustained short↔short
/// runs, not just the short→long case Task 7's mono fixture happened to hit.
///
/// Measured segmental SNR = 107.9 dB at a 0-frame offset — the same
/// near-bit-exact class as the Task-7 mono golden (both decoders share the
/// libvorbis IMDCT absolute scale). The bar is 60 dB, matching Task 7's: a
/// wrong left/right half-size selection at a transition, a wrong emitted
/// sample count/offset across a size change, or corrupted overlap state would
/// each show up as an audible-click-class error dropping this by tens of dB,
/// not a rounding-level residual.
#[test]
fn open_decodes_vorbis_block_switching_matching_ffmpeg() {
    let ogg = include_bytes!("../testdata/tone-vorbis-blocks.ogg");
    let refr = load_s16le(include_bytes!("../testdata/tone-vorbis-blocks.ref.pcm"));
    let ours = decode_vorbis_all(ogg);
    let (off, snr) = best_align_segsnr(&ours, &refr, 1);
    assert!(
        snr > 60.0,
        "Vorbis block-switching decode SNR vs ffmpeg too low: {snr:.1} dB (offset {off} frames)"
    );
}

/// Task 9: stereo channel coupling (§4.3.5 inverse coupling) + residue type 2
/// (§8.6.5 concat/de-interleave). `tone-vorbis-stereo.ogg` encodes the
/// workspace's stereo `testdata/tone.wav` (A4 = 440 Hz on the LEFT channel,
/// E5 = 659.25 Hz on the RIGHT) — distinct per-channel content specifically so
/// a channel-swap or mispairing bug in the inverse-coupling or type-2
/// de-interleave code (`residue.rs`) is caught, not averaged away by a
/// symmetric metric. The structural test
/// `formats::vorbis::tests::stereo_fixture_setup_declares_coupling_and_residue_type2`
/// confirms libvorbis actually chose channel coupling + residue type 2 for
/// this encode, so this golden genuinely exercises that code path.
///
/// Measured segmental SNR = 107.2 dB at a 0-frame offset — the same
/// near-bit-exact class as the Task 7/8 mono goldens (both decoders share the
/// libvorbis IMDCT absolute scale; coupling/type-2 were already spec-correct
/// from Task 6, so this fixture only had to prove it, not fix it). The bar is
/// 60 dB, matching Task 7/8's: a channel-swap or mispaired coupling step
/// would decorrelate the two channels entirely (comparing 440 Hz content
/// against 659 Hz content position-wise), dropping this by tens of dB, not a
/// rounding-level residual.
///
/// The channel-distinctness checks below measured: RMS(L−R) ≈ 16195 (bar
/// 100), and a ~10¹⁰× dominant/non-dominant Goertzel power ratio on both
/// channels (bar 4×) — L is overwhelmingly 440 Hz, R overwhelmingly 659.25
/// Hz, so the two channels are not swapped.
#[test]
fn open_decodes_vorbis_stereo_coupling_matching_ffmpeg() {
    let ogg = include_bytes!("../testdata/tone-vorbis-stereo.ogg");
    let refr = load_s16le(include_bytes!("../testdata/tone-vorbis-stereo.ref.pcm"));
    let ours = decode_vorbis_all(ogg);
    let (off, snr) = best_align_segsnr(&ours, &refr, 2);
    assert!(
        snr > 60.0,
        "Vorbis stereo coupling decode SNR vs ffmpeg too low: {snr:.1} dB (offset {off} frames)"
    );

    // Channel-distinctness: a coupling channel-swap or mispairing bug can
    // leave the combined interleaved SNR misleadingly high if, say, it
    // symmetrically garbles both channels alike — so directly assert L and R
    // are NOT near-identical, and that each is dominated by its own expected
    // tone (440 Hz left, 659.25 Hz right). This is the check the Task-6
    // review specifically asked this golden to provide.
    let l = deinterleave(&ours, 2, 0);
    let r = deinterleave(&ours, 2, 1);
    let n = l.len().min(r.len());
    assert!(n > 1000, "decoded too few stereo samples ({n})");

    let diff_rms = (l[..n]
        .iter()
        .zip(&r[..n])
        .map(|(&a, &b)| f64::from(a - b).powi(2))
        .sum::<f64>()
        / n as f64)
        .sqrt();
    assert!(
        diff_rms > 100.0,
        "decoded L/R channels are nearly identical (RMS diff {diff_rms:.1}) \
         — suggests a coupling channel-swap collapsed the stereo image"
    );

    let sample_rate = f64::from(vorbis_sample_rate(ogg));
    let l_440 = goertzel_power(&l, sample_rate, 440.0);
    let l_659 = goertzel_power(&l, sample_rate, 659.25);
    let r_440 = goertzel_power(&r, sample_rate, 440.0);
    let r_659 = goertzel_power(&r, sample_rate, 659.25);
    assert!(
        l_440 > l_659 * 4.0,
        "left channel not dominated by its expected 440 Hz tone \
         (power440={l_440:.3e}, power659={l_659:.3e}) — possible coupling channel-swap"
    );
    assert!(
        r_659 > r_440 * 4.0,
        "right channel not dominated by its expected 659.25 Hz tone \
         (power659={r_659:.3e}, power440={r_440:.3e}) — possible coupling channel-swap"
    );
}

/// The Task-3 carry-forward: `total_frames()` seeks to EOF to read the end
/// granule, which historically invalidated the Ogg reader position — corrupting a
/// subsequent decode. This asserts they coexist: querying the duration first must
/// NOT change the decoded output (same SNR + sample count as decoding straight
/// away).
#[test]
fn vorbis_total_frames_then_decode_is_uncorrupted() {
    let ogg: &'static [u8] = include_bytes!("../testdata/tone-vorbis-mono.ogg");
    let refr = load_s16le(include_bytes!("../testdata/tone-vorbis-mono.ref.pcm"));
    let (total, ours) = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let storage = vec![0u8; 128 * 1024];
            let mut dec =
                VorbisDecoder::new(SliceSource::new(ogg), storage).expect("open mono vorbis");
            // Query duration BEFORE decoding a single packet — the destructive path.
            let total = dec.total_frames();
            let mut out = Vec::new();
            let mut buf = [0i32; 4096];
            loop {
                let n = dec.decode_into(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            (total, out)
        })
        .expect("spawn")
        .join()
        .expect("join");

    // The stream declares its total per-channel sample count via the last page's
    // granule (2 s at 44.1 kHz).
    assert_eq!(total, Some(88_200));
    // Decoding after the total_frames() seek must still match ffmpeg (the reader
    // position was restored, not corrupted).
    let (_off, snr) = best_align_segsnr(&ours, &refr, 1);
    assert!(
        snr > 60.0,
        "decode after total_frames() was corrupted: {snr:.1} dB"
    );
}

// ---------------------------------------------------------------------------
// Task 10: granule end-trim + Ogg-granule seeking.
// ---------------------------------------------------------------------------

/// Task 10 (granule end-trim, Task-7 carry-forward): the **last** Ogg page's
/// granule specifies the EXACT total per-channel sample count (spec §4.3.8).
/// Before this fix the final block's `(prev_bs+bs)/4` emission formula could
/// run past that exact count (the Task-7 review's flagged concern — the SNR
/// goldens above tolerate a too-long tail because `best_align_segsnr` only
/// requires the overlap to cover half of `refr`, silently ignoring any excess
/// tail in `ours`). This asserts the FULL decoded length matches the
/// granule-declared total exactly — and, since all three fixtures happen to
/// encode precisely 2.0 s at 44.1 kHz, also matches ffmpeg's own reference PCM
/// length exactly — for all three real fixtures (mono, block-switching,
/// stereo coupling), so the fix is proven across short-block, block-switching,
/// and multi-channel decode paths alike.
#[test]
fn vorbis_full_decode_length_matches_the_granule_exactly() {
    let cases: [(&str, &[u8], &[u8]); 3] = [
        (
            "mono",
            include_bytes!("../testdata/tone-vorbis-mono.ogg"),
            include_bytes!("../testdata/tone-vorbis-mono.ref.pcm"),
        ),
        (
            "block-switching",
            include_bytes!("../testdata/tone-vorbis-blocks.ogg"),
            include_bytes!("../testdata/tone-vorbis-blocks.ref.pcm"),
        ),
        (
            "stereo",
            include_bytes!("../testdata/tone-vorbis-stereo.ogg"),
            include_bytes!("../testdata/tone-vorbis-stereo.ref.pcm"),
        ),
    ];
    for (label, ogg, refpcm) in cases {
        let refr = load_s16le(refpcm);
        let ours = decode_vorbis_all(ogg);
        let (total, channels) = vorbis_total_frames_and_channels(ogg);
        let total = total.unwrap_or_else(|| panic!("{label}: stream declares no length"));
        assert_eq!(
            ours.len(),
            total as usize * usize::from(channels),
            "{label}: decoded length must match the granule-declared total exactly"
        );
        assert_eq!(
            ours.len(),
            refr.len(),
            "{label}: decoded length must match ffmpeg's reference length exactly"
        );
    }
}

/// The declared sample rate of a Vorbis stream, on the same big-stack thread
/// pattern every other real-file Vorbis test in this file uses. Lets a golden
/// read the fixture's actual rate for its frequency-domain checks (e.g. the
/// stereo-coupling golden's Goertzel bins) instead of assuming 44.1 kHz.
fn vorbis_sample_rate(bytes: &'static [u8]) -> u32 {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let storage = vec![0u8; 128 * 1024];
            let dec = VorbisDecoder::new(SliceSource::new(bytes), storage).expect("open vorbis");
            dec.stream_info().sample_rate
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked")
}

/// `(total_frames, channels)` for a Vorbis stream, on the same big-stack
/// thread pattern every other real-file Vorbis test in this file uses.
fn vorbis_total_frames_and_channels(bytes: &'static [u8]) -> (Option<u64>, u16) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let storage = vec![0u8; 128 * 1024];
            let dec = VorbisDecoder::new(SliceSource::new(bytes), storage).expect("open vorbis");
            (dec.total_frames(), dec.stream_info().channels)
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked")
}

/// Open a Vorbis stream, seek to per-channel `sample`, then decode up to
/// `want` per-channel frames worth of interleaved samples. Returns
/// `(landed, pcm, channels)`. Same big-stack thread pattern as
/// [`decode_vorbis_all`] (the decoder is large by value, no-alloc DSP scratch).
fn vorbis_seek_and_decode(
    bytes: &'static [u8],
    seek_sample: u64,
    want_frames: usize,
) -> (u64, Vec<i32>, u16) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let storage = vec![0u8; 128 * 1024];
            let mut dec =
                VorbisDecoder::new(SliceSource::new(bytes), storage).expect("open vorbis");
            let channels = dec.stream_info().channels;
            let landed = dec.seek_to(seek_sample).expect("seek_to");
            let mut out = Vec::new();
            let mut buf = [0i32; 4096];
            while out.len() < want_frames * usize::from(channels) {
                let n = dec.decode_into(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out.truncate(want_frames * usize::from(channels));
            (landed, out, channels)
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked")
}

/// Slide `needle` (interleaved, `channels` per frame) over `haystack` at every
/// per-channel frame offset and return the `(offset_frames, snr_db)` of the
/// best-matching alignment — the same segmental-SNR metric as
/// [`best_align_segsnr`], but searching the WHOLE of `haystack` (not just a
/// bounded priming window), since [`OggPackets::seek_to_granule`] bisection is
/// page-granular (design spec: "land NEAR the target sample" — see
/// `soul_audio_core::formats::vorbis`'s module docs and the `seek_to` doc
/// comment): the true post-seek resume position can be anywhere at or before
/// the landed page's granule, not necessarily at the seek target itself. Only
/// the first `cap` samples of the overlap are scored per candidate offset (a
/// severe corruption or misalignment shows up well within a few hundred
/// samples), keeping an exhaustive whole-file search cheap.
fn best_full_match(haystack: &[i32], needle: &[i32], channels: usize, cap: usize) -> (usize, f64) {
    let mut best = (0usize, f64::NEG_INFINITY);
    let frames = haystack.len() / channels;
    let needle_frames = needle.len() / channels;
    for off_frames in 0..=frames.saturating_sub(needle_frames.min(frames)) {
        let off = off_frames * channels;
        let n = needle.len().min(haystack.len() - off).min(cap * channels);
        if n == 0 {
            continue;
        }
        let mut sig = 0f64;
        let mut err = 0f64;
        for i in 0..n {
            let h = haystack[off + i] as f64;
            let d = needle[i] as f64;
            sig += h * h;
            let e = h - d;
            err += e * e;
        }
        let snr = if err <= 0.0 {
            f64::INFINITY
        } else {
            10.0 * (sig / err).log10()
        };
        if snr > best.1 {
            best = (off_frames, snr);
        }
    }
    best
}

/// Task 10 seek round-trip: seeking mid-stream and decoding must reproduce
/// audio a full from-start decode ALSO contains, somewhere — proving the
/// re-primed overlap state reconstructs genuinely correct, correctly-overlapped
/// PCM (not silence or garbage) — while the reported landed position stays a
/// sane, bounded estimate (never before the target, never past the stream's
/// end).
///
/// `tone-vorbis-mono.ogg` has only 4 Ogg pages (2 header + 2 audio, each audio
/// page bundling dozens of packets) — Ogg-granule bisection is fundamentally
/// page-granular (design spec: "land NEAR the target sample"), and this
/// decoder's re-prime discards only the ONE packet immediately at the landed
/// page's start (not the whole page — see [`VorbisDecoder::seek_to`]'s doc
/// comment), so for a page bundling many packets the audio that actually
/// resumes can start well BEFORE the landed granule, even though `landed`
/// itself is always `>= target`. That tradeoff is deliberate: the alternative
/// (discarding an entire multi-packet page to make the reported position
/// exact) would emit NO audio at all for any seek landing on the stream's
/// final page — worse for a real player than an imprecise-but-honest position
/// alongside real, correctly-decoded trailing audio.
///
/// Measured: seeking to 1.5 s (66 150) lands on the file's last page
/// (`landed = 88 200`, the exact end — this fixture's last two audio pages
/// bracket both 44 608..66 150 and 66 150..88 200 onto the same final page),
/// while the actually-resumed audio best-matches the full decode at
/// **segmental SNR = ∞ (bit-exact)** starting at full-decode sample ≈ 45 632 —
/// close to the landed page's start (44 608), confirming the re-primed audio
/// is correct, not merely plausible, even though `landed` overstates how far
/// into the file playback actually resumes for this coarse fixture.
#[test]
fn vorbis_seek_round_trip_matches_the_full_decode_somewhere() {
    let ogg: &'static [u8] = include_bytes!("../testdata/tone-vorbis-mono.ogg");
    let full = decode_vorbis_all(ogg);

    let target = 66_150u64; // 1.5 s into the 2.0 s mono fixture
    let want_frames = 4096usize;
    let (landed, seek_pcm, channels) = vorbis_seek_and_decode(ogg, target, want_frames);
    assert_eq!(channels, 1);

    // The landed position must be a real, bounded estimate: never before the
    // target (seek_to_granule's own "first page whose granule >= target"
    // contract), and never past the stream's declared end.
    assert!(
        landed >= target,
        "landed {landed} must be >= target {target}"
    );
    let (total, _) = vorbis_total_frames_and_channels(ogg);
    assert!(
        landed <= total.unwrap(),
        "landed {landed} must not exceed the stream's total {total:?}"
    );

    // The post-seek audio must match the full decode's content SOMEWHERE —
    // proving re-priming reconstructs correct PCM. Skip roughly the first
    // block's worth of samples (the documented "one block of overlap warm-up"
    // — `synthesize`'s first post-reprime emission still depends on exactly
    // where the discarded packet's context began) before scoring.
    let warmup = 2048usize; // > one long block's per-channel emission
    let scored = &seek_pcm[warmup.min(seek_pcm.len())..];
    let (off, snr) = best_full_match(&full, scored, usize::from(channels), 4096);
    assert!(
        snr > 60.0,
        "seek(66150) decode doesn't match the full decode anywhere well: \
         best {snr:.1} dB at offset {off} frames (landed={landed})"
    );
}

/// Seeking to 0 must reproduce a fresh decode of the whole stream exactly —
/// the special-cased "back to start" path ([`VorbisDecoder::seek_to`]'s
/// `target == 0` branch) rewinds past the three header packets and resets the
/// overlap state, so there is no approximation here (unlike a nonzero seek).
#[test]
fn vorbis_seek_to_zero_matches_a_fresh_decode() {
    let ogg: &'static [u8] = include_bytes!("../testdata/tone-vorbis-mono.ogg");
    let fresh = decode_vorbis_all(ogg);

    let (landed, seek_pcm, _channels) = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let storage = vec![0u8; 128 * 1024];
            let mut dec =
                VorbisDecoder::new(SliceSource::new(ogg), storage).expect("open mono vorbis");
            // Decode partway first, so seek_to(0) has real state to undo.
            let mut probe = [0i32; 4096];
            let _ = dec.decode_into(&mut probe).unwrap();
            let landed = dec.seek_to(0).expect("seek_to(0)");
            let mut out = Vec::new();
            let mut buf = [0i32; 4096];
            loop {
                let n = dec.decode_into(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            (landed, out, dec.stream_info().channels)
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked");

    assert_eq!(landed, 0);
    assert_eq!(
        seek_pcm, fresh,
        "seek_to(0) must reproduce a fresh decode bit-for-bit"
    );
}

/// Seeking beyond the stream's end must clamp to the declared total (never
/// panic, never error) — [`OggPackets::seek_to_granule`]'s own "closest
/// available position" contract, reused as-is by [`VorbisDecoder::seek_to`].
#[test]
fn vorbis_seek_beyond_end_clamps_without_panicking() {
    let ogg: &'static [u8] = include_bytes!("../testdata/tone-vorbis-mono.ogg");
    let (total, _) = vorbis_total_frames_and_channels(ogg);
    let total = total.expect("mono fixture declares a length");

    let (landed, tail, _channels) = vorbis_seek_and_decode(ogg, total * 100, 4096);
    assert_eq!(landed, total);
    // Whatever trails after landing at/near the end must stay within the
    // exact declared total (Task 10's granule end-trim applies here too).
    assert!(tail.len() as u64 <= total);
}

/// Task 10 review fix: seeking to a non-trivial mid-file target, then
/// decoding ALL the way to true EOF, must land the decode EXACTLY on the
/// granule-declared total — not merely "close enough" (the SNR-based
/// `vorbis_seek_round_trip_matches_the_full_decode_somewhere` above only
/// proves the resumed audio is CORRECT content, not that it stops at the
/// right sample). `decode_into`'s end-trim compares an ABSOLUTE ceiling
/// (`OggPackets::last_page_granule`, file-wide) against `samples_emitted`;
/// `seek_to` used to always reset that counter to `0`, undercounting the
/// true absolute position by the landed page's ENTIRE sample span (not "at
/// most one block" as a since-corrected doc comment claimed) whenever a seek
/// landed past the very first page. With the ceiling still absolute but the
/// counter reset to 0, `ceiling - samples_emitted` stayed far too large for
/// the whole rest of the decode, so the end-trim clamp never fired and
/// decode ran to the raw end of the last page's data — reproducing the
/// original Task-7 final-block overshoot, just reached by seeking instead of
/// a from-start decode.
///
/// The mono fixture's target (66 150, 1.5 s into the 2.0 s file) is chosen
/// specifically because it lands [`OggPackets::seek_to_granule`] on the
/// file's OWN LAST page (see the round-trip test above: `landed = 88 200`,
/// the exact end) — exactly the broken path, where the end-trim ceiling and
/// the seek-landed page are the SAME page.
#[test]
fn vorbis_seek_then_decode_to_eof_length_matches_the_granule_exactly() {
    let ogg: &'static [u8] = include_bytes!("../testdata/tone-vorbis-mono.ogg");
    let full = decode_vorbis_all(ogg);
    let (total, channels) = vorbis_total_frames_and_channels(ogg);
    let total = total.expect("mono fixture declares a length");
    assert_eq!(channels, 1);

    let target = 66_150u64; // 1.5 s into the 2.0 s mono fixture — lands on the last page.
    // `want_frames` generously covers "the rest of the stream": the loop
    // inside `vorbis_seek_and_decode` still stops the instant `decode_into`
    // reports a clean end of stream, so this decodes to true EOF exactly,
    // never an artificially truncated prefix.
    let (landed, tail, ch) =
        vorbis_seek_and_decode(ogg, target, total as usize + usize::from(channels) * 4096);
    assert_eq!(ch, 1);
    assert_eq!(
        landed, total,
        "this target must land exactly on the stream's own last page"
    );

    // Locate where this post-seek tail begins within a full from-start
    // decode (same alignment search the round-trip test above uses — Ogg
    // granule-position seeking is page-granular in WHERE real audio resumes,
    // spec/design: "land NEAR the target sample"). As in that test, skip
    // roughly the first block's worth of samples before scoring: the very
    // first post-reprime block still carries window-edge warm-up artifacts
    // that would otherwise drag down the alignment search — but this skip
    // only trims the FRONT of `tail`, so its END (what this test actually
    // checks) is untouched.
    let warmup = 2048usize.min(tail.len() / usize::from(ch));
    let scored = &tail[warmup * usize::from(ch)..];
    let (off, snr) = best_full_match(&full, scored, usize::from(ch), 4096);
    assert!(
        snr > 60.0,
        "seek({target}) + decode-to-EOF tail doesn't align with the full decode: \
         best {snr:.1} dB at offset {off} frames"
    );
    // Assert the tail's END lines up with the granule-declared total EXACTLY
    // — the discriminating check the round-trip test's SNR-only comparison
    // does not make: it catches the tail running EITHER short OR long of
    // `total`, in particular the review's overshoot-past-`total` bug.
    let scored_frames = scored.len() / usize::from(ch);
    assert_eq!(
        off + scored_frames,
        total as usize,
        "seek({target}) + decode-to-EOF must end EXACTLY at the granule-declared total \
         (aligned at offset {off}, {scored_frames} scored frames) — overshooting past it \
         reproduces the Task-7 final-block bug via seeking instead of a from-start decode"
    );
}

// ---------------------------------------------------------------------------
// Opus CELT conformance vs ffmpeg (Task 8 — first correct PCM milestone). Opus
// is LOSSY, but this golden decodes the SAME stream ffmpeg's own libopus-based
// decoder does, so a spec-correct CELT synthesis (denormalise → anti-collapse →
// inverse MDCT → overlap-add → post-filter → de-emphasis) tracks ffmpeg to near
// float precision and should score very high. `tone.opus` is a CELT-only
// fullband **stereo** encode (A4=440 Hz left, E5=659.25 Hz right); ffmpeg
// applies pre-skip/output-gain, and so does our decoder, so the streams align
// tightly. A scale/normalisation/overlap bug (the classic MDCT `n/2` class the
// Vorbis milestone hit) drops this by tens of dB rather than a rounding-level
// residual.
//
// Measured segmental SNR ≈ 84 dB at a 0-frame offset (our pre-skip trims the
// same 312 priming samples ffmpeg does, so the streams align at the front); the
// exact figure varies a little by platform / ffmpeg build. The bar is 60 dB —
// far above a merely conformant CELT decode. The residual
// (not near-∞ like the lossless goldens) is the two audio-quality-only
// approximations the codec permits post-range-coder: the `celt_exp2` denormalise
// gain (libopus's `FLOAT_APPROX` cubic) and the Q15 spreading-rotation cosine in
// the PVQ — neither is a bug, and any real MDCT/overlap/scale/pre-skip regression
// drops straight through the bar.
#[test]
fn open_decodes_opus_celt_stereo_matching_ffmpeg() {
    let opus = include_bytes!("../../../testdata/tone.opus");
    let refr = load_s16le(include_bytes!("../testdata/tone-opus.ref.pcm"));
    let dec = open(SliceSource::new(opus)).expect("probe + construct Opus");
    assert_eq!(dec.stream_info().sample_rate, 48_000);
    assert_eq!(dec.stream_info().channels, 2);
    let ours = decode_all(opus);
    let (off, snr) = best_align_segsnr(&ours, &refr, 2);
    assert!(
        snr > 60.0,
        "Opus CELT decode SNR vs ffmpeg too low: {snr:.1} dB (offset {off} frames)"
    );
}

// ---------------------------------------------------------------------------
// Opus SILK conformance vs ffmpeg (Task 11 — the SILK milestone: LTP+LPC
// synthesis → stereo unmix (mono here) → resampler → 48 kHz PCM). Unlike the
// CELT golden, ffmpeg's native Opus decoder resamples SILK's internal rate
// (16 kHz WB here) up to 48 kHz with *libswresample*, NOT libopus's own IIR/FIR
// resampler that this decoder faithfully transcribes — so (a) the achievable
// ceiling sits far below CELT's ~87 dB (the two resamplers' pass/stop-band
// shapes and group delays differ), and (b) our libopus-matching output leads
// ffmpeg's by a couple of samples, a fixed delay-convention offset the alignment
// search below absorbs.
//
// Measured segmental SNR = 33.75 dB at the aligned offset (per-0.1 s block:
// 39–42 dB across the file, one transient block dipping to ~23 dB — the
// resampler phase difference peaks on fast transients). The SILK *synthesis*
// itself is exact: its inputs (`exc_Q14`, gains, LPC/LTP coefs) are the
// byte-for-byte libopus oracle values the front-end unit tests lock, and
// `silk_decode_core` is a line-for-line transcription — so the whole residual
// vs ffmpeg is the resampler shape, not a decode error. The bar is 30 dB (a
// synthesis/LCG/gain/state or resampler-delay-line bug drops this into single
// digits). Bit-exact conformance is validated later by Task 14's RFC 8251
// vectors.
#[test]
fn open_decodes_silk_mono_matching_ffmpeg() {
    let opus = include_bytes!("../testdata/voice-silk-mono.opus");
    let refr = load_s16le(include_bytes!("../testdata/voice-silk-mono.ref.pcm"));
    let dec = open(SliceSource::new(opus)).expect("probe + construct Opus");
    assert_eq!(dec.stream_info().sample_rate, 48_000);
    assert_eq!(dec.stream_info().channels, 1);
    let ours = decode_all(opus);

    // Best segmental SNR over a small symmetric priming-offset window: our
    // libopus-matching resampler leads ffmpeg's swr by a couple of samples, so
    // — unlike the forward-only `best_align_segsnr` — this also lets the
    // reference slide forward.
    let mut best = (0i64, f64::NEG_INFINITY);
    for off in -64i64..=64 {
        let (o, r): (&[i32], &[i32]) = if off >= 0 {
            (&ours[(off as usize).min(ours.len())..], &refr)
        } else {
            (&ours, &refr[(-off as usize).min(refr.len())..])
        };
        let n = o.len().min(r.len());
        if n < refr.len() / 2 {
            continue;
        }
        let snr = seg_snr_at(o, &r[..n], 0);
        if snr > best.1 {
            best = (off, snr);
        }
    }
    let (off, snr) = best;
    assert!(
        snr > 30.0,
        "SILK mono decode SNR vs ffmpeg too low: {snr:.2} dB (offset {off} samples)"
    );
}

// ---------------------------------------------------------------------------
// Opus HYBRID conformance vs ffmpeg (Task 12 — SILK low band + CELT high band,
// summed). `hybrid-mono.opus` is a mono, all-hybrid (TOC config 15 = Hybrid FB
// 20 ms, every packet) libopus encode: SILK decodes 0–8 kHz (WB, 16 kHz
// internal → 48 kHz resample), CELT decodes bands 17..21 (8 kHz+) from the SAME
// range coder positioned after SILK, and the two 48 kHz signals are summed.
//
// Like the SILK golden, the hybrid *low* band is resampled by SILK's own IIR/FIR
// resampler, which our decoder transcribes exactly but ffmpeg's NATIVE Opus
// decoder replaces with libswresample — so the achievable SNR vs ffmpeg is
// capped well below CELT's ~87 dB (the two resamplers' group delays differ, and
// the tonal low-band content is phase-sensitive to that sub-sample delay). A
// correct decode scores ~20 dB here (native-ffmpeg vs libopus itself only
// reaches ~24 dB on this stream, the true ceiling); a start-band, delay, or
// SILK+CELT-sum bug drops it to single digits or negative. The decisive
// bit-exactness proof is sample-by-sample vs libopus (see the Task-12 report).
#[test]
fn open_decodes_opus_hybrid_mono_matching_ffmpeg() {
    let opus = include_bytes!("../testdata/hybrid-mono.opus");
    let refr = load_s16le(include_bytes!("../testdata/hybrid-mono.ref.pcm"));
    let dec = open(SliceSource::new(opus)).expect("probe + construct Opus");
    assert_eq!(dec.stream_info().sample_rate, 48_000);
    assert_eq!(dec.stream_info().channels, 1);
    let ours = decode_all(opus);

    // Best segmental SNR over a small symmetric priming-offset window: our
    // libopus-matching resampler leads/lags ffmpeg's swr by a couple of samples.
    let mut best = (0i64, f64::NEG_INFINITY);
    for off in -64i64..=64 {
        let (o, r): (&[i32], &[i32]) = if off >= 0 {
            (&ours[(off as usize).min(ours.len())..], &refr)
        } else {
            (&ours, &refr[(-off as usize).min(refr.len())..])
        };
        let n = o.len().min(r.len());
        if n < refr.len() / 2 {
            continue;
        }
        let snr = seg_snr_at(o, &r[..n], 0);
        if snr > best.1 {
            best = (off, snr);
        }
    }
    let (off, snr) = best;
    assert!(
        snr > 15.0,
        "Opus hybrid mono decode SNR vs ffmpeg too low: {snr:.2} dB (offset {off} samples)"
    );
}

// ---------------------------------------------------------------------------
// Opus STEREO SILK + STEREO HYBRID conformance vs ffmpeg (Task 13b). These
// decode genuine stereo Opus (nChannelsInternal == 2): the SILK mid/side
// prediction + `silk_stereo_MS_to_LR` unmix drive two independent channel
// states → L/R, then each is resampled to 48 kHz. Like the mono SILK/hybrid
// goldens the ffmpeg-native reference is resampler-limited (ffmpeg resamples
// SILK's internal rate with libswresample, we transcribe libopus's own IIR/FIR),
// so the SNR ceiling vs ffmpeg is well below CELT's ~87 dB. The DECISIVE
// bit-exactness proof is sample-by-sample vs the libopus v1.5.2 oracle: stereo
// SILK is **bit-exact on both channels** (∞ dB, 100 %), stereo hybrid matches to
// the CELT float ceiling (90.9 / 105.9 dB, max abs diff 1 — the SILK low band is
// bit-exact, the ±1 is CELT's permitted `celt_exp2`/PVQ float approximation).
// See the Task-13b report.
//
// Each golden scores L and R **independently** at a shared priming offset, so a
// channel-swap / mono-collapse / mispaired-unmix bug (which decorrelates one
// channel) cannot average out against a good channel — both must clear the bar.
// ---------------------------------------------------------------------------

/// Stereo SILK (`silk-stereo.opus`, TOC config 9 = SILK/WB/20 ms, stereo bit
/// set on every packet): L is a 300 Hz tone, R a 500 Hz tone (distinct AM), so a
/// stereo-image collapse or channel swap is caught by the per-channel SNR + the
/// Goertzel tone-dominance checks below.
///
/// Measured (vs the ffmpeg-native reference): L = 44.0 dB, R = 40.9 dB at a
/// 1-frame priming offset — both well above the 30 dB bar (same
/// resampler-ceiling calibration as the 33.75 dB mono SILK golden). Bit-exact
/// vs the libopus oracle (see the module comment).
#[test]
fn open_decodes_silk_stereo_matching_ffmpeg() {
    let opus = include_bytes!("../testdata/silk-stereo.opus");
    let refr = load_s16le(include_bytes!("../testdata/silk-stereo.ref.pcm"));
    let dec = open(SliceSource::new(opus)).expect("probe + construct Opus");
    assert_eq!(dec.stream_info().sample_rate, 48_000);
    assert_eq!(dec.stream_info().channels, 2);
    let ours = decode_all(opus);

    // Shared priming offset (our libopus-matching resampler leads ffmpeg's swr
    // by ~1 sample), then per-channel SNR at that offset.
    let (off, _combined) = best_align_segsnr(&ours, &refr, 2);
    for ch in 0..2 {
        let o = deinterleave(&ours, 2, ch);
        let r = deinterleave(&refr, 2, ch);
        let snr = seg_snr_at(&o, &r, off);
        assert!(
            snr > 30.0,
            "SILK stereo ch{ch} SNR vs ffmpeg too low: {snr:.2} dB (shared offset {off} frames)"
        );
    }

    // Channel distinctness: L must be dominated by 300 Hz, R by 500 Hz, and the
    // two channels must genuinely differ (a swap/collapse fails these).
    let l = deinterleave(&ours, 2, 0);
    let r = deinterleave(&ours, 2, 1);
    let n = l.len().min(r.len());
    let diff_rms = (l[..n]
        .iter()
        .zip(&r[..n])
        .map(|(&a, &b)| f64::from(a - b).powi(2))
        .sum::<f64>()
        / n as f64)
        .sqrt();
    assert!(
        diff_rms > 100.0,
        "SILK stereo L/R nearly identical (RMS diff {diff_rms:.1}) — stereo image collapsed"
    );
    let l_300 = goertzel_power(&l, 48_000.0, 300.0);
    let l_500 = goertzel_power(&l, 48_000.0, 500.0);
    let r_300 = goertzel_power(&r, 48_000.0, 300.0);
    let r_500 = goertzel_power(&r, 48_000.0, 500.0);
    assert!(
        l_300 > l_500,
        "left channel not dominated by its 300 Hz tone (p300={l_300:.2e} p500={l_500:.2e})"
    );
    assert!(
        r_500 > r_300,
        "right channel not dominated by its 500 Hz tone (p500={r_500:.2e} p300={r_300:.2e})"
    );
}

/// Stereo HYBRID (`hybrid-stereo.opus`, TOC config 15 = Hybrid/FB/20 ms, stereo
/// on every packet): stereo SILK low band + stereo CELT high band, summed per
/// channel. Measured (vs ffmpeg-native): L = 25.1 dB, R = 40.9 dB at a 1-frame
/// offset — both above the 15 dB bar (same low resampler-ceiling as the 24 dB
/// mono hybrid golden; the tonal low band is phase-sensitive to the sub-sample
/// resampler-delay difference). Near-bit-exact vs the libopus oracle
/// (90.9 / 105.9 dB, max abs diff 1 — see the module comment).
#[test]
fn open_decodes_opus_hybrid_stereo_matching_ffmpeg() {
    let opus = include_bytes!("../testdata/hybrid-stereo.opus");
    let refr = load_s16le(include_bytes!("../testdata/hybrid-stereo.ref.pcm"));
    let dec = open(SliceSource::new(opus)).expect("probe + construct Opus");
    assert_eq!(dec.stream_info().sample_rate, 48_000);
    assert_eq!(dec.stream_info().channels, 2);
    let ours = decode_all(opus);

    let (off, _combined) = best_align_segsnr(&ours, &refr, 2);
    for ch in 0..2 {
        let o = deinterleave(&ours, 2, ch);
        let r = deinterleave(&refr, 2, ch);
        let snr = seg_snr_at(&o, &r, off);
        assert!(
            snr > 15.0,
            "hybrid stereo ch{ch} SNR vs ffmpeg too low: {snr:.2} dB (shared offset {off} frames)"
        );
    }

    // The two channels must genuinely differ (mid/side unmix produced a real
    // stereo image, not a mono duplicate).
    let l = deinterleave(&ours, 2, 0);
    let r = deinterleave(&ours, 2, 1);
    let n = l.len().min(r.len());
    let diff_rms = (l[..n]
        .iter()
        .zip(&r[..n])
        .map(|(&a, &b)| f64::from(a - b).powi(2))
        .sum::<f64>()
        / n as f64)
        .sqrt();
    assert!(
        diff_rms > 100.0,
        "hybrid stereo L/R nearly identical (RMS diff {diff_rms:.1}) — stereo image collapsed"
    );
}

// ---------------------------------------------------------------------------
// Opus ADAPTIVE MODE-SWITCHING conformance vs libopus (issue #37). Unlike the
// single-mode goldens above (all-CELT / all-SILK / all-Hybrid), `mode-switch-
// mono.opus` is a real adaptive libopus encode whose TOC bytes span **all three
// modes** with FIVE mid-stream transitions covering every direction —
// Hybrid→SILK, SILK→CELT, CELT→SILK, SILK→Hybrid, Hybrid→CELT (see the mode-span
// assertion below). Those transitions exercise the CELT redundancy / mode-
// transition logic (the redundant 5 ms CELT-only frame, `smooth_fade`
// cross-fades, and libopus's `OPUS_RESET_STATE` on a mode change) that the
// raw-packet conformance path always had but the Ogg `open()` path originally
// skipped. Before issue #37 the transition frames diverged badly from libopus
// (thousands of counts per switch: the SILK↔CELT boundaries by 5000-12000, the
// Hybrid boundaries by 7000-8000); after unifying both paths onto the shared
// `OpusFrameDecoder` core, `open()` decodes each transition with the identical
// RFC-8251-validated logic — Hybrid→SILK / SILK→CELT / SILK→Hybrid / Hybrid→CELT
// become bit-exact-to-≤40 vs libopus, and CELT→SILK lands at the compounded-CELT
// float-approximation ceiling (its redundant-CELT + cross-fade decode runs two
// CELT passes whose approximations add).
//
// The reference (`mode-switch-mono.ref.pcm`) is ffmpeg's **libopus** decode
// (`ffmpeg -c:a libopus -i mode-switch-mono.opus -f s16le`), i.e. the reference
// decoder itself — not ffmpeg's *native* Opus decoder — so it uses libopus's own
// SILK resampler (the one this decoder transcribes) and the achievable SNR is
// the CELT float ceiling across every mode, not the resampler-limited ~20-30 dB
// of the native-ffmpeg SILK/hybrid goldens. That makes this a tight,
// discriminating gate on the transitions specifically: the pre-#37 `fill_packet`
// scored 19.1 dB here (whole-signal SNR dominated by the diverging transition
// frames), while the unified core scores 41.2 dB — bar 30 dB. (The whole-signal
// figure is held to ~41 dB, not the ~80 dB of the per-frame masked metric, by
// the single CELT→SILK boundary frame at the compounded-CELT ceiling; a
// regression in ANY transition drops it straight back through 30 dB.)
// ---------------------------------------------------------------------------

/// Count the audio-packet modes of an Ogg-Opus stream by parsing each packet's
/// TOC byte (RFC 6716 §3.1) — used to prove the fixture genuinely switches mode.
/// Returns the per-mode packet counts `(silk, hybrid, celt)`.
fn opus_mode_histogram(bytes: &[u8]) -> (usize, usize, usize) {
    let mut ogg = OggPackets::new(SliceSource::new(bytes)).expect("ogg");
    let mut buf = vec![0u8; 1 << 16];
    // Skip the two Opus header packets (OpusHead + OpusTags).
    ogg.read_packet_prefix(&mut buf).expect("head");
    ogg.read_packet_prefix(&mut buf).expect("tags");
    let (mut s, mut h, mut c) = (0, 0, 0);
    while let Ok(n) = ogg.next_packet(&mut buf) {
        if n == 0 {
            break;
        }
        match toc::parse_toc(buf[0]).mode {
            Mode::SilkOnly => s += 1,
            Mode::Hybrid => h += 1,
            Mode::CeltOnly => c += 1,
        }
    }
    (s, h, c)
}

/// `mode-switch-mono.opus` — a genuinely adaptive libopus encode spanning all
/// three Opus modes with five mode transitions — decoded via `open()` →
/// `AnyDecoder::Opus` matches the libopus reference decode, proving the Ogg path
/// now applies the redundancy / mode-transition logic across every mode switch
/// (the shared `OpusFrameDecoder` core it now embeds).
///
/// Measured whole-signal SNR vs the libopus reference = 41.2 dB (bar 30 dB); the
/// pre-#37 Ogg path scored 19.1 dB here because it skipped the redundant CELT
/// frame + cross-fades and never reset CELT on a mode change.
#[test]
fn open_decodes_mode_switching_opus_matching_libopus() {
    let opus = include_bytes!("../testdata/mode-switch-mono.opus");
    let refr = load_s16le(include_bytes!("../testdata/mode-switch-mono.ref.pcm"));

    // The fixture must genuinely switch mode — it spans all THREE Opus modes
    // (else it would not exercise the full transition logic across every
    // direction). A degenerate single-mode fixture would make this golden a
    // no-op regardless of the transition code.
    let (s, h, c) = opus_mode_histogram(opus);
    assert!(
        s > 0 && h > 0 && c > 0,
        "fixture must span all three Opus modes to test transitions (SilkOnly={s} Hybrid={h} CeltOnly={c})"
    );

    let dec = open(SliceSource::new(opus)).expect("probe + construct Opus");
    assert_eq!(dec.stream_info().sample_rate, 48_000);
    assert_eq!(dec.stream_info().channels, 1);
    let ours = decode_all(opus);
    let (off, snr) = best_align_segsnr(&ours, &refr, 1);
    assert!(
        snr > 30.0,
        "mode-switching Opus decode SNR vs libopus too low: {snr:.2} dB (offset {off} frames) \
         — a transition frame likely diverged (redundancy / OPUS_RESET_STATE)"
    );
}

// ---------------------------------------------------------------------------
// Opus integration (Task 13): pre-skip + granule end-trim, seek round-trip,
// seek-to-0, and beyond-end clamp. `tone.opus` is the CELT-only fullband stereo
// fixture (A4 left / E5 right, pre-skip 312), decoded through the public
// `open()` → `AnyDecoder::Opus` path.
// ---------------------------------------------------------------------------

/// `(total_frames, channels)` for an Opus stream, on the big-stack thread every
/// other real-file decode here uses (the decoder carries large fixed CELT/SILK
/// state by value).
fn opus_total_frames_and_channels(bytes: &'static [u8]) -> (Option<u64>, u16) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let dec = open(SliceSource::new(bytes)).expect("open opus");
            (dec.total_frames(), dec.stream_info().channels)
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked")
}

/// Open an Opus stream, seek to per-channel output `seek_sample`, then decode up
/// to `want_frames` per-channel frames worth of interleaved samples. Returns
/// `(landed, pcm, channels)`.
fn opus_seek_and_decode(
    bytes: &'static [u8],
    seek_sample: u64,
    want_frames: usize,
) -> (u64, Vec<i32>, u16) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut dec = open(SliceSource::new(bytes)).expect("open opus");
            let channels = dec.stream_info().channels;
            let landed = dec.seek_to(seek_sample).expect("seek_to");
            let mut out = Vec::new();
            let mut buf = [0i32; 4096];
            while out.len() < want_frames * usize::from(channels) {
                let n = dec.decode_into(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out.truncate(want_frames * usize::from(channels));
            (landed, out, channels)
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked")
}

/// Task 13 (RFC 7845 §4.2 pre-skip + granule end-trim): the total decoded
/// length must equal `(end_granule − pre_skip) · channels` **exactly** — the
/// stream-global pre-skip discards the first 312 priming samples, and the final
/// Ogg page's granule trims the last packet's tail. Because `tone.opus` encodes
/// exactly 2.0 s at 48 kHz, that also matches ffmpeg's own reference PCM length
/// (`tone-opus.ref.pcm`, s16le) to the sample — a stricter check than the SNR
/// golden, which tolerates a too-long tail.
#[test]
fn opus_full_decode_length_matches_end_granule_minus_pre_skip() {
    let opus: &'static [u8] = include_bytes!("../../../testdata/tone.opus");
    let refr = load_s16le(include_bytes!("../testdata/tone-opus.ref.pcm"));
    let ours = decode_all(opus);
    let (total, channels) = opus_total_frames_and_channels(opus);
    let total = total.expect("tone.opus declares a length");
    assert_eq!(
        ours.len(),
        total as usize * usize::from(channels),
        "decoded length must equal (end_granule − pre_skip) · channels exactly"
    );
    assert_eq!(
        ours.len(),
        refr.len(),
        "decoded length must match ffmpeg's reference length exactly"
    );
}

/// Task 13 seek round-trip: seeking mid-stream and decoding must reproduce the
/// same samples a full linear decode produces from that point. Opus seeking is
/// **sample-accurate** — the decoder lands on an Ogg page ≥80 ms before the
/// target, decode-and-discards the run-in (reconverging the CELT overlap +
/// energy-predictor state), and begins emitting exactly at the requested output
/// sample — so `landed == target` and the re-primed audio aligns at offset 0
/// with the full decode. Measured **bit-exact (∞ dB)** for this CELT-only
/// fixture: the 80 ms pre-roll fully reconverges the MDCT overlap and coarse
/// energy predictor, so the resumed decode is identical to the full one (the
/// short warm-up skip is belt-and-braces, not needed here). The bar is 60 dB — a
/// re-prime/state-reset bug (stale overlap, an un-reconverged energy predictor,
/// or a wrong granule baseline) drops it by tens of dB.
#[test]
fn opus_seek_round_trip_matches_the_full_decode() {
    let opus: &'static [u8] = include_bytes!("../../../testdata/tone.opus");
    let full = decode_all(opus);
    let (total, channels) = opus_total_frames_and_channels(opus);
    let total = total.expect("tone.opus declares a length");
    let channels = usize::from(channels);

    let target = total / 2; // 1.0 s into the 2.0 s stream
    let want = 8192usize;
    let (landed, seek_pcm, ch) = opus_seek_and_decode(opus, target, want);
    assert_eq!(usize::from(ch), channels);
    assert_eq!(
        landed, target,
        "Opus seek is sample-accurate — it must land exactly on the requested output sample"
    );

    // Align the re-primed tail against the full decode starting at `target`.
    let base = target as usize * channels;
    let aligned = &full[base.min(full.len())..];
    let n = seek_pcm.len().min(aligned.len());
    assert!(n > 4096, "too few post-seek samples to score ({n})");
    // Skip ~10 ms of warm-up so the tail of the energy-predictor error decay
    // doesn't drag the score; the rest must match the full decode closely.
    let warm = 480 * channels;
    let snr = seg_snr_at(&seek_pcm[warm..n], &aligned[warm..n], 0);
    assert!(
        snr > 60.0,
        "seek({target}) decode doesn't match the full decode: {snr:.1} dB (landed={landed})"
    );
}

/// Seeking to 0 must reproduce a fresh decode of the whole stream exactly — the
/// special-cased "back to start" path rewinds past the two Opus header packets
/// and fully resets the CELT/SILK state, so there is no re-prime approximation
/// (unlike a nonzero seek).
#[test]
fn opus_seek_to_zero_matches_a_fresh_decode() {
    let opus: &'static [u8] = include_bytes!("../../../testdata/tone.opus");
    let fresh = decode_all(opus);

    let (landed, seek_pcm) = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut dec = open(SliceSource::new(opus)).expect("open opus");
            // Decode partway first, so seek_to(0) has real state to undo.
            let mut probe = [0i32; 4096];
            let _ = dec.decode_into(&mut probe).unwrap();
            let landed = dec.seek_to(0).expect("seek_to(0)");
            let mut out = Vec::new();
            let mut buf = [0i32; 4096];
            loop {
                let n = dec.decode_into(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            (landed, out)
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked");

    assert_eq!(landed, 0);
    assert_eq!(
        seek_pcm, fresh,
        "seek_to(0) must reproduce a fresh decode bit-for-bit"
    );
}

/// Seeking beyond the stream's declared end must clamp to the total (never
/// panic, never error), landing on the final output sample — from which there
/// is nothing left to decode.
#[test]
fn opus_seek_beyond_end_clamps_without_panicking() {
    let opus: &'static [u8] = include_bytes!("../../../testdata/tone.opus");
    let (total, _) = opus_total_frames_and_channels(opus);
    let total = total.expect("tone.opus declares a length");

    let (landed, tail, _) = opus_seek_and_decode(opus, total * 100, 4096);
    assert_eq!(
        landed, total,
        "a beyond-end seek clamps to the declared total"
    );
    assert!(
        tail.is_empty(),
        "seeking to the very end leaves nothing to decode (got {} samples)",
        tail.len()
    );
}

#[test]
fn open_surfaces_mp3_stream_info() {
    // MPEG-1 Layer III, 128 kbps, 44.1 kHz, stereo. A synthetic all-zero frame
    // body carries no spectral energy, so the codec decodes it to silence.
    // (Bit-exact decode of real LAME clips is covered by the golden-vector tests
    // in `formats::mp3`.)
    let b1 = 0xE0 | (0b11 << 3) | (0b01 << 1);
    let b2 = 9u8 << 4; // bitrate idx 9 (128k), srate idx 0 (44100), no padding
    let mut bytes = vec![0xFFu8, b1, b2, 0];
    bytes.resize(1024, 0);

    let mut dec = open(SliceSource::new(&bytes)).expect("probe + construct");
    assert_eq!(dec.stream_info().sample_rate, 44_100);
    assert_eq!(dec.stream_info().channels, 2);
    let mut out = [7i32; 8];
    let n = dec.decode_into(&mut out).unwrap();
    assert_eq!(n, 8);
    assert!(out.iter().all(|&s| s == 0), "silent frame decodes to zeros");
}

/// Decode-time-vs-real-time ratio for the workspace-root `tone-vorbis.ogg`
/// fixture (Task 11 — the fast-Huffman follow-up decision). `Codebook::decode_scalar`
/// is an O(entries·max_len) linear scan (deferred since Task 4: no fast-Huffman
/// acceleration table), and the residue hot loop calls it heavily — this is the
/// number that decides whether that matters yet. Measured `--release` at
/// ~0.40x real-time (2 s of audio decodes in ~0.8 s): comfortably faster than
/// real-time, same "the deck buffers ahead" territory as DSD's ~0.18x, so
/// fast-Huffman stays a nice-to-have, not implemented here. `#[ignore]`d — a
/// perf number, not a correctness gate; run explicitly with
/// `cargo test --release -p soul-audio-core --test decode \
/// vorbis_decode_speed_benchmark -- --ignored --nocapture` to re-check it
/// after a DSP change.
#[test]
#[ignore]
fn vorbis_decode_speed_benchmark() {
    let ogg: &'static [u8] = include_bytes!("../../../testdata/tone-vorbis.ogg");
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let storage = vec![0u8; 128 * 1024];
            let mut dec = VorbisDecoder::new(SliceSource::new(ogg), storage).expect("open vorbis");
            let channels = usize::from(dec.stream_info().channels);
            let rate = f64::from(dec.stream_info().sample_rate);
            let mut buf = [0i32; 4096];
            let start = std::time::Instant::now();
            let mut total = 0usize;
            loop {
                let n = dec.decode_into(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                total += n;
            }
            let elapsed = start.elapsed();
            let frames = total / channels;
            let real_time_secs = frames as f64 / rate;
            println!(
                "decoded {frames} frames ({real_time_secs:.3}s of audio) in {:.6}s -> ratio {:.5}x real-time",
                elapsed.as_secs_f64(),
                elapsed.as_secs_f64() / real_time_secs
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

// ---------------------------------------------------------------------------
// Opus CELT decode throughput micro-benchmark (issue #38 — fast MDCT).
// `#[ignore]` so it never runs in the default gate; invoke explicitly with
// `cargo test -p soul-audio-core --release --test decode opus_celt_decode_throughput -- --ignored --nocapture`.
// Decodes `tone.opus` in a loop and reports decode time vs audio duration.
// ---------------------------------------------------------------------------
#[test]
#[ignore = "manual benchmark; run with --ignored --nocapture"]
fn opus_celt_decode_throughput() {
    let opus: &'static [u8] = include_bytes!("../../../testdata/tone.opus");
    const ITERS: usize = 200;
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            // Warm up + measure total frames once.
            let frames = decode_all(opus).len() / 2;
            let real_time_secs = frames as f64 / 48_000.0;
            let start = std::time::Instant::now();
            let mut sink = 0u64;
            for _ in 0..ITERS {
                let out = decode_all(opus);
                sink = sink.wrapping_add(out.len() as u64);
            }
            let elapsed = start.elapsed();
            let per_decode = elapsed.as_secs_f64() / ITERS as f64;
            println!(
                "opus CELT: {ITERS} decodes of {frames} frames ({real_time_secs:.3}s audio) \
                 in {:.4}s total -> {:.6}s/decode, {:.2}x faster than real-time (sink={sink})",
                elapsed.as_secs_f64(),
                per_decode,
                real_time_secs / per_decode,
            );
        })
        .unwrap()
        .join()
        .unwrap();
}
