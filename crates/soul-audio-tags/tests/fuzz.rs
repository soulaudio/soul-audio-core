//! Deterministic mutation fuzzer for the **tag-reading surface**.
//!
//! Invariant under test: [`soul_audio_tags::read`] — the single entry point a
//! library scan calls for every file it finds — must **never panic and never
//! hang** on *any* byte input, for *any* scratch size. The only allowed
//! outcomes are `Err(Malformed/Unsupported/Io)` or a bounded `Ok` walk.
//!
//! Why this surface deserves its own fuzzer: a tag reader is the *first* code
//! to touch an untrusted file, it runs over the user's entire library (tens of
//! thousands of files, including whatever a stranger put in a torrent), and it
//! is almost entirely length-field arithmetic — synchsafe ID3v2 sizes, MP4 atom
//! sizes, Ogg page/segment tables, FLAC metadata-block lengths, RIFF/AIFF/CAF
//! chunk sizes, Matroska EBML var-ints. That is the same shape of code that
//! produced the overflow panics found in the decode paths (#150, #151, and the
//! CAF/AIFF depth guards), and before this file there was no fuzz gate on it at
//! all — `tests/differential.rs` compares *correct* output against lofty, which
//! says nothing about hostile input.
//!
//! The crate is `#![forbid(unsafe_code)]`, so there is no UB to chase; the real
//! failure modes are debug-build panics (index-out-of-bounds, arithmetic
//! overflow, `unwrap`/`expect`) and non-terminating loops — the latter very
//! much live here, because several of these formats are *chains* (Ogg page →
//! next page, MP4 atom → child atom, EBML element → child element) where a
//! corrupted size of `0` is the classic "advance by nothing, forever" bug.
//!
//! Mirrors `soul-audio-core/tests/fuzz.rs` deliberately — same PRNG, same
//! strategies, same worker-thread hang detection, same two modes:
//!   * **CI (default).** Fixed seed, bounded iterations ([`CI_ITERS`]) —
//!     deterministic and fast, part of the normal `cargo test` run.
//!   * **Long (env-gated).** `TAG_FUZZ_ITERS=<n>` or `TAG_FUZZ_SECONDS=<n>`,
//!     with `TAG_FUZZ_SEED=<hex|dec>` for another corpus.
//!
//! Note it runs on the crate's **default (no_std, no-alloc) features**: `read`
//! with a caller-provided scratch buffer is exactly what the DAP firmware
//! calls, so this fuzzes the embedded path, not just the desktop one.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use soul_audio_core::SliceSource;
use soul_audio_tags::{Field, Picture, StreamFacts, TagSink};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Fixed seed for the deterministic CI run.
const CI_SEED: u64 = 0x7A61_5F46_5A5A_0001;

/// CI iteration count. A tag walk is far cheaper than a decode (no codec state,
/// no sample output), so this affords many more inputs per second than the
/// decode fuzzer's budget — but each input is driven at five scratch sizes, so
/// keep the product in mind. Comfortably under 10 s in a debug build.
const CI_ITERS: u64 = 3_000;

/// Per-input wall-clock budget, across all scratch sizes. A bounded walk of a
/// capped input is milliseconds even in a debug build, so this only trips on a
/// genuine non-terminating loop.
const HANG_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest generated input (splices/duplications are clamped to it).
const MAX_INPUT: usize = 96 * 1024;

/// Seed fixtures are truncated to this. Tag data and container headers live at
/// the *head* of every one of these formats (Ogg's comment packet is in the
/// first pages, MP4's `moov` is at the front in all our fixtures), so the tail
/// is audio payload the tag reader skips over — keeping seeds small is pure
/// mutation throughput.
const MAX_SEED: usize = 48 * 1024;

/// Scratch sizes every input is driven at. `read` writes one field value at a
/// time into this buffer, so the small sizes exercise the
/// value-longer-than-scratch paths (truncate/chunk/skip) that a comfortable
/// 4 KiB buffer never reaches — and `1` is the degenerate case.
const SCRATCH_SIZES: [usize; 5] = [1, 8, 64, 256, 4096];

