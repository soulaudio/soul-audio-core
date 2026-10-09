//! I/O-error path hardening.
//!
//! A real [`Source`](soul_audio_core::Source) can fail with a *hard* error
//! mid-stream (a disk read fault, a dropped network connection) — not just end
//! cleanly at EOF. The [`SliceSource`](soul_audio_core::SliceSource)-based
//! `robustness` sweep only ever hits the clean `Ok(0)` EOF, so the
//! [`DecodeError::Io`] path — every `src.read(..).map_err(|_| DecodeError::Io)`
//! site across the demuxers and codecs — was never actually exercised (flagged
//! as a known test gap).
//!
//! This file drives every decoder over [`ErrAt`], a source that serves a
//! fixture's first `fail_at` bytes and then returns `Err` on every subsequent
//! read, and asserts the contract: a failing reader always degrades to a clean
//! `Err` — never a panic, a hang, or an out-of-bounds. It also pins the two
//! crisp invariants: a reader that fails on the first byte surfaces as
//! `Err(Io)` from `open()` (the probe path), and a streaming decode that reads
//! to a mid-stream fault surfaces `Err(Io)` from `decode_into` (the decode-loop
//! path, distinct from the probe).

use std::panic::{AssertUnwindSafe, catch_unwind};

use embedded_io::{ErrorType, Read};
use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{BufReader, DecodeError, Decoder, DsdDecoder, open};

/// A `Source` that serves `data[..fail_at]` and then returns a hard I/O error
/// on every read thereafter — a stream that dies mid-transfer instead of
/// ending at EOF (which a `SliceSource` models as the clean `Ok(0)`).
struct ErrAt {
    data: &'static [u8],
    pos: usize,
    fail_at: usize,
}

impl ErrAt {
    fn new(data: &'static [u8], fail_at: usize) -> Self {
        Self {
            data,
            pos: 0,
            // Clamp so once the served bytes run out we always *error* rather
            // than read out of bounds: `fail_at == data.len()` serves the whole
            // fixture and then faults where a real reader would report EOF.
            fail_at: fail_at.min(data.len()),
        }
    }
}

impl ErrorType for ErrAt {
    type Error = embedded_io::ErrorKind;
}

impl Read for ErrAt {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if self.pos >= self.fail_at {
            return Err(embedded_io::ErrorKind::Other);
        }
        let n = buf.len().min(self.fail_at - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Run `f` on a 16 MiB stack (the by-value decoders — notably `AnyDecoder` and
/// `VorbisDecoder` — exceed the default test-thread stack) and catch any panic,
/// so a decoder that *panics* on a failing reader is reported as a test failure
/// rather than aborting the process. Returns `Err` iff `f` panicked.
fn no_panic<R: Send>(f: impl FnOnce() -> R + Send) -> std::thread::Result<R> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(s, || catch_unwind(AssertUnwindSafe(f)))
            .expect("spawn io-error thread")
            .join()
            .expect("io-error thread join")
    })
}

/// Bounded PCM drain (the loop cap is the hang guard: a decoder that neither
/// progresses, ends, nor errors would otherwise spin forever).
fn drain_pcm(dec: &mut impl Decoder) -> Result<(), DecodeError> {
    let mut buf = [0i32; 4096];
    for _ in 0..1_000_000 {
        if dec.decode_into(&mut buf)? == 0 {
            return Ok(());
        }
    }
    Ok(())
}

/// Full `open()` dispatch + drain over a reader that fails at byte `fail_at`.
fn open_drive(data: &'static [u8], fail_at: usize) -> Result<(), DecodeError> {
    let mut scratch = [0u8; 8192];
    let mut dec = open(BufReader::new(ErrAt::new(data, fail_at), &mut scratch))?;
    drain_pcm(&mut dec)
}

/// Vorbis is consumer-dispatched (needs a caller arena), so it bypasses `open`.
fn vorbis_drive(data: &'static [u8], fail_at: usize) -> Result<(), DecodeError> {
    let mut dec = VorbisDecoder::new(ErrAt::new(data, fail_at), vec![0u8; 128 * 1024])?;
    drain_pcm(&mut dec)
}

/// DSD is consumer-dispatched (needs an explicit mode), so it bypasses `open`;
/// drive its native `read_dsd` surface rather than PCM.
fn dsd_drive(data: &'static [u8], fail_at: usize) -> Result<(), DecodeError> {
    let mut dec = DffDecoder::new(ErrAt::new(data, fail_at))?;
    let mut buf = [0u8; 4096];
    for _ in 0..1_000_000 {
        if dec.read_dsd(&mut buf)? == 0 {
            return Ok(());
        }
    }
    Ok(())
}

