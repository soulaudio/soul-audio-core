//! Integration tests exercising the public decode API end-to-end.

use soul_audio_core::{DecodeError, Decoder, SliceSource, open};

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

/// Decode a full stream through the public `open` API and return interleaved PCM.
///
/// Runs on a generous stack: an `AnyDecoder` is large by value (no-alloc), and
/// the HE-AAC (SBR) decoder in particular carries ~250 KiB of QMF/HF state plus
/// per-frame DSP scratch — more than the default test-thread stack. This mirrors
/// the CLI's own big-stack loader thread (see `soul-audio-cli` / MEMORY.md).
fn decode_all(bytes: &[u8]) -> Vec<i32> {
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
                out
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked")
    })
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
