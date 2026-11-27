//! DSD-over-PCM end-to-end through the CLI's `render` command.
//!
//! `soul-audio-desktop` already pins the DoP **framer** (`open_track_dsd` →
//! markers at the carrier rate), and `soul-audio-core` pins the **policy**
//! (`dop::plan`). What neither covers is the part that actually breaks on real
//! hardware: whether the 1-bit stream survives the *pipeline* between them.
//! Every stage of the deck is a way to lose it — the resampler interpolates the
//! marker byte, the downmixer averages two channels' markers, a gain scales
//! them, normalization shifts them — and all of those are silent: the file
//! plays, the DAC emits full-level noise.
//!
//! So these tests assert the whole chain, engine → deck → sink, against the
//! decoder's own output: for a DoP render the bytes must be **bit-identical**
//! to what the framer produced, and for the default (decimated) render they
//! must not be. `render` is the offline counterpart of `play`, so this is the
//! same pipeline a device would get.
//!
//! `play --dsd dop` deliberately does not exist: cpal's shared device runs at
//! its own f32/i16 mixer format, so the OS would rescale every word and put
//! full-level noise into someone's headphones. Delivering DoP needs one of the
//! exclusive engines; this is how the stream is verified until the harness
//! opens one.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use soul_audio_core::Decoder;
use soul_audio_core::model::DsdMode;
use soul_audio_desktop::open_track_dsd;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("soul-dop-{name}"))
}

/// Read a raw interleaved `i32` LE PCM render.
fn read_pcm(path: &Path) -> Vec<i32> {
    let bytes = std::fs::read(path).expect("read render");
    assert_eq!(bytes.len() % 4, 0, "render is not whole i32 words");
    bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Decode `name` straight through the decoder in `mode`, with no engine, deck
/// or sink in the way — the reference the render is compared against. Runs on a
/// big stack: `OwnedDecoder` is large by value (no-alloc), like its siblings.
fn decode_reference(name: &str, mode: DsdMode) -> (u32, u16, Vec<i32>) {
    let path = testdata(name);
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let mut d = open_track_dsd(&path, mode).expect("open fixture");
            let info = d.stream_info();
            let mut out = Vec::new();
            let mut buf = [0i32; 8192];
            loop {
                let n = d.decode_into(&mut buf).expect("decode");
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            (info.sample_rate, info.channels, out)
        })
        .expect("spawn reference decode thread")
        .join()
        .expect("reference decode thread panicked")
}

/// Assert `pcm` is a valid DoP stream: canonical signed-24 samples whose low 24
/// bits are DoP words, the marker byte alternating `0x05`/`0xFA` once per frame
/// and shared by every channel of a frame.
///
/// Reading the marker off `sample as u32` (not `sample`) is the point: a
/// `0xFA`-marked word has bit 23 set, so the canonical sample is **negative**.
/// A framer that emitted these as positive values instead would be outside
/// canonical range and clamped to `0x7F_FFFF` by the deck — which is exactly
/// the bug this file was written to catch.
fn assert_dop_framing(pcm: &[i32], channels: usize) {
    assert!(!pcm.is_empty(), "nothing rendered");
    assert_eq!(pcm.len() % channels, 0, "partial frame in the render");
    let first = (pcm[0] as u32 >> 16) & 0xFF;
    assert!(
        first == 0x05 || first == 0xFA,
        "first word carries no DoP marker: {first:#04x}"
    );
    for (f, frame) in pcm.chunks_exact(channels).enumerate() {
        // The phase is taken from the first frame rather than assumed, so a
        // render that starts mid-pair (a trim) is still checked for alternation.
        let want = if f % 2 == 0 {
            first
        } else {
            first ^ (0x05 ^ 0xFA)
        };
        for (ch, &word) in frame.iter().enumerate() {
            assert_eq!(
                (word as u32 >> 16) & 0xFF,
                want,
                "frame {f} channel {ch}: DoP marker broken — a DAC plays this as noise"
            );
            assert!(
                (-(1 << 23)..1 << 23).contains(&word),
                "frame {f} channel {ch}: {word:#x} is outside canonical signed-24, \
                 so the deck clamps it"
            );
        }
    }
}

