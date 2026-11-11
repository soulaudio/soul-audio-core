//! Deterministic mutation fuzzer for the public decode surface.
//!
//! Invariant under test: `probe` / `open` / `open_seekable` / `decode_into` /
//! `seek_to` / `seek_to_time` / `total_frames` — plus the DSD
//! (`DsfDecoder`/`DffDecoder` + `PlaybackDecimateAdapter`/`DopAdapter`) and
//! Vorbis (arena) construct paths that `open` does not build — must **never
//! panic and never hang** on *any* byte input. The only allowed outcomes are an
//! `Err(Malformed/Unsupported/Io/UnexpectedEof)` or bounded `Ok` output.
//!
//! The crate is `#![forbid(unsafe_code)]`, so there is no UB to chase; the real
//! failure modes are debug-build panics (index-out-of-bounds, arithmetic
//! overflow, `unwrap`/`expect`, slice misuse) and non-terminating loops.
//!
//! How this differs from (and complements) `tests/robustness.rs`:
//!   * **Real wall-clock hang detection.** Each input is decoded on a worker
//!     thread and the driver waits with a timeout, so a *single* `decode_into`
//!     call that loops forever (consuming no input, which an iteration/call cap
//!     cannot catch) is flagged as a HANG instead of wedging the test runner.
//!   * **Structural mutations** the byte-flip sweep doesn't do: splicing two
//!     fixtures, duplicating a fixture, zeroing runs, and *targeted corruption
//!     of container length/size fields* (a huge box/atom/page/frame length is a
//!     classic source of `len as usize` index and multiply-overflow panics).
//!
//! Two modes:
//!   * **CI (default).** A fixed seed and a bounded iteration count
//!     ([`CI_ITERS`]); deterministic, green, and fast (well under 30 s). This is
//!     the mode that runs in the normal `cargo test` suite.
//!   * **Long (env-gated).** `FUZZ_ITERS=<n>` runs `n` iterations; or
//!     `FUZZ_SECONDS=<n>` runs for a wall-clock budget. `FUZZ_SEED=<hex|dec>`
//!     overrides the seed. For extended local sessions.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{
    Decoder, DopAdapter, DsdDecoder, DsdSeek, PlaybackDecimateAdapter, SeekableDecoder,
    SliceSource, open, open_seekable, probe,
};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Fixed seed for the deterministic CI run. Same seed → same inputs → the same
/// pass/fail, every time, on every machine.
const CI_SEED: u64 = 0xD1CE_F0FF_5EED_1234;

/// CI iteration count: generated-input count for the default `cargo test` run.
/// Chosen so the whole test stays well under 30 s in a debug build (≈20 s here;
/// decoder construction, not decode depth, dominates per-input cost) while still
/// exercising every strategy against every seed. The env-gated long mode
/// (`FUZZ_ITERS`/`FUZZ_SECONDS`) runs far more for real fuzzing.
const CI_ITERS: u64 = 220;

/// Per-input wall-clock budget. A bounded decode of a capped input finishes in
/// milliseconds even in a debug build, so this generous margin only ever trips
/// on a genuine non-terminating loop — never a slow-but-progressing decode
/// (which would make CI flaky). It is the hard "never hang" enforcement.
const HANG_TIMEOUT: Duration = Duration::from_secs(4);

/// Upper bound on samples drained from any one decoder instance. Bounds total
/// work so a "valid-looking huge" input cannot run forever (the task's "cap
/// total frames" requirement); a decoder that is making progress hits this and
/// stops cleanly (not a failure). 64 Ki samples is several frames of every
/// codec — enough to exercise the per-frame decode loops, lacing, and
/// cross-frame state — while keeping each iteration cheap (exhaustive
/// full-length decode is `tests/robustness.rs`'s job; this harness trades
/// per-input depth for mutation breadth).
const SAMPLE_CAP: usize = 1 << 16; // 65_536 interleaved samples

/// Largest generated input. Splices/duplications of big fixtures are truncated
/// to this so one iteration can't balloon the work.
const MAX_INPUT: usize = 192 * 1024;