/// Worker-thread stack: generous, matching the decode fuzzer's rationale (the
/// readers themselves are shallow, but a debug build's frames are not).
const WORKER_STACK: usize = 16 * 1024 * 1024;

/// Cap on fields/pictures visited per walk, so a "valid-looking huge" input
/// cannot run forever while still making progress.
const VISIT_CAP: usize = 100_000;

// ---------------------------------------------------------------------------
// Deterministic PRNG (xorshift64*) — same as the decode fuzzer's.
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        // SplitMix64 the caller's seed into the xorshift state. The usual
        // `seed | 1` (which only dodged xorshift's zero fixed point) quietly
        // made every even/odd pair the SAME corpus — seeds 2 and 3 explore
        // identical inputs — and the weekly CI job seeds from the run number,
        // so every other run would have re-explored the previous run's corpus.
        // Mixing first means consecutive seeds are unrelated; the final `| 1`
        // still avoids the zero state, costing one bit of a well-mixed value.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(z | 1)
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
        lo + self.below(hi - lo)
    }
    fn pick(&mut self, len: usize) -> usize {
        self.below(len.max(1))
    }
}

// ---------------------------------------------------------------------------
// The sink: counts what it is handed, and bounds the walk.
// ---------------------------------------------------------------------------

/// Collects nothing (values borrow internal scratch) but counts everything, and
/// records the picture descriptors so the caller can sanity-check them.
#[derive(Default)]
struct CountingSink {
    texts: usize,
    pictures: Vec<Picture>,
}

impl TagSink for CountingSink {
    fn text(&mut self, _field: Option<Field>, raw_key: &str, value: &str) {
        // Touch both strings: they borrow the reader's scratch, and a reader
        // that handed out a bad slice would show up here, not in the counts.
        let _ = raw_key.len() + value.len();
        let _ = raw_key.chars().count() + value.chars().count();
        self.texts += 1;
        assert!(self.texts < VISIT_CAP, "runaway field count");
    }
    fn picture(&mut self, pic: &Picture) {
        if self.pictures.len() < 64 {
            self.pictures.push(*pic);
        }
        assert!(self.pictures.len() < VISIT_CAP, "runaway picture count");
    }
}

// ---------------------------------------------------------------------------
// Seed corpus: the real fixtures, one per tag-bearing container.
// ---------------------------------------------------------------------------

/// Container magics, for the "valid header then garbage" strategy — the shape
/// that drives a reader past its sniff and into the length arithmetic.
///
/// The size fields are **plausible**, not filler: a parser that bounds its walk
/// by the container's declared size bails immediately on an absurd one, so a
/// `0x2E2E2E2E` ("....") length would keep every mutation in the shallows. With
/// a believable size the reader proceeds into the chunk/atom/block loop, which
/// is the code worth fuzzing — and `corrupt_length_field` still rewrites these
/// bytes on most inputs anyway.
const MAGICS: &[&[u8]] = &[
    b"fLaC",
    b"ID3\x03\x00",
    b"ID3\x04\x00",
    b"RIFF\x80\x01\x00\x00WAVEfmt \x10\x00\x00\x00",
    b"FORM\x00\x00\x01\x80AIFFCOMM\x00\x00\x00\x12",
    b"FORM\x00\x00\x01\x80AIFCCOMM\x00\x00\x00\x16",
    b"DSD ",
    b"FRM8",
    b"\xFF\xF1",                                 // ADTS
    b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00", // MP4
    b"OggS\x00\x02",                             // Ogg
    b"wvpk",                                     // WavPack
    b"caff\x00\x01\x00\x00",                     // CAF (version 1, flags 0)
    b"\x1A\x45\xDF\xA3",                         // Matroska/EBML
];

fn workspace_testdata() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/soul-audio-tags -> workspace root")
        .join("testdata")
}

