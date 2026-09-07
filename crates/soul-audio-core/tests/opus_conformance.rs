//! RFC 8251 Opus conformance gate.
//!
//! The 12 RFC 8251 test vectors (`opus_testvectors-rfc8251.tar.gz`) are the
//! definitive validation of the from-scratch Opus decoder. Each vector is an
//! `opus_demo` raw bitstream (`testvectorNN.bit`, per-packet
//! `[4-byte BE length][4-byte BE enc-final-range][packet]` framing — **not**
//! Ogg) plus two reference decodes: `testvectorNN.dec` (the stream decoded at
//! 2 channels) and `testvectorNNm.dec` (decoded at 1 channel), **both stored as
//! 48 kHz interleaved-stereo s16le** (that is why they are equal in size — the
//! official `opus_compare` always reads the reference as 2 channels).
//!
//! This file drives [`ConformanceDecoder`] — the raw-packet decode path that
//! reuses the shipping CELT/SILK synthesis with no Ogg demux, `OpusHead`,
//! pre-skip trim, or output gain (matching `opus_demo -d`, whose reference
//! decode uses `skip = 0`) — and scores its output against the reference with a
//! **faithful Rust port of the official `opus_compare` metric** (RFC 6716
//! Appendix, `opus_compare.c`): a Hann-windowed pseudo-NMR over 21 Bark-like
//! bands with frequency/temporal masking, reduced to a single quality metric
//! `Q`; `Q >= 0` is the RFC pass criterion (identical to `opus_compare`'s exit
//! code). The port was validated to agree with the C `opus_compare` on the
//! committed vectors (see the task report).
//!
//! Two gates:
//! * committed representative vectors (small truncated prefixes covering
//!   SILK↔CELT / Hybrid↔SILK mode transitions, redundancy, a silent-CELT frame,
//!   and stereo), embedded via `include_bytes!` so CI needs no external data or
//!   `gcc`/libopus;
//! * an `#[ignore]`d full-corpus mode ([`full_corpus`]) that reads all 12
//!   vectors from `$OPUS_VECTORS_DIR` (mirroring `soul-audio-tags`'s corpus
//!   pattern) for the complete RFC 8251 run.

use soul_audio_core::formats::opus::conformance::ConformanceDecoder;

// ---------------------------------------------------------------------------
// Raw `.bit` decode
// ---------------------------------------------------------------------------

/// Split an `opus_demo` `.bit` stream into its raw Opus packets (dropping the
/// per-packet length + `enc_final_range` prefixes).
fn bit_packets(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 8 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 8; // skip length + enc_final_range
        if i + len > data.len() {
            break;
        }
        out.push(&data[i..i + len]);
        i += len;
    }
    out
}

/// Decode a `.bit` stream at `channels` (1 or 2) into interleaved i16 samples —
/// the full untrimmed decoder output (no pre-skip), matching `opus_demo -d`.
fn decode_bit(data: &[u8], channels: usize) -> Vec<i16> {
    let mut dec = ConformanceDecoder::new(channels);
    let mut buf = vec![0i32; 5760 * channels];
    let mut pcm = Vec::new();
    for pkt in bit_packets(data) {
        let n = dec.decode_packet(pkt, &mut buf);
        pcm.extend(buf[..n].iter().map(|&s| s.clamp(-32768, 32767) as i16));
    }
    pcm
}

// ---------------------------------------------------------------------------
// Faithful port of the official `opus_compare` metric (RFC 6716 / opus_compare.c)
// ---------------------------------------------------------------------------

const NBANDS: usize = 21;
const NFREQS: usize = 240;
const TEST_WIN_SIZE: usize = 480;
const TEST_WIN_STEP: usize = 120;
/// Bark-derived CELT band edges (`opus_compare.c` `BANDS`).
const BANDS: [usize; NBANDS + 1] = [
    0, 2, 4, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 68, 80, 96, 120, 156, 200,
];