/// Worker-thread stack. The by-value decoders are large (AnyDecoder's inline
/// FLAC planar buffers, the arena-backed VorbisDecoder, the WavPack block
/// buffer) and debug-build construction moves multiply that; 32 MiB matches the
/// headroom `tests/robustness.rs` uses so a deep construction can't overflow
/// (a false positive unrelated to the invariant).
const WORKER_STACK: usize = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Deterministic PRNG (xorshift64*, no external dev-dep; reproducible corpus)
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the zero fixed-point of xorshift.
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 33) as u8
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        // inclusive lo, exclusive hi; hi>lo assumed by callers.
        lo + self.below(hi - lo)
    }
    /// Pick an index into a non-empty slice.
    fn pick(&mut self, len: usize) -> usize {
        self.below(len.max(1))
    }
}

// ---------------------------------------------------------------------------
// Decode drivers — exercise every target, bounded, on the current thread.
// A shared `stage` atomic is advanced before each operation so that, when the
// worker thread hangs, the main thread can name the operation that wedged.
// ---------------------------------------------------------------------------

mod stage {
    pub const IDLE: usize = 0;
    pub const PROBE: usize = 1;
    pub const OPEN: usize = 2;
    pub const OPEN_DECODE: usize = 3;
    pub const OPEN_SEEK: usize = 4;
    pub const OPEN_SEEKABLE: usize = 5;
    pub const OPEN_SEEKABLE_DECODE: usize = 6;
    pub const DSF: usize = 7;
    pub const DFF: usize = 8;
    pub const DSF_PCM: usize = 9;
    pub const DFF_DOP: usize = 10;
    pub const VORBIS: usize = 11;

    pub fn name(s: usize) -> &'static str {
        match s {
            PROBE => "probe",
            OPEN => "open",
            OPEN_DECODE => "open→decode_into",
            OPEN_SEEK => "open→seek",
            OPEN_SEEKABLE => "open_seekable",
            OPEN_SEEKABLE_DECODE => "open_seekable→decode_into",
            DSF => "DsfDecoder",
            DFF => "DffDecoder",
            DSF_PCM => "Dsf→PlaybackDecimate",
            DFF_DOP => "Dff→DoP",
            VORBIS => "VorbisDecoder",
            _ => "idle",
        }
    }
}

/// Seek targets: start, a plausible mid, and overflow-prone extremes.
const SEEK_TARGETS: [u64; 4] = [0, 44_100, u64::MAX / 2, u64::MAX];

/// Drain a PCM decoder under the sample cap. Returns normally once it ends,
/// errors, or reaches the cap — all three are "bounded" (not a failure). A true
/// infinite loop never returns and is caught by the worker-thread timeout.
fn drain_pcm(dec: &mut dyn Decoder) {
    let mut buf = [0i32; 2048];
    let mut total = 0usize;
    // Call cap is a belt-and-suspenders backstop; the sample cap is the real
    // gate (every non-empty read adds ≥1 to `total`).
    for _ in 0..(SAMPLE_CAP + 16) {
        match dec.decode_into(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                total = total.saturating_add(n);
                if total >= SAMPLE_CAP {
                    return;
                }
            }
        }
    }
}

/// Drain a raw DSD reader under the (byte) cap.
fn drain_dsd(dec: &mut dyn DsdDecoder) {
    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    for _ in 0..(SAMPLE_CAP + 16) {
        match dec.read_dsd(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                total = total.saturating_add(n);
                if total >= SAMPLE_CAP {
                    return;
                }
            }
        }
    }
}