/// One fixture per container the reader dispatches to, so every parser in
/// `read`'s match is reachable by mutation. Missing files are skipped (the
/// corpus is checked for breadth by `corpus_covers_every_container`).
const FIXTURES: &[&str] = &[
    "tagged.flac",    // FLAC + VorbisComment + PICTURE
    "arted.flac",     // FLAC with cover art
    "realmusic.mp3",  // ID3v2
    "rva2-only.mp3",  // ID3v2.4 RVA2 frames
    "complex.wav",    // RIFF
    "tone-aiff.aiff", // AIFF
    "tone-aifc-sowt.aifc",
    "tone.dsf", // DSF (embeds ID3v2)
    "tone.dff", // DFF
    "tone.aac", // ADTS
    "tone.m4a", // MP4 ilst
    "realmusic-alac.m4a",
    "tone.ogg",      // Ogg Vorbis comments
    "tone.opus",     // Ogg Opus comments
    "tone.wv",       // WavPack
    "tone.caf",      // CAF
    "tone-flac.mka", // Matroska tags
    "tone-opus.webm",
];

fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let dir = workspace_testdata();
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for name in FIXTURES {
        if let Ok(mut bytes) = std::fs::read(dir.join(name)) {
            bytes.truncate(MAX_SEED);
            out.push((name, bytes));
        }
    }
    // Synthetic minimal seeds: a bare magic, and a magic with a plausible but
    // empty body — the shapes a mutation can grow a hostile header from.
    out.push((
        "magic-flac",
        b"fLaC\x00\x00\x00\x04\x00\x00\x00\x00".to_vec(),
    ));
    out.push(("magic-id3", b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec()));
    out.push(("empty", Vec::new()));
    out
}

// ---------------------------------------------------------------------------
// Mutations — the decode fuzzer's strategy set.
// ---------------------------------------------------------------------------

fn clamp_len(mut v: Vec<u8>) -> Vec<u8> {
    v.truncate(MAX_INPUT);
    v
}