/// `band_energy` (opus_compare.c): Hann-windowed short-time band energies.
/// `out` receives per-band energy (or is skipped when `None`); `ps` receives the
/// per-bin power spectrum. All arrays are interleaved by `nchannels`.
#[allow(clippy::too_many_arguments)]
fn band_energy(
    out: Option<&mut [f32]>,
    ps: &mut [f32],
    in_: &[f32],
    nchannels: usize,
    nframes: usize,
    window_sz: usize,
    step: usize,
    downsample: usize,
) {
    let mut window = vec![0.0f32; window_sz];
    let mut c = vec![0.0f32; window_sz];
    let mut s = vec![0.0f32; window_sz];
    let mut x = vec![0.0f32; nchannels * window_sz];
    let ps_sz = window_sz / 2;
    let pi = std::f32::consts::PI;
    for (j, w) in window.iter_mut().enumerate() {
        *w = 0.5 - 0.5 * ((2.0 * pi / (window_sz as f32 - 1.0)) * j as f32).cos();
    }
    for (j, cj) in c.iter_mut().enumerate() {
        *cj = ((2.0 * pi / window_sz as f32) * j as f32).cos();
    }
    for (j, sj) in s.iter_mut().enumerate() {
        *sj = ((2.0 * pi / window_sz as f32) * j as f32).sin();
    }
    let mut out = out;
    for xi in 0..nframes {
        for ci in 0..nchannels {
            for xk in 0..window_sz {
                x[ci * window_sz + xk] = window[xk] * in_[(xi * step + xk) * nchannels + ci];
            }
        }
        let mut xj = 0usize;
        for bi in 0..NBANDS {
            let mut p = [0.0f32; 2];
            while xj < BANDS[bi + 1] {
                for ci in 0..nchannels {
                    let mut ti = 0usize;
                    let mut re = 0.0f32;
                    let mut im = 0.0f32;
                    for xk in 0..window_sz {
                        re += c[ti] * x[ci * window_sz + xk];
                        im -= s[ti] * x[ci * window_sz + xk];
                        ti += xj;
                        if ti >= window_sz {
                            ti -= window_sz;
                        }
                    }
                    re *= downsample as f32;
                    im *= downsample as f32;
                    let v = re * re + im * im + 100000.0;
                    ps[(xi * ps_sz + xj) * nchannels + ci] = v;
                    p[ci] += v;
                }
                xj += 1;
            }
            if let Some(out) = out.as_deref_mut() {
                let width = (BANDS[bi + 1] - BANDS[bi]) as f32;
                out[(xi * NBANDS + bi) * nchannels] = p[0] / width;
                if nchannels == 2 {
                    out[(xi * NBANDS + bi) * nchannels + 1] = p[1] / width;
                }
            }
        }
    }
}

/// Read an interleaved-stereo s16le reference buffer as `nchannels` float
/// samples (opus_compare `read_pcm16` with the argv[1] mono downmix): the
/// reference is always 2-channel; for a mono comparison the two channels are
/// averaged. Returns the per-channel sample count and the interleaved floats.
fn read_ref_as(nchannels: usize, ref_i16: &[i16]) -> Vec<f32> {
    // Reference is stored 2-channel.
    let stereo: Vec<f32> = ref_i16.iter().map(|&s| s as f32).collect();
    if nchannels == 2 {
        stereo
    } else {
        // 0.5*(L+R) per frame.
        (0..stereo.len() / 2)
            .map(|i| 0.5 * (stereo[2 * i] + stereo[2 * i + 1]))
            .collect()
    }
}