/// Short post-seek decode probe — a seek bug surfaces in the seek call itself
/// or the first reads after it, so there's no need to drain fully.
fn probe_after_seek(dec: &mut dyn Decoder) {
    let mut buf = [0i32; 512];
    for _ in 0..8 {
        match dec.decode_into(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// Run every target against `bytes`, advancing `stage` before each so a hang
/// can be attributed. Everything here must terminate for *any* input.
///
/// To keep each iteration cheap (so CI can run many mutations), seeks reuse one
/// decoder — seeking is an absolute reposition, so a fresh vs. mid-stream start
/// doesn't change whether the seek arithmetic over/underflows, which is the
/// class of bug this targets.
fn drive_one(bytes: &[u8], stage: &AtomicUsize) {
    // --- probe (consumes nothing; must classify or decline, never panic).
    stage.store(stage::PROBE, Ordering::Relaxed);
    let _ = probe(&mut SliceSource::new(bytes));

    // --- open → total_frames → seek surface → bounded decode (one decoder).
    stage.store(stage::OPEN, Ordering::Relaxed);
    if let Ok(mut dec) = open(SliceSource::new(bytes)) {
        let _ = dec.total_frames();
        dec.resolve_lengths();
        let _ = dec.total_frames();
        stage.store(stage::OPEN_SEEK, Ordering::Relaxed);
        for &t in &SEEK_TARGETS {
            let _ = dec.seek_to(t);
            probe_after_seek(&mut dec);
        }
        for &secs in &[0.0f64, -1.0, f64::NAN, 1.0e18, 123.456] {
            let _ = dec.seek_to_time(secs);
            probe_after_seek(&mut dec);
        }
        // Re-open for a clean, uninterrupted full decode (post-seek state is
        // valid, but a fresh decode exercises the straight-through path).
        stage.store(stage::OPEN_DECODE, Ordering::Relaxed);
        if let Ok(mut d2) = open(SliceSource::new(bytes)) {
            drain_pcm(&mut d2);
        }
    }

    // --- open_seekable (the seekable superset; the only path that builds a
    //     non-faststart MP4). One construct + decode + seeks.
    stage.store(stage::OPEN_SEEKABLE, Ordering::Relaxed);
    if let Ok(mut dec) = open_seekable(SliceSource::new(bytes)) {
        let _ = dec.total_frames();
        stage.store(stage::OPEN_SEEKABLE_DECODE, Ordering::Relaxed);
        drain_pcm(&mut dec);
        for &t in &SEEK_TARGETS {
            let _ = SeekableDecoder::seek_to(&mut dec, t);
            probe_after_seek(&mut dec);
        }
    }

    // --- DSD: raw readers + the PCM-producing playback adapters + DoP + seek.
    //     `open` never builds these (DSD needs an explicit mode), so they are
    //     fuzzed directly here.
    stage.store(stage::DSF, Ordering::Relaxed);
    if let Ok(mut d) = DsfDecoder::new(SliceSource::new(bytes)) {
        let _ = d.total_samples();
        drain_dsd(&mut d);
        for &t in &SEEK_TARGETS {
            let _ = d.seek_to_sample(t);
        }
    }
    stage.store(stage::DSF_PCM, Ordering::Relaxed);
    if let Ok(inner) = DsfDecoder::new(SliceSource::new(bytes)) {
        let mut a = PlaybackDecimateAdapter::new(inner);
        let _ = a.total_frames();
        for &t in &SEEK_TARGETS {
            let _ = a.seek_to_frame(t);
            probe_after_seek(&mut a);
        }
        drain_pcm(&mut a);
    }
    stage.store(stage::DFF, Ordering::Relaxed);
    if let Ok(mut d) = DffDecoder::new(SliceSource::new(bytes)) {
        drain_dsd(&mut d);
        for &t in &SEEK_TARGETS {
            let _ = d.seek_to_sample(t);
        }
    }
    stage.store(stage::DFF_DOP, Ordering::Relaxed);
    if let Ok(inner) = DffDecoder::new(SliceSource::new(bytes)) {
        let mut a = DopAdapter::new(inner);
        drain_pcm(&mut a);
    }

    // --- Vorbis: needs a caller arena, so `open` can't build it. Fuzzes the
    //     setup parse plus the audio-packet value decode (residue/floor/IMDCT).
    stage.store(stage::VORBIS, Ordering::Relaxed);
    if let Ok(mut d) = VorbisDecoder::new(SliceSource::new(bytes), vec![0u8; 128 * 1024]) {
        drain_pcm(&mut d);
        for &t in &SEEK_TARGETS {
            let _ = SeekableDecoder::seek_to(&mut d, t);
            probe_after_seek(&mut d);
        }
    }

    stage.store(stage::IDLE, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Panic capture
// ---------------------------------------------------------------------------

thread_local! {
    /// Source location of the most recent panic on *this* thread, recorded by
    /// the global hook and read back by the worker after `catch_unwind`.
    static LAST_LOC: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

static HOOK: Once = Once::new();

/// Install a quiet panic hook once: it records each panic's source location
/// (into thread-local storage on the panicking thread) and prints nothing — a
/// per-panic backtrace would swamp output and slow the sweep. `catch_unwind`
/// turns the panics into data points.
fn install_hook() {
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            LAST_LOC.with(|c| *c.borrow_mut() = loc);
        }));
    });
}

/// What the worker reports back for one input.
enum Outcome {
    Clean,
    Panic { loc: String, msg: String },
}

/// A persistent decode worker on a big stack. One worker handles every input in
/// the common (no-hang) case — cheap. If an input hangs, the main thread
/// abandons the stuck worker and spawns a fresh one.
struct Worker {
    tx: Sender<Arc<Vec<u8>>>,
    rx: Receiver<Outcome>,
    stage: Arc<AtomicUsize>,
}

fn spawn_worker() -> Worker {
    let (in_tx, in_rx) = channel::<Arc<Vec<u8>>>();
    let (out_tx, out_rx) = channel::<Outcome>();
    let stage = Arc::new(AtomicUsize::new(stage::IDLE));
    let wstage = Arc::clone(&stage);
    std::thread::Builder::new()
        .name("fuzz-worker".into())
        .stack_size(WORKER_STACK)
        .spawn(move || {
            while let Ok(bytes) = in_rx.recv() {
                wstage.store(stage::IDLE, Ordering::Relaxed);
                let st = &wstage;
                let outcome = match catch_unwind(AssertUnwindSafe(|| drive_one(&bytes, st))) {
                    Ok(()) => Outcome::Clean,
                    Err(payload) => {
                        let msg = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "?".to_string());
                        let loc = LAST_LOC.with(|c| c.borrow().clone());
                        Outcome::Panic { loc, msg }
                    }
                };
                // If the main thread gave up on us (hang), the receiver is gone;
                // just exit.
                if out_tx.send(outcome).is_err() {
                    return;
                }
            }
        })
        .expect("spawn fuzz worker");
    Worker {
        tx: in_tx,
        rx: out_rx,
        stage,
    }
}