/// Overwrite 2/4/8-byte windows with hostile values (`0xFF..`, `0x7FFF_FFFF`,
/// `0`, `u32::MAX - 1`) in both endiannesses. Every container here encodes its
/// structure as such a field, so this is the highest-yield strategy: a huge
/// length drives `len as usize` indexing and multiply overflow, and a **zero**
/// length drives the advance-by-nothing loops.
fn corrupt_length_field(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.len() < 8 {
        return v;
    }
    for _ in 0..rng.range(1, 5) {
        let width = [2usize, 4, 8][rng.below(3)];
        if v.len() <= width {
            break;
        }
        let at = rng.below(v.len() - width);
        let pattern: u64 = match rng.below(6) {
            0 => u64::MAX,
            1 => 0,
            2 => 0x7FFF_FFFF,
            3 => 0xFFFF_FFFE,
            4 => 1,
            _ => rng.next(),
        };
        let bytes = pattern.to_be_bytes();
        let src = &bytes[8 - width..];
        if rng.below(2) == 0 {
            v[at..at + width].copy_from_slice(src);
        } else {
            // Little-endian variant (RIFF/CAF/MP4 differ in endianness).
            for (i, b) in src.iter().rev().enumerate() {
                v[at + i] = *b;
            }
        }
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

fn truncate_at(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    data[..rng.below(data.len() + 1)].to_vec()
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

/// A valid container magic followed by hostile bytes — gets past the sniff and
/// into the parser proper, which random blobs almost never do.
fn magic_prefixed(rng: &mut Rng) -> Vec<u8> {
    let magic = MAGICS[rng.below(MAGICS.len())];
    let mut v = magic.to_vec();
    for _ in 0..rng.range(4, 2048) {
        v.push(rng.byte());
    }
    corrupt_length_field(rng, &v)
}

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

fn generate(
    rng: &mut Rng,
    corpus: &[(&'static str, Vec<u8>)],
) -> (&'static str, &'static str, Vec<u8>) {
    let s = rng.below(STRATEGIES.len());
    let strat = STRATEGIES[s];
    match s {
        0 => (strat, "random", random_blob(rng, 4096)),
        1 => (strat, "random", random_blob(rng, 16)),
        2 => {
            let (n, d) = &corpus[rng.pick(corpus.len())];
            (strat, n, bit_flips(rng, d))
        }
        3 => {
            let (n, d) = &corpus[rng.pick(corpus.len())];
            (strat, n, byte_subst(rng, d))
        }
        4 => {
            let (n, d) = &corpus[rng.pick(corpus.len())];
            (strat, n, truncate_at(rng, d))
        }
        5 => {
            let (n, d) = &corpus[rng.pick(corpus.len())];
            (strat, n, zero_run(rng, d))
        }
        6 => {
            let (na, a) = &corpus[rng.pick(corpus.len())];
            let (_nb, b) = &corpus[rng.pick(corpus.len())];
            (strat, na, splice(rng, a, b))
        }
        7 => {
            let (n, d) = &corpus[rng.pick(corpus.len())];
            (strat, n, duplicate(rng, d))
        }
        8 => {
            let (n, d) = &corpus[rng.pick(corpus.len())];
            (strat, n, corrupt_length_field(rng, d))
        }
        _ => (strat, "magic", magic_prefixed(rng)),
    }
}

// ---------------------------------------------------------------------------
// Driving one input. A `stage` atomic names the operation that wedged, so a
// HANG report says *which* scratch size and pass was spinning.
// ---------------------------------------------------------------------------

mod stage {
    pub const IDLE: usize = 0;
    pub const READ_BASE: usize = 1; // + index into SCRATCH_SIZES
    pub const FACTS: usize = 90;

    pub fn name(s: usize) -> String {
        match s {
            IDLE => "idle".into(),
            FACTS => "stream-facts check".into(),
            n if n >= READ_BASE && n < READ_BASE + super::SCRATCH_SIZES.len() => {
                format!("read(scratch={})", super::SCRATCH_SIZES[n - READ_BASE])
            }
            other => format!("stage {other}"),
        }
    }
}

/// Everything a library scan does with the bytes: read the tags at every
/// scratch size and look at what came back. Must be total.
fn drive(bytes: &[u8], stage_at: &AtomicUsize) {
    let mut facts_seen: Option<StreamFacts> = None;
    for (i, &size) in SCRATCH_SIZES.iter().enumerate() {
        stage_at.store(stage::READ_BASE + i, Ordering::Relaxed);
        let mut scratch = vec![0u8; size];
        let mut sink = CountingSink::default();
        let mut src = SliceSource::new(bytes);
        let Ok(facts) = soul_audio_tags::read(&mut src, &mut scratch, &mut sink) else {
            continue;
        };
        // Touch every descriptor the reader produced.
        for p in &sink.pictures {
            let _ = (p.data_offset, p.data_len, p.width, p.height);
            let _ = (p.usage, p.mime, p.encoding);
        }
        // The `alloc` surface, when it is built: `read_owned` materializes the
        // tags, and `read_picture_bytes` materializes a cover from a descriptor
        // the *file* chose. That last one is where a hostile `data_len` used to
        // request gigabytes before discovering the file was kilobytes, so the
        // invariant is explicit: a descriptor that does not fit must be refused,
        // and one that does must yield exactly the bytes it promised.
        #[cfg(feature = "alloc")]
        {
            let mut owned_src = SliceSource::new(bytes);
            if let Ok((tags, _)) = soul_audio_tags::read_owned(&mut owned_src) {
                for pic in tags.pictures.iter().take(8) {
                    let mut psrc = SliceSource::new(bytes);
                    // An `Err` is a lying or truncated descriptor, correctly
                    // refused; only a successful read makes a promise to check.
                    if let Ok(data) = soul_audio_tags::read_picture_bytes(&mut psrc, pic) {
                        // A `Raw` descriptor promises exactly `data_len` bytes
                        // from inside the file; Base64 (Ogg) decodes an
                        // embedded block whose length is its own.
                        if pic.encoding == soul_audio_tags::DataEncoding::Raw {
                            assert_eq!(
                                data.len(),
                                pic.data_len as usize,
                                "raw picture read returned a different length than promised"
                            );
                            assert!(
                                data.len() <= bytes.len(),
                                "picture larger than the whole file"
                            );
                        }
                    }
                }
            }
        }
        stage_at.store(stage::FACTS, Ordering::Relaxed);
        // The scratch size must not change what the *facts* are — it only
        // bounds how much of a field value is materialized. A reader whose
        // header parse depends on the value buffer is a bug worth catching.
        match facts_seen {
            None => facts_seen = Some(facts),
            Some(prev) => assert_eq!(
                prev, facts,
                "stream facts changed with the scratch size ({} vs {size} bytes)",
                SCRATCH_SIZES[0]
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Worker thread + hang detection
// ---------------------------------------------------------------------------

enum Outcome {
    Clean,
    Panic { loc: String, msg: String },
}

struct Worker {
    tx: Sender<Arc<Vec<u8>>>,
    rx: Receiver<Outcome>,
    stage: Arc<AtomicUsize>,
}

thread_local! {
    /// `file:line` of the most recent panic on this thread, captured by the
    /// hook below. Without it a report says only *which stage* panicked, and
    /// triaging "add with overflow somewhere in twelve parsers" is guesswork.
    static LAST_LOC: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Replace the default panic printer for the duration of the sweep: a panicking
/// input is an expected *result* here (collected into the report), and
/// thousands of backtraces would bury it. The hook still records where the
/// panic came from.
fn install_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            LAST_LOC.with(|c| *c.borrow_mut() = loc);
        }));
    });
}

fn spawn_worker() -> Worker {
    let (tx, in_rx) = channel::<Arc<Vec<u8>>>();
    let (out_tx, rx) = channel::<Outcome>();
    let stage = Arc::new(AtomicUsize::new(stage::IDLE));
    let worker_stage = Arc::clone(&stage);
    std::thread::Builder::new()
        .name("tag-fuzz".into())
        .stack_size(WORKER_STACK)
        .spawn(move || {
            for input in in_rx {
                let st = Arc::clone(&worker_stage);
                let result = catch_unwind(AssertUnwindSafe(|| drive(&input, &st)));
                let outcome = match result {
                    Ok(()) => Outcome::Clean,
                    Err(e) => {
                        let msg = e
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|| e.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "<non-string panic>".to_owned());
                        let at = LAST_LOC.with(|c| c.borrow().clone());
                        Outcome::Panic {
                            loc: format!(
                                "{at} (stage={})",
                                stage::name(st.load(Ordering::Relaxed))
                            ),
                            msg,
                        }
                    }
                };
                if out_tx.send(outcome).is_err() {
                    break;
                }
            }
        })
        .expect("spawn tag-fuzz worker");
    Worker { tx, rx, stage }
}

struct Failure {
    iter: u64,
    strategy: &'static str,
    seed_name: &'static str,
    kind: &'static str,
    detail: String,
    input: Arc<Vec<u8>>,
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    let mut s = String::new();
    for b in bytes.iter().take(n) {
        s.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > n {
        s.push('…');
    }
    s
}

struct Config {
    seed: u64,
    iters: u64,
    budget: Option<Duration>,
}

fn config_from_env() -> Config {
    let seed = std::env::var("TAG_FUZZ_SEED")
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
    let env_iters = std::env::var("TAG_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let budget = std::env::var("TAG_FUZZ_SECONDS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let iters = env_iters.unwrap_or(if budget.is_some() { u64::MAX } else { CI_ITERS });
    Config {
        seed,
        iters,
        budget,
    }
}

fn run(cfg: &Config) -> (Vec<Failure>, u64) {
    install_hook();
    let corpus = seeds();
    let mut rng = Rng::new(cfg.seed);
    let mut failures = Vec::new();
    let mut worker = spawn_worker();
    let start = Instant::now();
    let mut done = 0u64;

    for i in 0..cfg.iters {
        if let Some(b) = cfg.budget
            && start.elapsed() >= b
        {
            break;
        }
        done = i + 1;
        let (strat, seed_name, bytes) = generate(&mut rng, &corpus);
        let input = Arc::new(bytes);

        worker.stage.store(stage::IDLE, Ordering::Relaxed);
        if worker.tx.send(Arc::clone(&input)).is_err() {
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
                kind: "PANIC",
                detail: format!("{loc}: {msg}"),
                input,
            }),
            Err(RecvTimeoutError::Timeout) => {
                // A genuine non-terminating loop: the worker is wedged, so
                // abandon it and stop (a spinning thread skews the rest).
                let st = worker.stage.load(Ordering::Relaxed);
                failures.push(Failure {
                    iter: i,
                    strategy: strat,
                    seed_name,
                    kind: "HANG",
                    detail: format!("no progress in {} within {HANG_TIMEOUT:?}", stage::name(st)),
                    input,
                });
                break;
            }
            Err(RecvTimeoutError::Disconnected) => worker = spawn_worker(),
        }
    }
    (failures, done)
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

/// The gate: no byte sequence may panic or hang the tag reader, at any scratch
/// size. Deterministic in CI; `TAG_FUZZ_ITERS`/`TAG_FUZZ_SECONDS`/
/// `TAG_FUZZ_SEED` switch it into the extended local mode.
#[test]
fn fuzz_tag_surface_never_panics_or_hangs() {
    let cfg = config_from_env();
    let (failures, done) = run(&cfg);
    if !failures.is_empty() {
        // The quiet hook would swallow the assert's own message.
        let r = report(&failures, done);
        eprintln!("\n{r}");
        panic!(
            "tag fuzz found {} crash(es) — see report above",
            failures.len()
        );
    }
}

/// The corpus must actually reach every container `read` dispatches to —
/// otherwise this fuzzer silently covers three formats and reports green. Keyed
/// on the fixtures being present and sniffable, not on parser internals.
#[test]
fn corpus_covers_every_container() {
    let corpus = seeds();
    let mut sniffed = 0;
    for (name, bytes) in &corpus {
        let mut scratch = [0u8; 256];
        let mut sink = CountingSink::default();
        let mut src = SliceSource::new(bytes);
        if soul_audio_tags::read(&mut src, &mut scratch, &mut sink).is_ok() {
            sniffed += 1;
        } else if FIXTURES.contains(name) {
            panic!("fixture {name} no longer reads as a tagged file");
        }
    }
    assert!(
        sniffed >= FIXTURES.len(),
        "only {sniffed} of {} seed fixtures read successfully — the corpus has holes",
        FIXTURES.len()
    );
}

/// The generator must be reproducible: same seed → identical input sequence, so
/// a reported failure can be replayed exactly.
#[test]
fn generator_is_deterministic() {
    let corpus = seeds();
    let fingerprint = |seed: u64| -> Vec<(usize, u64)> {
        let mut rng = Rng::new(seed);
        (0..256)
            .map(|_| {
                let (_, _, b) = generate(&mut rng, &corpus);
                let sum = b
                    .iter()
                    .fold(0u64, |a, &x| a.wrapping_mul(31).wrapping_add(x.into()));
                (b.len(), sum)
            })
            .collect()
    };
    assert_eq!(fingerprint(CI_SEED), fingerprint(CI_SEED));
    assert_ne!(fingerprint(CI_SEED), fingerprint(CI_SEED ^ 0xFFFF));
    // Consecutive seeds must explore DIFFERENT corpora. They did not when the
    // state was `seed | 1`: 2 and 3 were one corpus, which is precisely the
    // pairing the weekly CI job hits, since it seeds from the run number.
    for base in [CI_SEED, 0, 1, 2, 1000] {
        assert_ne!(
            fingerprint(base),
            fingerprint(base + 1),
            "seeds {base} and {} generate the same inputs",
            base + 1
        );
    }
}