/// The official `opus_compare` quality metric `Q` (percent). `ref_i16` is the
/// reference (always interleaved-stereo s16le), `test_i16` the decode under test
/// (interleaved `nchannels`). Returns `None` when the sample counts do not match
/// or there is too little data (opus_compare's hard errors). `Q >= 0` is a pass.
fn opus_compare_q(nchannels: usize, ref_i16: &[i16], test_i16: &[i16]) -> Option<f64> {
    let downsample = 1usize; // 48 kHz.
    let mut x = read_ref_as(nchannels, ref_i16); // reference (interleaved nchannels)
    let xlength = x.len() / nchannels;
    let y: Vec<f32> = test_i16.iter().map(|&s| s as f32).collect();
    let ylength = y.len() / nchannels;
    if xlength != ylength * downsample || xlength < TEST_WIN_SIZE {
        return None;
    }
    let nframes = (xlength - TEST_WIN_SIZE + TEST_WIN_STEP) / TEST_WIN_STEP;
    let mut xb = vec![0.0f32; nframes * NBANDS * nchannels];
    let mut big_x = vec![0.0f32; nframes * NFREQS * nchannels];
    let mut big_y = vec![0.0f32; nframes * NFREQS * nchannels];
    band_energy(
        Some(&mut xb),
        &mut big_x,
        &x,
        nchannels,
        nframes,
        TEST_WIN_SIZE,
        TEST_WIN_STEP,
        1,
    );
    x.clear();
    x.shrink_to_fit();
    band_energy(
        None,
        &mut big_y,
        &y,
        nchannels,
        nframes,
        TEST_WIN_SIZE,
        TEST_WIN_STEP,
        downsample,
    );

    for xi in 0..nframes {
        // Frequency masking low→high: 10 dB/Bark.
        for bi in 1..NBANDS {
            for ci in 0..nchannels {
                xb[(xi * NBANDS + bi) * nchannels + ci] +=
                    0.1 * xb[(xi * NBANDS + bi - 1) * nchannels + ci];
            }
        }
        // Frequency masking high→low: 15 dB/Bark.
        for bi in (0..NBANDS - 1).rev() {
            for ci in 0..nchannels {
                xb[(xi * NBANDS + bi) * nchannels + ci] +=
                    0.03 * xb[(xi * NBANDS + bi + 1) * nchannels + ci];
            }
        }
        if xi > 0 {
            // Temporal masking: -3 dB/2.5 ms.
            for bi in 0..NBANDS {
                for ci in 0..nchannels {
                    xb[(xi * NBANDS + bi) * nchannels + ci] +=
                        0.5 * xb[((xi - 1) * NBANDS + bi) * nchannels + ci];
                }
            }
        }
        // Cross-talk.
        if nchannels == 2 {
            for bi in 0..NBANDS {
                let l = xb[(xi * NBANDS + bi) * nchannels];
                let r = xb[(xi * NBANDS + bi) * nchannels + 1];
                xb[(xi * NBANDS + bi) * nchannels] += 0.01 * r;
                xb[(xi * NBANDS + bi) * nchannels + 1] += 0.01 * l;
            }
        }
        // Apply masking to the per-bin spectra.
        for bi in 0..NBANDS {
            for xj in BANDS[bi]..BANDS[bi + 1] {
                for ci in 0..nchannels {
                    big_x[(xi * NFREQS + xj) * nchannels + ci] +=
                        0.1 * xb[(xi * NBANDS + bi) * nchannels + ci];
                    big_y[(xi * NFREQS + xj) * nchannels + ci] +=
                        0.1 * xb[(xi * NBANDS + bi) * nchannels + ci];
                }
            }
        }
    }

    // Average consecutive frames (in place, per opus_compare).
    for bi in 0..NBANDS {
        for xj in BANDS[bi]..BANDS[bi + 1] {
            for ci in 0..nchannels {
                let mut xtmp = big_x[xj * nchannels + ci];
                let mut ytmp = big_y[xj * nchannels + ci];
                for xi in 1..nframes {
                    let xtmp2 = big_x[(xi * NFREQS + xj) * nchannels + ci];
                    let ytmp2 = big_y[(xi * NFREQS + xj) * nchannels + ci];
                    big_x[(xi * NFREQS + xj) * nchannels + ci] += xtmp;
                    big_y[(xi * NFREQS + xj) * nchannels + ci] += ytmp;
                    xtmp = xtmp2;
                    ytmp = ytmp2;
                }
            }
        }
    }

    let max_compare = BANDS[NBANDS]; // rate == 48000
    let mut err = 0.0f64;
    for xi in 0..nframes {
        let mut ef = 0.0f64;
        for bi in 0..NBANDS {
            let mut eb = 0.0f64;
            let mut xj = BANDS[bi];
            while xj < BANDS[bi + 1] && xj < max_compare {
                for ci in 0..nchannels {
                    let re = big_y[(xi * NFREQS + xj) * nchannels + ci]
                        / big_x[(xi * NFREQS + xj) * nchannels + ci];
                    let mut im = (re - re.ln() - 1.0) as f64;
                    if (79..=81).contains(&xj) {
                        im *= 0.1;
                    }
                    if xj == 80 {
                        im *= 0.1;
                    }
                    eb += im;
                }
                xj += 1;
            }
            eb /= ((BANDS[bi + 1] - BANDS[bi]) * nchannels) as f64;
            ef += eb * eb;
        }
        ef /= NBANDS as f64;
        ef *= ef;
        err += ef * ef;
    }
    err = (err / nframes as f64).powf(1.0 / 16.0);
    let q = 100.0 * (1.0 - 0.5 * (1.0 + err).ln() / 1.13f64.ln());
    Some(q)
}

/// Run the RFC 8251 pass criterion for one vector at `channels` output channels,
/// mirroring `run_vectors.sh`: score against BOTH reference decodes (stereo
/// `dec` and mono `m_dec`, both stored 2-channel) and pass if EITHER gives
/// `Q >= 0`. Returns `(passed, best_Q)`.
fn score_vector(channels: usize, bit: &[u8], dec: &[i16], m_dec: &[i16]) -> (bool, f64) {
    let out = decode_bit(bit, channels);
    let q1 = opus_compare_q(channels, dec, &out);
    let q2 = opus_compare_q(channels, m_dec, &out);
    let best = q1.into_iter().chain(q2).fold(f64::NEG_INFINITY, f64::max);
    (best >= 0.0, best)
}

fn load_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

// ---------------------------------------------------------------------------
// Committed representative vectors (small truncated prefixes; see module docs)
// ---------------------------------------------------------------------------

/// One committed vector: the truncated `.bit` and its two truncated references.
struct Committed {
    name: &'static str,
    bit: &'static [u8],
    dec: &'static [u8],
    m_dec: &'static [u8],
}