// ---------------------------------------------------------------------------
// Failure records + reporting
// ---------------------------------------------------------------------------

struct Failure {
    iter: u64,
    strategy: &'static str,
    seed_name: &'static str,
    kind: String,
    detail: String,
    input: Arc<Vec<u8>>,
}

fn hex_prefix(bytes: &[u8], max: usize) -> String {
    let n = bytes.len().min(max);
    let mut s = String::with_capacity(n * 2 + 4);
    for b in &bytes[..n] {
        s.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > n {
        s.push('…');
    }
    s
}

// ---------------------------------------------------------------------------
// Seed corpus — valid fixtures spanning every container + codec, kept compact
// for CI speed, plus hand-built minimal WAV/FLAC and tiny/empty blobs.
// ---------------------------------------------------------------------------

fn minimal_wav() -> Vec<u8> {
    let data: [u8; 8] = [0, 0, 1, 0, 2, 0, 3, 0];
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&8000u32.to_le_bytes());
    v.extend_from_slice(&16000u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

fn minimal_flac() -> Vec<u8> {
    let mut si = [0u8; 34];
    let v: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36);
    si[10..18].copy_from_slice(&v.to_be_bytes());
    let mut b = Vec::new();
    b.extend_from_slice(b"fLaC");
    b.push(0x80);
    b.extend_from_slice(&[0, 0, 34]);
    b.extend_from_slice(&si);
    b
}

/// The valid-fixture seeds. Compact variants chosen per format so the CI pass
/// stays fast; every container + codec the decode surface accepts is present.
/// A truncated DSF prefix stands in for the (large) full `.dsf` so the DSF
/// header/parse path is seeded without a 360 KiB fixture.
fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let dsf_full = include_bytes!("../../../testdata/tone.dsf");
    vec![
        ("empty", Vec::new()),
        ("one-byte", vec![0xFF]),
        ("wav-min", minimal_wav()),
        ("flac-min", minimal_flac()),
        (
            "adpcm-mono",
            include_bytes!("../testdata/ima-adpcm-mono.wav").to_vec(),
        ),
        (
            "l2-mono",
            include_bytes!("../testdata/l2_m44_128.mp2").to_vec(),
        ),
        (
            "l1",
            include_bytes!("../testdata/layer1-32k-stereo.mp1").to_vec(),
        ),
        (
            "mp3-vbr",
            include_bytes!("../testdata/vbr-44100-stereo.mp3").to_vec(),
        ),
        (
            "ogg-flac",
            include_bytes!("../testdata/flac-in-ogg.ogg").to_vec(),
        ),
        (
            "flac",
            include_bytes!("../../../testdata/tone.flac").to_vec(),
        ),
        (
            "aiff",
            include_bytes!("../../../testdata/tone-aiff.aiff").to_vec(),
        ),
        ("alac", include_bytes!("../testdata/alac.m4a").to_vec()),
        ("aac-m4a", include_bytes!("../testdata/aac.m4a").to_vec()),
        ("aac-adts", include_bytes!("../testdata/aac.aac").to_vec()),
        ("sbr-adts", include_bytes!("../testdata/sbr.aac").to_vec()),
        (
            "frag-aac",
            include_bytes!("../testdata/frag-aac.m4a").to_vec(),
        ),
        (
            "opus-silk",
            include_bytes!("../testdata/voice-silk-mono.opus").to_vec(),
        ),
        (
            "opus-hybrid",
            include_bytes!("../testdata/hybrid-stereo.opus").to_vec(),
        ),
        (
            "opus-5p1",
            include_bytes!("../testdata/opus-5p1.opus").to_vec(),
        ),
        (
            "vorbis-mono",
            include_bytes!("../testdata/tone-vorbis-mono.ogg").to_vec(),
        ),
        (
            "vorbis-blocks",
            include_bytes!("../testdata/tone-vorbis-blocks.ogg").to_vec(),
        ),
        (
            "wavpack",
            include_bytes!("../../../testdata/tone-mono.wv").to_vec(),
        ),
        (
            "caf-alac",
            include_bytes!("../../../testdata/tone-alac.caf").to_vec(),
        ),
        (
            "caf-lpcm",
            include_bytes!("../../../testdata/tone-caf-le.caf").to_vec(),
        ),
        (
            "mkv-opus",
            include_bytes!("../../../testdata/tone-opus.webm").to_vec(),
        ),
        (
            "mkv-flac",
            include_bytes!("../../../testdata/tone-flac.mka").to_vec(),
        ),
        (
            "mkv-aac",
            include_bytes!("../../../testdata/tone-aac.mka").to_vec(),
        ),
        (
            "mkv-vorbis",
            include_bytes!("../../../testdata/tone-vorbis.mka").to_vec(),
        ),
        (
            "dff",
            include_bytes!("../testdata/dst-64fs44-2ch.slice.dff").to_vec(),
        ),
        // A truncated DSF prefix (header + a little data) — seeds the DSF parse
        // path without carrying the full 360 KiB fixture into the corpus.
        ("dsf-head", dsf_full[..dsf_full.len().min(8192)].to_vec()),
    ]
}

