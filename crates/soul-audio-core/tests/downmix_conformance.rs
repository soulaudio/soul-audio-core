//! Downmix **conformance vs ffmpeg** — the independent oracle.
//!
//! The deck's own downmix tests (`tests/downmix.rs`) assert against the same `1/√2`
//! constant the implementation uses, so they can't catch a systematic misreading of the
//! ITU-R BS.775 matrix. ffmpeg is a second, independent implementation of that same matrix:
//! its default `-ac 2` fold is `FL = FL + 0.707·FC + 0.707·SL`, `FR = FR + 0.707·FC +
//! 0.707·SR`, LFE dropped — exactly ours. So we render a 5.1 source through our deck and
//! through `ffmpeg -ac 2` and assert the two stereo streams agree to a high **scale-
//! invariant** SNR (immune to any global normalization ffmpeg may apply). A wrong
//! coefficient, a dropped channel, or an L/R speaker swap drops the SNR off a cliff.
//!
//! The source's LFE is silent, so the one spot implementations legitimately differ (keep vs
//! drop LFE) can't make a correct fold look wrong.
//!
//! `#[ignore]`d: ffmpeg is not on CI. Run with ffmpeg on `PATH` (or set `FFMPEG_BIN`):
//!
//! ```text
//! cargo test -p soul-audio-core --test downmix_conformance -- --ignored --nocapture
//! ```

use std::io::Write;
use std::process::Command;

use soul_audio_core::{
    AnyDecoder, Deck, Decoder, Progress, Quality, SampleFormat, Sink, SinkError, SliceSource,
    StreamInfo, channels::speaker, decoder_scratch_len, open,
};

const RATE: u32 = 48_000;
const FRAMES: usize = 24_000; // 0.5 s
/// Compare a steady window clear of the deck's seek/fade-in ramp.
const SKIP: usize = 6_000;
const WIN: usize = 12_000;

/// ffmpeg binary: `FFMPEG_BIN` or `ffmpeg` on PATH. `None` ⇒ the test no-ops (ignored).
fn ffmpeg_bin() -> Option<String> {
    if let Ok(p) = std::env::var("FFMPEG_BIN") {
        return Some(p);
    }
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "ffmpeg".to_string())
}

/// Pack a signed 24-bit sample into 3 little-endian bytes.
fn push_s24(buf: &mut Vec<u8>, v: i32) {
    buf.push((v & 0xFF) as u8);
    buf.push(((v >> 8) & 0xFF) as u8);
    buf.push(((v >> 16) & 0xFF) as u8);
}

/// Read a signed 24-bit little-endian sample.
fn read_s24(b: &[u8]) -> i32 {
    let raw = (b[0] as i32) | (b[1] as i32) << 8 | (b[2] as i32) << 16;
    (raw << 8) >> 8 // sign-extend from bit 23
}

/// A WAVE_FORMAT_EXTENSIBLE 24-bit PCM file: `mask` names the layout; `planes[c]` is channel
/// `c`'s samples (all equal length), interleaved into the `data` chunk.
fn wav_ext_24(mask: u32, planes: &[Vec<i32>]) -> Vec<u8> {
    let ch = planes.len() as u16;
    let frames = planes[0].len();
    let block_align = ch * 3;
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&ch.to_le_bytes());
    fmt.extend_from_slice(&RATE.to_le_bytes());
    fmt.extend_from_slice(&(RATE * u32::from(block_align)).to_le_bytes());
    fmt.extend_from_slice(&block_align.to_le_bytes());
    fmt.extend_from_slice(&24u16.to_le_bytes());
    fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    fmt.extend_from_slice(&24u16.to_le_bytes()); // valid bits
    fmt.extend_from_slice(&mask.to_le_bytes()); // dwChannelMask
    // KSDATAFORMAT_SUBTYPE_PCM {00000001-0000-0010-8000-00AA00389B71} — the full GUID, so
    // ffmpeg (which validates all 16 bytes) accepts the stream as PCM.
    fmt.extend_from_slice(&[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ]);

    let mut data = Vec::with_capacity(frames * usize::from(ch) * 3);
    for f in 0..frames {
        for plane in planes {
            push_s24(&mut data, plane[f]);
        }
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&fmt);
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    bytes
}