fn committed() -> Vec<Committed> {
    vec![
        Committed {
            name: "testvector08 (SILK↔CELT transition + redundancy + silent-CELT frame, stereo)",
            bit: include_bytes!("../testdata/opus_tv08.bit"),
            dec: include_bytes!("../testdata/opus_tv08.dec"),
            m_dec: include_bytes!("../testdata/opus_tv08m.dec"),
        },
        Committed {
            name: "testvector12 (Hybrid↔SILK transitions, mono)",
            bit: include_bytes!("../testdata/opus_tv12.bit"),
            dec: include_bytes!("../testdata/opus_tv12.dec"),
            m_dec: include_bytes!("../testdata/opus_tv12m.dec"),
        },
    ]
}

/// The committed representative vectors pass the official-`opus_compare`-metric
/// gate (`Q >= 0`) at BOTH mono and stereo decode.
#[test]
fn committed_vectors_pass_rfc8251_metric() {
    for v in committed() {
        let dec = load_i16(v.dec);
        let m_dec = load_i16(v.m_dec);
        for &ch in &[1usize, 2] {
            let (pass, q) = score_vector(ch, v.bit, &dec, &m_dec);
            assert!(
                pass,
                "{} ch{ch}: RFC 8251 opus_compare metric FAILS (best Q = {q:.2} % < 0)",
                v.name
            );
        }
    }
}

/// Non-tautology guard: a *corrupted* decode (mid-signal noise burst) must
/// FAIL the same metric that the true decode passes — proving the gate has
/// discriminating power and is not trivially satisfied.
#[test]
fn corrupted_output_fails_the_metric() {
    let v = &committed()[0];
    let dec = load_i16(v.dec);
    let m_dec = load_i16(v.m_dec);
    // True mono decode passes.
    let (pass_true, q_true) = score_vector(1, v.bit, &dec, &m_dec);
    assert!(pass_true, "sanity: true decode should pass (Q={q_true:.2})");

    // Corrupt the decode: overwrite a contiguous middle span with a loud tone.
    let good = decode_bit(v.bit, 1);
    let mut bad = good.clone();
    let start = bad.len() / 4;
    let end = (bad.len() * 3 / 4).min(bad.len());
    for (i, s) in bad[start..end].iter_mut().enumerate() {
        *s = if i % 2 == 0 { 12000 } else { -12000 };
    }
    let q1 = opus_compare_q(1, &dec, &bad);
    let q2 = opus_compare_q(1, &m_dec, &bad);
    let best = q1.into_iter().chain(q2).fold(f64::NEG_INFINITY, f64::max);
    assert!(
        best < 0.0,
        "corrupted decode must FAIL the metric, got Q={best:.2} %"
    );
}

// ---------------------------------------------------------------------------
// Full corpus (all 12 vectors) — reads from $OPUS_VECTORS_DIR, ignored by
// default (mirrors soul-audio-tags' large-corpus gating).
// ---------------------------------------------------------------------------

/// Full RFC 8251 run over all 12 vectors. Set `OPUS_VECTORS_DIR` to the
/// extracted `opus_newvectors` directory (from
/// `opus_testvectors-rfc8251.tar.gz`) and run with `--ignored`:
///
/// ```text
/// OPUS_VECTORS_DIR=/path/to/opus_newvectors \
///   cargo test -p soul-audio-core --test opus_conformance full_corpus -- --ignored --nocapture
/// ```
///
/// Each vector must pass at BOTH mono and stereo (the `run_vectors.sh` criterion).
#[test]
#[ignore]
fn full_corpus() {
    let dir = std::env::var("OPUS_VECTORS_DIR")
        .expect("set OPUS_VECTORS_DIR to the extracted opus_newvectors directory");
    let mut failures = Vec::new();
    for n in 1..=12 {
        let name = format!("testvector{n:02}");
        let bit = std::fs::read(format!("{dir}/{name}.bit")).expect("read .bit");
        let dec = load_i16(&std::fs::read(format!("{dir}/{name}.dec")).expect("read .dec"));
        let m_dec = load_i16(&std::fs::read(format!("{dir}/{name}m.dec")).expect("read m.dec"));
        for (ch, tag) in [(1usize, "mono"), (2usize, "stereo")] {
            let (pass, q) = score_vector(ch, &bit, &dec, &m_dec);
            eprintln!(
                "{name} {tag}: Q={q:.2} % -> {}",
                if pass { "PASS" } else { "FAIL" }
            );
            if !pass {
                failures.push(format!("{name} {tag} (Q={q:.2})"));
            }
        }
    }
    assert!(failures.is_empty(), "RFC 8251 failures: {failures:?}");
}