/// Container magics — random/structured bodies behind these drive the parsers
/// past their sync checks into the length-driven parse code.
const MAGICS: &[&[u8]] = &[
    b"RIFF\x24\x00\x00\x00WAVEfmt ",
    b"RF64\xff\xff\xff\xffWAVEfmt ",
    b"fLaC",
    b"OggS\x00\x02",
    b"OggS\x00\x02____________________________\x7fFLAC",
    b"OggS\x00\x02____________________________OpusHead",
    b"OggS\x00\x02____________________________\x01vorbis",
    b"DSD ",
    b"FRM8",
    b"ID3\x04\x00\x00",
    b"\xff\xfb\x90\x00",
    b"\xff\xfd\x80\x04",
    b"\xff\xfe\xc8\x04",
    b"\xff\xf3\x00\x00",
    b"\x00\x00\x00\x18ftypM4A ",
    b"\xff\xf1\x50\x80",
    b"\xff\xf9\x50\x80",
    b"FORM\x00\x00\x00\x00AIFF",
    b"FORM\x00\x00\x00\x00AIFC",
    b"wvpk",
    b"caff\x00\x01\x00\x00",
    b"\x1a\x45\xdf\xa3",
];

// ---------------------------------------------------------------------------
// Mutation strategies. Each draws all its randomness from the single seeded
// RNG, so the whole input sequence is a pure function of (seed, iteration).
// ---------------------------------------------------------------------------