/// A pure sine at `hz`, amplitude `amp`, `FRAMES` long at [`RATE`].
fn tone(hz: f64, amp: f64) -> Vec<i32> {
    (0..FRAMES)
        .map(|n| {
            let t = n as f64 / RATE as f64;
            (amp * (2.0 * std::f64::consts::PI * hz * t).sin()).round() as i32
        })
        .collect()
}

/// Minimal stereo-capturing sink.
struct Cap {
    w: Vec<i32>,
}
impl Sink for Cap {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: RATE,
            channels: 2,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, f: &[i32]) -> Result<(), SinkError> {
        self.w.extend_from_slice(f);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// Decode `wav` through the public `open()` path into a stereo deck; return interleaved L/R.
fn our_downmix(wav: &[u8]) -> Vec<i32> {
    let mut deck: Deck<Cap, AnyDecoder<SliceSource, Vec<i32>>, 0> =
        Deck::new(Cap { w: Vec::new() }, Quality::Hq);
    let dec = open(SliceSource::new(wav), vec![0i32; decoder_scratch_len()]).expect("open");
    deck.load(dec).expect("load");
    for _ in 0..1_000_000 {
        match deck.pump().expect("pump") {
            Progress::Ended | Progress::Idle => break,
            _ => {}
        }
    }
    deck.sink().w.clone()
}

/// `ffmpeg -ac 2` the source WAV to 24-bit stereo; return interleaved L/R.
fn ffmpeg_downmix(bin: &str, wav: &[u8]) -> Vec<i32> {
    let dir = std::env::temp_dir();
    let src = dir.join("soul_downmix_src.wav");
    let out = dir.join("soul_downmix_out.wav");
    std::fs::File::create(&src).unwrap().write_all(wav).unwrap();
    let status = Command::new(bin)
        .args(["-y", "-i"])
        .arg(&src)
        .args(["-ac", "2", "-c:a", "pcm_s24le", "-f", "wav"])
        .arg(&out)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        status.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    parse_wav_s24_stereo(&std::fs::read(&out).unwrap())
}

/// Parse interleaved 24-bit stereo samples from the `data` chunk of a canonical WAV.
fn parse_wav_s24_stereo(bytes: &[u8]) -> Vec<i32> {
    // Walk chunks from offset 12 to find `data`.
    let mut p = 12;
    while p + 8 <= bytes.len() {
        let id = &bytes[p..p + 4];
        let sz =
            u32::from_le_bytes([bytes[p + 4], bytes[p + 5], bytes[p + 6], bytes[p + 7]]) as usize;
        let body = p + 8;
        if id == b"data" {
            let end = (body + sz).min(bytes.len());
            return bytes[body..end].chunks_exact(3).map(read_s24).collect();
        }
        p = body + sz + (sz & 1);
    }
    panic!("no data chunk in ffmpeg output");
}

/// Scale-invariant SNR (dB) of `a` against `b`: the best global scale `k` is fit so a global
/// normalization in `b` can't hurt, then SNR is signal/residual. `f64::INFINITY` if exact.
fn scale_invariant_snr(a: &[i32], b: &[i32]) -> f64 {
    assert_eq!(a.len(), b.len());
    let (mut ab, mut bb, mut aa) = (0.0f64, 0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (x as f64, y as f64);
        ab += x * y;
        bb += y * y;
        aa += x * x;
    }
    if bb == 0.0 {
        return 0.0;
    }
    let k = ab / bb;
    let mut err = 0.0f64;
    for (&x, &y) in a.iter().zip(b) {
        let e = x as f64 - k * y as f64;
        err += e * e;
    }
    if err == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (aa / err).log10()
    }
}