/// No read-fault position may panic or hang the drive; a clean `Ok`/`Err` is
/// the whole contract. Checks the fault at the start, three interior points,
/// and exactly at EOF (where a real reader would instead report `Ok(0)`).
fn assert_graceful_at_all_faults(
    label: &str,
    data: &'static [u8],
    drive: fn(&'static [u8], usize) -> Result<(), DecodeError>,
) {
    let len = data.len();
    for &fa in &[0usize, len / 4, len / 2, (len * 3) / 4, len] {
        if let Err(_panic) = no_panic(move || drive(data, fa)) {
            panic!("{label}: decoder PANICKED on a reader that fails at byte {fa} of {len}");
        }
    }
}

/// Formats `open()` routes directly (its dispatch produces a working decoder).
const OPEN_FIXTURES: &[(&str, &[u8])] = &[
    ("wav", include_bytes!("../../../testdata/tone.wav")),
    ("flac", include_bytes!("../../../testdata/tone.flac")),
    ("flac-in-ogg", include_bytes!("../testdata/flac-in-ogg.ogg")),
    ("mp3", include_bytes!("../testdata/vbr-44100-stereo.mp3")),
    ("mp2", include_bytes!("../testdata/l2_s44_128.mp2")),
    ("mp1", include_bytes!("../testdata/layer1-32k-stereo.mp1")),
    ("alac", include_bytes!("../testdata/alac.m4a")),
    ("aac-mp4", include_bytes!("../testdata/aac.m4a")),
    ("aac-frag-mp4", include_bytes!("../testdata/frag-aac.m4a")),
    ("he-aac-mp4", include_bytes!("../testdata/sbr.m4a")),
    ("aac-adts", include_bytes!("../testdata/aac.aac")),
    ("opus", include_bytes!("../../../testdata/tone.opus")),
];

#[test]
fn open_routed_decoders_handle_io_errors_gracefully() {
    for &(label, data) in OPEN_FIXTURES {
        assert_graceful_at_all_faults(label, data, open_drive);

        // Crisp probe-path invariant: a reader that fails on the very first
        // byte is reported by `open()` as `Err(Io)` (probe maps a `fill_buf`
        // fault to `Io`) — never `Unsupported`/`Malformed`, which would imply
        // we misread a fault as a format decision.
        match no_panic(move || open_drive(data, 0)) {
            Ok(Err(DecodeError::Io)) => {}
            other => panic!(
                "{label}: open() over an always-failing reader must be Err(Io), got {other:?}"
            ),
        }
    }
}

#[test]
fn vorbis_handles_io_errors_gracefully() {
    let data: &[u8] = include_bytes!("../testdata/tone-vorbis-mono.ogg");
    assert_graceful_at_all_faults("vorbis", data, vorbis_drive);
    // First-byte fault → a clean construction error, not a panic.
    assert!(
        matches!(no_panic(move || vorbis_drive(data, 0)), Ok(Err(_))),
        "vorbis: construction over an always-failing reader must be a clean Err"
    );
}

#[test]
fn dsd_handles_io_errors_gracefully() {
    let data: &[u8] = include_bytes!("../testdata/dst-64fs44-2ch.slice.dff");
    assert_graceful_at_all_faults("dff", data, dsd_drive);
    assert!(
        matches!(no_panic(move || dsd_drive(data, 0)), Ok(Err(_))),
        "dff: construction over an always-failing reader must be a clean Err"
    );
}

/// The decode-*loop* I/O path, distinct from the probe: a streaming decoder
/// (ADTS AAC has no declared length, so it reads frames until the stream ends)
/// driven over a reader that serves the whole file and then faults where EOF
/// would be must surface that fault as `Err(Io)` from `decode_into` — not a
/// silent clean stop (which would mean a real read fault is indistinguishable
/// from a complete stream).
#[test]
fn decode_loop_surfaces_io_error_not_just_clean_eof() {
    let data: &[u8] = include_bytes!("../testdata/aac.aac");
    match no_panic(move || open_drive(data, data.len())) {
        Ok(Err(DecodeError::Io)) => {}
        other => panic!("ADTS decode over a fault-at-EOF reader must be Err(Io), got {other:?}"),
    }
}