fn clamp_len(mut v: Vec<u8>) -> Vec<u8> {
    if v.len() > MAX_INPUT {
        v.truncate(MAX_INPUT);
    }
    v
}

/// Overwrite a field at a random offset with a hostile length-ish value, in a
/// random width (2/4/8) and endianness. This is where "attacker-controlled
/// length" panics hide: a huge box/atom/page/frame size fed into `len as usize`
/// slicing or `a - b` offset math.
fn corrupt_length_field(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.len() < 4 {
        return v;
    }
    let widths = [2usize, 4, 8];
    let values: [u64; 7] = [
        0,
        1,
        0x7FFF_FFFF,
        0xFFFF_FFFF,
        0xFFFF_FFFF_FFFF_FFFF,
        0x8000_0000,
        u64::from(rng.byte()) << 24 | 0x00FF_FFFF,
    ];
    // A few independent corruptions per mutant — real files have many length
    // fields and the interesting bugs often need the *right* one hit.
    for _ in 0..rng.range(1, 4) {
        let w = widths[rng.below(widths.len())];
        if v.len() < w {
            continue;
        }
        let pos = rng.below(v.len() - w + 1);
        let val = values[rng.below(values.len())];
        let big_endian = rng.below(2) == 0;
        let bytes = if big_endian {
            val.to_be_bytes()
        } else {
            val.to_le_bytes()
        };
        // Take the low `w` bytes of the chosen value in the chosen endianness.
        let src = if big_endian {
            &bytes[8 - w..]
        } else {
            &bytes[..w]
        };
        v[pos..pos + w].copy_from_slice(src);
    }
    v
}

fn bit_flips(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.is_empty() {
        return v;
    }
    for _ in 0..rng.range(1, 24) {
        let pos = rng.below(v.len());
        v[pos] ^= 1 << rng.below(8);
    }
    v
}

fn byte_subst(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.is_empty() {
        return v;
    }
    for _ in 0..rng.range(1, 24) {
        let pos = rng.below(v.len());
        v[pos] = rng.byte();
    }
    v
}

fn truncate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let n = rng.below(data.len() + 1);
    data[..n].to_vec()
}

fn zero_run(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.is_empty() {
        return v;
    }
    let start = rng.below(v.len());
    let len = rng.range(1, (v.len() - start).max(1) + 1);
    for b in &mut v[start..start + len] {
        *b = 0;
    }
    v
}

fn splice(rng: &mut Rng, a: &[u8], b: &[u8]) -> Vec<u8> {
    let cut_a = rng.below(a.len() + 1);
    let cut_b = rng.below(b.len() + 1);
    let mut v = a[..cut_a].to_vec();
    v.extend_from_slice(&b[cut_b..]);
    clamp_len(v)
}