/// `render --dsd dop` must hand the sink exactly what the framer produced: the
/// carrier format untouched, every marker intact, bit-identical to the decoder.
#[test]
fn dop_render_is_bit_identical_to_the_framer() {
    let out = tmp("passthrough.pcm");
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("render")
        .arg(testdata("tone.dsf"))
        .args(["--out"])
        .arg(&out)
        .args(["--dsd", "dop"])
        .assert()
        .success();

    let (rate, channels, reference) = decode_reference("tone.dsf", DsdMode::Dop);
    assert_eq!(rate, 176_400, "DSD64 → a 176.4 kHz carrier");
    assert_eq!(channels, 2);

    let pcm = read_pcm(&out);
    assert_dop_framing(&pcm, usize::from(channels));
    assert_eq!(
        pcm.len(),
        reference.len(),
        "the pipeline changed the DoP stream's length"
    );
    assert_eq!(
        pcm, reference,
        "the pipeline altered DoP words — the carrier is not bit-perfect"
    );
}

/// The default (`--dsd decimate`, i.e. no flag) is unchanged by any of this: a
/// DSD file still renders as PCM at its decimated rate, and that output is
/// emphatically *not* a DoP stream.
#[test]
fn the_default_still_decimates_dsd_to_pcm() {
    let out = tmp("decimated.pcm");
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("render")
        .arg(testdata("tone.dsf"))
        .args(["--out"])
        .arg(&out)
        // The decimated rate, so the resampler is a passthrough and the
        // comparison below is exact.
        .args(["--rate", "44100", "--channels", "2"])
        .assert()
        .success();

    let (rate, channels, reference) = decode_reference("tone.dsf", DsdMode::Decimate);
    assert_eq!(rate, 44_100, "the decimated path is unchanged");
    let pcm = read_pcm(&out);
    assert_eq!(pcm, reference, "decimated render is not the decoded PCM");

    // And it is PCM, not a carrier: a real DoP stream has the marker in every
    // single word, which decimated audio does not.
    let marked = pcm
        .chunks_exact(usize::from(channels))
        .filter(|f| matches!((f[0] >> 16) & 0xFF, 0x05 | 0xFA))
        .count();
    let frames = pcm.len() / usize::from(channels);
    assert!(
        marked < frames,
        "decimated audio should not look like a DoP carrier ({marked}/{frames} marked)"
    );
}

/// A rate that is not the carrier would be resampled, and a resampled DoP
/// stream is noise. The render refuses instead of writing it.
#[test]
fn an_explicit_rate_that_would_resample_the_carrier_is_refused() {
    let out = tmp("wrong-rate.pcm");
    let _ = std::fs::remove_file(&out);
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("render")
        .arg(testdata("tone.dsf"))
        .args(["--out"])
        .arg(&out)
        .args(["--dsd", "dop", "--rate", "48000"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("176400"));
}

/// Likewise a channel count that would remix it.
#[test]
fn an_explicit_channel_count_that_would_remix_the_carrier_is_refused() {
    let out = tmp("wrong-channels.pcm");
    let _ = std::fs::remove_file(&out);
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("render")
        .arg(testdata("tone.dsf"))
        .args(["--out"])
        .arg(&out)
        .args(["--dsd", "dop", "--channels", "1"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("2ch"));
}

/// Two DSD files at different rates cannot share one output format: the second
/// would be resampled to the first's carrier. Refused, not rendered.
#[test]
fn two_dsd_rates_in_one_dop_render_are_refused() {
    let out = tmp("mixed-rates.pcm");
    let _ = std::fs::remove_file(&out);
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("render")
        .arg(testdata("tone.dsf"))
        .arg(testdata("tone-dsd128.dsf"))
        .args(["--out"])
        .arg(&out)
        .args(["--dsd", "dop"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("one render has a single"));
}

/// `--dsd dop` on a file that is not DSD is a no-op: nothing to carry, so the
/// ordinary defaults apply and the render is the ordinary render.
#[test]
fn dop_on_a_non_dsd_input_changes_nothing() {
    let dop = tmp("flac-dop.pcm");
    let plain = tmp("flac-plain.pcm");
    for (out, extra) in [(&dop, vec!["--dsd", "dop"]), (&plain, vec![])] {
        Command::cargo_bin("soul-audio-cli")
            .unwrap()
            .arg("render")
            .arg(testdata("tone.flac"))
            .args(["--out"])
            .arg(out)
            .args(extra)
            .assert()
            .success();
    }
    assert_eq!(
        read_pcm(&dop),
        read_pcm(&plain),
        "--dsd dop must not change a PCM render"
    );
}