#[test]
#[ignore = "requires ffmpeg (FFMPEG_BIN or on PATH)"]
fn downmix_5_1_matches_ffmpeg_ac2() {
    let Some(bin) = ffmpeg_bin() else {
        eprintln!("skip: no ffmpeg");
        return;
    };
    // 5.1 = FL FR FC LFE BL BR. Distinct tones per channel; LFE silent (so a keep-vs-drop
    // LFE difference can't skew the comparison). Amplitudes keep the fold within full scale.
    let mask = speaker::FL | speaker::FR | speaker::FC | speaker::LFE | speaker::BL | speaker::BR;
    let a = 1_500_000.0;
    let planes = vec![
        tone(220.0, a),  // FL
        tone(330.0, a),  // FR
        tone(440.0, a),  // FC
        vec![0; FRAMES], // LFE (silent)
        tone(550.0, a),  // BL
        tone(660.0, a),  // BR
    ];
    let wav = wav_ext_24(mask, &planes);

    let ours = our_downmix(&wav);
    let theirs = ffmpeg_downmix(&bin, &wav);

    // Compare a steady, equal-length window clear of our fade-in. Both paths are the same
    // rate (no resample) so sample N ↔ sample N.
    let n = WIN * 2;
    let o = &ours[SKIP * 2..SKIP * 2 + n];
    let t = &theirs[SKIP * 2..SKIP * 2 + n];
    let snr = scale_invariant_snr(o, t);
    println!("5.1→stereo conformance vs ffmpeg: {snr:.1} dB");
    assert!(
        snr > 60.0,
        "our 5.1 downmix diverges from ffmpeg -ac 2 (SNR {snr:.1} dB); coefficient/layout bug?"
    );
}

/// Encode `wav` to another codec via ffmpeg (so ffmpeg maps the WAVEFORMATEXTENSIBLE channels
/// into the codec's own native order), returning the encoded file bytes.
fn ffmpeg_encode(bin: &str, wav: &[u8], codec_args: &[&str], out_name: &str) -> Vec<u8> {
    let dir = std::env::temp_dir();
    // Unique per fixture: tests run in parallel, and a shared source path would let one
    // test's WAV clobber another's mid-encode.
    let src = dir.join(format!("{out_name}.src.wav"));
    let out = dir.join(out_name);
    std::fs::File::create(&src).unwrap().write_all(wav).unwrap();
    let status = Command::new(bin)
        .args(["-y", "-i"])
        .arg(&src)
        .args(codec_args)
        .arg(&out)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        status.status.success(),
        "ffmpeg encode failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    std::fs::read(&out).unwrap()
}

fn rms(xs: &[i32]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    (xs.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / xs.len() as f64).sqrt()
}

/// Opus (mapping family 1) puts the centre channel SECOND (`FL C FR …`), not in
/// WAVEFORMATEXTENSIBLE order. A 5.1 source with audio ONLY in the front-centre, encoded to
/// Opus and decoded through our deck, must fold the centre EQUALLY to L and R (−3 dB each).
/// If we wrongly assumed WAVEFORMATEXTENSIBLE order (centre = channel 2 = FR), the centre
/// would land only in R and L would be silent — so this is a direct order check, end to end
/// through the real `open()` → multistream-Opus → deck path.
#[test]
#[ignore = "requires ffmpeg (FFMPEG_BIN or on PATH)"]
fn opus_surround_folds_centre_by_family1_order() {
    let Some(bin) = ffmpeg_bin() else {
        eprintln!("skip: no ffmpeg");
        return;
    };
    let mask = speaker::FL | speaker::FR | speaker::FC | speaker::LFE | speaker::BL | speaker::BR;
    let planes = vec![
        vec![0; FRAMES],          // FL
        vec![0; FRAMES],          // FR
        tone(440.0, 1_500_000.0), // FC — the only audio
        vec![0; FRAMES],          // LFE
        vec![0; FRAMES],          // BL
        vec![0; FRAMES],          // BR
    ];
    let src = wav_ext_24(mask, &planes);
    // ffmpeg maps WAVEFORMATEXTENSIBLE FC into Opus's family-1 centre (channel 1).
    let opus = ffmpeg_encode(
        &bin,
        &src,
        &["-c:a", "libopus", "-mapping_family", "1", "-f", "ogg"],
        "soul_dm.opus",
    );

    let ours = our_downmix(&opus);
    let end = (SKIP * 2 + WIN * 2).min(ours.len());
    assert!(end > SKIP * 2, "no steady Opus output decoded");
    let o = &ours[SKIP * 2..end];
    let l: Vec<i32> = o.iter().step_by(2).copied().collect();
    let r: Vec<i32> = o.iter().skip(1).step_by(2).copied().collect();
    let (rl, rr) = (rms(&l), rms(&r));
    println!("Opus 5.1 centre fold: L rms={rl:.0} R rms={rr:.0}");
    assert!(
        rl > 1_000.0 && rr > 1_000.0,
        "centre must fold to BOTH L and R (L rms={rl:.0} R rms={rr:.0}); channel order wrong?"
    );
    // Both sides are the same source channel folded at the same gain, so they must match.
    let snr = scale_invariant_snr(&l, &r);
    assert!(
        snr > 40.0,
        "a centre-only fold must make L == R (SNR {snr:.1} dB); channel order wrong?"
    );
}