fn duplicate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    let take = rng.below(data.len() + 1);
    v.extend_from_slice(&data[..take]);
    clamp_len(v)
}

fn random_blob(rng: &mut Rng, max: usize) -> Vec<u8> {
    let n = rng.below(max);
    (0..n).map(|_| rng.byte()).collect()
}

fn magic_prefixed(rng: &mut Rng) -> Vec<u8> {
    let magic = MAGICS[rng.below(MAGICS.len())];
    let body = rng.range(4, 2048);
    let mut v = magic.to_vec();
    // Mostly random, but sprinkle in hostile length values to drive the
    // length-sensitive parse code behind a valid header.
    for _ in 0..body {
        v.push(rng.byte());
    }
    // Overlay a couple of length-field corruptions on the body.
    corrupt_length_field(rng, &v)
}

/// The strategy names, used in failure reports.
const STRATEGIES: &[&str] = &[
    "random-blob",
    "tiny",
    "bit-flips",
    "byte-subst",
    "truncate",
    "zero-run",
    "splice",
    "duplicate",
    "length-field",
    "magic-prefixed",
];

/// Produce one input deterministically for iteration `i`. Returns the strategy
/// name, the originating seed name, and the bytes.
fn generate(
    rng: &mut Rng,
    corpus: &[(&'static str, Vec<u8>)],
) -> (&'static str, &'static str, Vec<u8>) {
    let s = rng.below(STRATEGIES.len());
    let strat = STRATEGIES[s];
    match s {
        0 => ("random-blob", "random", random_blob(rng, 4096)),
        1 => ("tiny", "random", random_blob(rng, 16)),
        2 => {
            let (name, d) = &corpus[rng.pick(corpus.len())];
            (strat, name, bit_flips(rng, d))
        }
        3 => {
            let (name, d) = &corpus[rng.pick(corpus.len())];
            (strat, name, byte_subst(rng, d))
        }
        4 => {
            let (name, d) = &corpus[rng.pick(corpus.len())];
            (strat, name, truncate(rng, d))
        }
        5 => {
            let (name, d) = &corpus[rng.pick(corpus.len())];
            (strat, name, zero_run(rng, d))
        }
        6 => {
            let (na, a) = &corpus[rng.pick(corpus.len())];
            let (_nb, b) = &corpus[rng.pick(corpus.len())];
            (strat, na, splice(rng, a, b))
        }
        7 => {
            let (name, d) = &corpus[rng.pick(corpus.len())];
            (strat, name, duplicate(rng, d))
        }
        8 => {
            let (name, d) = &corpus[rng.pick(corpus.len())];
            (strat, name, corrupt_length_field(rng, d))
        }
        _ => ("magic-prefixed", "magic", magic_prefixed(rng)),
    }
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

struct Config {
    seed: u64,
    iters: u64,
    budget: Option<Duration>,
}

fn config_from_env() -> Config {
    let seed = std::env::var("FUZZ_SEED")
        .ok()
        .and_then(|s| {
            let t = s.trim();
            if let Some(hex) = t.strip_prefix("0x") {
                u64::from_str_radix(hex, 16).ok()
            } else {
                t.parse::<u64>().ok()
            }
        })
        .unwrap_or(CI_SEED);
    let env_iters = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let budget = std::env::var("FUZZ_SECONDS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    // Long mode when either env knob is set; else the fixed CI budget.
    let iters = env_iters.unwrap_or(if budget.is_some() { u64::MAX } else { CI_ITERS });
    Config {
        seed,
        iters,
        budget,
    }
}

fn run(cfg: &Config) -> Vec<Failure> {
    install_hook();
    let corpus = seeds();
    let mut rng = Rng::new(cfg.seed);
    let mut failures: Vec<Failure> = Vec::new();
    let mut worker = spawn_worker();
    let start = Instant::now();

    for i in 0..cfg.iters {
        if let Some(b) = cfg.budget {
            if start.elapsed() >= b {
                break;
            }
        }
        let (strat, seed_name, bytes) = generate(&mut rng, &corpus);
        let input = Arc::new(bytes);

        worker.stage.store(stage::IDLE, Ordering::Relaxed);
        if worker.tx.send(Arc::clone(&input)).is_err() {
            // Worker died unexpectedly — respawn and retry this input once.
            worker = spawn_worker();
            if worker.tx.send(Arc::clone(&input)).is_err() {
                break;
            }
        }

        match worker.rx.recv_timeout(HANG_TIMEOUT) {
            Ok(Outcome::Clean) => {}
            Ok(Outcome::Panic { loc, msg }) => failures.push(Failure {
                iter: i,
                strategy: strat,
                seed_name,
                kind: "PANIC".into(),
                detail: format!("{loc}: {msg}"),
                input,
            }),
            Err(RecvTimeoutError::Timeout) => {
                // A genuine non-terminating loop. The worker is wedged; abandon
                // it, record the hang (with the stage it wedged in), and stop —
                // a spinning thread would skew the rest of the run.
                let st = worker.stage.load(Ordering::Relaxed);
                failures.push(Failure {
                    iter: i,
                    strategy: strat,
                    seed_name,
                    kind: "HANG".into(),
                    detail: format!(
                        "no progress in {} within {:?}",
                        stage::name(st),
                        HANG_TIMEOUT
                    ),
                    input,
                });
                break;
            }
            Err(RecvTimeoutError::Disconnected) => {
                worker = spawn_worker();
            }
        }
    }
    failures
}

fn report(failures: &[Failure], iters_done: u64) -> String {
    let mut msg = format!(
        "{} malformed input(s) panicked/hung across {} iteration(s):\n",
        failures.len(),
        iters_done
    );
    for f in failures.iter().take(30) {
        msg.push_str(&format!(
            "  iter={} [{} seed={}] {} — {}\n    len={} input={}\n",
            f.iter,
            f.strategy,
            f.seed_name,
            f.kind,
            f.detail,
            f.input.len(),
            hex_prefix(&f.input, 128),
        ));
    }
    msg
}

/// Deterministic CI fuzz: a fixed seed and a bounded iteration count. Must stay
/// green, fast, and reproducible. Env knobs (`FUZZ_ITERS`/`FUZZ_SECONDS`/
/// `FUZZ_SEED`) switch it into the extended local mode.
#[test]
fn fuzz_decode_surface_never_panics_or_hangs() {
    let cfg = config_from_env();
    let failures = run(&cfg);
    if !failures.is_empty() {
        // The quiet panic hook installed for the sweep would otherwise swallow
        // the assert's own message, so print the report explicitly first.
        let r = report(&failures, cfg.iters);
        eprintln!("\n{r}");
        panic!("fuzz found {} crash(es) — see report above", failures.len());
    }
}

// ---------------------------------------------------------------------------
// Determinism guard: the same seed must generate the identical input sequence.
// ---------------------------------------------------------------------------

#[test]
fn generator_is_deterministic() {
    let corpus = seeds();
    let fingerprint = |seed: u64| -> Vec<(usize, u64)> {
        let mut rng = Rng::new(seed);
        (0..256)
            .map(|_| {
                let (_s, _n, bytes) = generate(&mut rng, &corpus);
                // (length, a cheap content hash) uniquely fingerprints the input.
                let mut h = 0xcbf2_9ce4_8422_2325u64;
                for b in &bytes {
                    h ^= u64::from(*b);
                    h = h.wrapping_mul(0x0000_0100_0000_01B3);
                }
                (bytes.len(), h)
            })
            .collect()
    };
    assert_eq!(
        fingerprint(CI_SEED),
        fingerprint(CI_SEED),
        "same seed → same inputs"
    );
    assert_ne!(
        fingerprint(CI_SEED),
        fingerprint(CI_SEED ^ 0x9E37_79B9_7F4A_7C15),
        "different seed → different inputs"
    );
}