/// AAC's `channel_configuration` decode order puts the centre channel FIRST (the leading
/// SCE), not in WAVEFORMATEXTENSIBLE order. A 5.1 source with audio ONLY in the front-centre,
/// encoded to AAC and decoded through our deck, must fold the centre EQUALLY to L and R. If
/// we mis-read the order (centre taken as front-left/right), the centre would skew to one
/// side. End to end through the real `open()` -> AAC -> deck path.
#[test]
// AAC decode is patent-gated off by default, so without the feature this test
// cannot open its own fixture — it used to die with a bare `open: Unsupported`
// partway through an otherwise-green `--ignored` run, which reads as a decoder
// bug rather than a missing feature. Run it with
// `--features aac --test downmix_conformance -- --ignored`.
#[cfg(feature = "aac")]
#[ignore = "requires ffmpeg (FFMPEG_BIN or on PATH) and --features aac"]
fn aac_surround_folds_centre_by_config_order() {
    let Some(bin) = ffmpeg_bin() else {
        eprintln!("skip: no ffmpeg");
        return;
    };
    let mask = speaker::FL | speaker::FR | speaker::FC | speaker::LFE | speaker::BL | speaker::BR;
    let planes = vec![
        vec![0; FRAMES],          // FL
        vec![0; FRAMES],          // FR
        tone(440.0, 1_500_000.0), // FC — the only audio
        vec![0; FRAMES],          // LFE
        vec![0; FRAMES],          // BL
        vec![0; FRAMES],          // BR
    ];
    let src = wav_ext_24(mask, &planes);
    // ADTS AAC-LC 5.1 (channel_configuration 6); ffmpeg maps WAVEFORMATEXTENSIBLE FC into the
    // AAC centre element.
    let aac = ffmpeg_encode(
        &bin,
        &src,
        &["-c:a", "aac", "-b:a", "256k", "-f", "adts"],
        "soul_dm.aac",
    );

    let ours = our_downmix(&aac);
    let end = (SKIP * 2 + WIN * 2).min(ours.len());
    assert!(end > SKIP * 2, "no steady AAC output decoded");
    let o = &ours[SKIP * 2..end];
    let l: Vec<i32> = o.iter().step_by(2).copied().collect();
    let r: Vec<i32> = o.iter().skip(1).step_by(2).copied().collect();
    let (rl, rr) = (rms(&l), rms(&r));
    println!("AAC 5.1 centre fold: L rms={rl:.0} R rms={rr:.0}");
    assert!(
        rl > 1_000.0 && rr > 1_000.0,
        "centre must fold to BOTH L and R (L rms={rl:.0} R rms={rr:.0}); channel order wrong?"
    );
    let snr = scale_invariant_snr(&l, &r);
    assert!(
        snr > 40.0,
        "a centre-only fold must make L == R (SNR {snr:.1} dB); channel order wrong?"
    );
}

/// Goertzel single-bin power at `hz` (sample rate [`RATE`]). Only relative powers compared.
fn goertzel(samples: &[i32], hz: f64) -> f64 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let w = 2.0 * std::f64::consts::PI * hz / RATE as f64;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s0 = x as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2) / n as f64
}

/// Decode `bytes` through the public `open()` path into `ch` per-channel planes (a steady
/// window past any decoder priming).
fn decode_planes(bytes: &[u8], ch: usize) -> Vec<Vec<i32>> {
    let mut dec = open(SliceSource::new(bytes), vec![0i32; decoder_scratch_len()]).expect("open");
    let mut pcm: Vec<i32> = Vec::new();
    let mut buf = vec![0i32; 4096 * ch];
    for _ in 0..10_000 {
        let n = dec.decode_into(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        pcm.extend_from_slice(&buf[..n]);
        if pcm.len() >= ch * (SKIP + WIN) {
            break;
        }
    }
    let start = (SKIP * ch).min(pcm.len());
    let frames = &pcm[start..];
    (0..ch)
        .map(|c| frames.chunks_exact(ch).map(|f| f[c]).collect())
        .collect()
}

/// ALAC multichannel: every channel must decode correctly AND land in the right position.
/// Encode a 5.1 source with a DISTINCT tone per speaker, decode through the real `open()`
/// path, and check each decoded channel carries the tone its speaker should - which proves
/// both that the multichannel decode works and that the reported channel order (`alac_layout`,
/// centre first) matches what the decoder emits.
#[test]
#[ignore = "requires ffmpeg (FFMPEG_BIN or on PATH)"]
fn alac_5_1_decodes_each_channel_in_order() {
    let Some(bin) = ffmpeg_bin() else {
        eprintln!("skip: no ffmpeg");
        return;
    };
    // WAVEFORMATEXTENSIBLE order FL FR FC LFE BL BR, one distinct tone each.
    let mask = speaker::FL | speaker::FR | speaker::FC | speaker::LFE | speaker::BL | speaker::BR;
    let f = [300.0, 400.0, 500.0, 600.0, 700.0, 800.0];
    let src_planes: Vec<Vec<i32>> = f.iter().map(|&hz| tone(hz, 1_500_000.0)).collect();
    let wav = wav_ext_24(mask, &src_planes);
    // ffmpeg maps by speaker into ALAC's centre-first order (C L R Ls Rs LFE).
    let alac = ffmpeg_encode(
        &bin,
        &wav,
        &[
            "-c:a",
            "alac",
            "-sample_fmt",
            "s16p",
            "-movflags",
            "+faststart",
        ],
        "soul_dm_alac.m4a",
    );

    let planes = decode_planes(&alac, 6);
    assert_eq!(planes.len(), 6, "ALAC did not decode 6 channels");
    assert!(planes[0].len() > WIN / 2, "too little ALAC audio decoded");
    // alac_layout(6) = [FC, FL, FR, BL, BR, LFE] → the tone expected in each decoded channel.
    let expect = [500.0, 300.0, 400.0, 700.0, 800.0, 600.0];
    for (i, &want) in expect.iter().enumerate() {
        let p_want = goertzel(&planes[i], want);
        let p_other = f
            .iter()
            .filter(|&&hz| hz != want)
            .map(|&hz| goertzel(&planes[i], hz))
            .fold(0.0, f64::max);
        println!("ALAC plane {i}: {want} Hz power {p_want:.2e} vs best other {p_other:.2e}");
        assert!(
            p_want > p_other * 10.0,
            "ALAC channel {i} must carry {want} Hz (multichannel decode or channel order wrong)"
        );
    }
}

/// ALAC 5.1 end to end through the deck: a centre-only source folds EQUALLY to L and R,
/// confirming the ordered layout reaches the deck via `open()` -> ALAC -> fold.
#[test]
#[ignore = "requires ffmpeg (FFMPEG_BIN or on PATH)"]
fn alac_surround_folds_centre() {
    let Some(bin) = ffmpeg_bin() else {
        eprintln!("skip: no ffmpeg");
        return;
    };
    let mask = speaker::FL | speaker::FR | speaker::FC | speaker::LFE | speaker::BL | speaker::BR;
    let planes = vec![
        vec![0; FRAMES],
        vec![0; FRAMES],
        tone(440.0, 1_500_000.0),
        vec![0; FRAMES],
        vec![0; FRAMES],
        vec![0; FRAMES],
    ];
    let wav = wav_ext_24(mask, &planes);
    let alac = ffmpeg_encode(
        &bin,
        &wav,
        &[
            "-c:a",
            "alac",
            "-sample_fmt",
            "s16p",
            "-movflags",
            "+faststart",
        ],
        "soul_dm_alac_c.m4a",
    );
    let ours = our_downmix(&alac);
    let end = (SKIP * 2 + WIN * 2).min(ours.len());
    assert!(end > SKIP * 2, "no steady ALAC output decoded");
    let o = &ours[SKIP * 2..end];
    let l: Vec<i32> = o.iter().step_by(2).copied().collect();
    let r: Vec<i32> = o.iter().skip(1).step_by(2).copied().collect();
    let (rl, rr) = (rms(&l), rms(&r));
    println!("ALAC 5.1 centre fold: L rms={rl:.0} R rms={rr:.0}");
    assert!(
        rl > 1_000.0 && rr > 1_000.0 && scale_invariant_snr(&l, &r) > 40.0,
        "centre must fold equally to L and R (L rms={rl:.0} R rms={rr:.0}); channel order wrong?"
    );
}
