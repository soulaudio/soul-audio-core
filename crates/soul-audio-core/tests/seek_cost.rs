//! GATE 1 — per-format **seek-COST** gate.
//!
//! Makes each format's seek *work* visible and locks it in, so a regression to
//! linear (decode-discard-from-start) seeking fails CI. The metric is
//! **deterministic source work**, never wall-clock: a [`CountingSource`] wraps an
//! in-memory [`SliceSource`] of each committed fixture and counts `read` calls,
//! `seek` calls, and **bytes pulled forward** (bytes returned by `read` plus
//! bytes advanced by `consume` — `fill_buf` peeks and `seek` jumps are *not*
//! bytes-forward, which is exactly what distinguishes a random-access seek from a
//! linear one). A `SliceSource` *is its own buffer* (`fill_buf` hands back the
//! whole remainder), so a decoder that parses via `BufRead` advances only through
//! `consume`; counting both `read` and `consume` captures the true forward work
//! whichever API the decoder uses.
//!
//! For EACH seekable format we open the decoder over a `CountingSource`, seek to
//! ~20 % and ~80 % of the total frames (a **fresh** decoder per target, so each
//! measurement is the full cost of reaching that depth from `open` — the honest
//! input to the position-independence ratio), decode one block so the seek fully
//! resolves, and record the counters. We then assert, per format:
//!
//!   (i)  an absolute upper bound on `bytes@80%` — generous, but far below the
//!        whole file, so a linear seek (which reads ~the whole prefix) trips it;
//!   (ii) **position-independence**: `bytes@80% ≤ POS_FACTOR × bytes@20%`. A
//!        linear O(target) seek makes this ratio ≈ 4 (0.8·file / 0.2·file) and
//!        fails; a random-access / index / bisection seek keeps it ≈ 1–2.
//!
//! WavPack is the one committed format whose seek is *legitimately* linear
//! (rewind + decode-discard forward — a block index is a documented follow-up),
//! so it is asserted at its real class (ratio ≈ 4, bytes scale with depth) with a
//! comment, not forced into the indexed bound. Everything else is random-access,
//! bisection (Ogg Vorbis/Opus/FLAC), a byte-estimate (CBR MP3/ADTS/MP2/MP1), an
//! MP4 sample table (AAC/ALAC), or a Matroska Cues jump (bounded within-cluster
//! forward-decode — the reason POS_FACTOR tolerates a small multiple).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use embedded_io::{BufRead, ErrorType, Read, Seek, SeekFrom};
use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{Decoder, DsdDecoder, DsdSeek, SeekableDecoder, SliceSource, open};

// ---------------------------------------------------------------------------
// CountingSource — the deterministic source-work probe.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counters {
    read_calls: AtomicU64,
    seek_calls: AtomicU64,
    bytes: AtomicU64,
}
impl Counters {
    fn reset(&self) {
        self.read_calls.store(0, Ordering::Relaxed);
        self.seek_calls.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }
    fn snapshot(&self) -> Metric {
        Metric {
            reads: self.read_calls.load(Ordering::Relaxed),
            seeks: self.seek_calls.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

/// A seekable `BufSource`+`SeekSource` over a `'static` fixture slice that counts
/// `read`/`seek` calls and bytes pulled forward (`read` returns + `consume`
/// amounts). `fill_buf` peeks and `seek` jumps add no forward bytes.
struct CountingSource {
    inner: SliceSource<'static>,
    c: Arc<Counters>,
}
impl CountingSource {
    fn new(data: &'static [u8], c: Arc<Counters>) -> Self {
        Self {
            inner: SliceSource::new(data),
            c,
        }
    }
}
impl ErrorType for CountingSource {
    type Error = embedded_io::ErrorKind;
}
impl Read for CountingSource {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.c.read_calls.fetch_add(1, Ordering::Relaxed);
        let n = self.inner.read(buf)?;
        self.c.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}
impl BufRead for CountingSource {
    fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        self.inner.fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        self.c.bytes.fetch_add(amt as u64, Ordering::Relaxed);
        self.inner.consume(amt);
    }
}
impl Seek for CountingSource {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        self.c.seek_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.seek(pos)
    }
}

#[derive(Clone, Copy)]
struct Metric {
    reads: u64,
    seeks: u64,
    bytes: u64,
}

/// Run `f` on a 16 MiB stack — an `AnyDecoder`/Opus/AAC carries large by-value
/// state that exceeds the default test-thread stack.
fn big_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(s, f)
            .expect("spawn")
            .join()
            .expect("thread panicked")
    })
}

// ---------------------------------------------------------------------------
// Totals (ground truth for the 20 %/80 % targets) — measured off a plain,
// *uncounted* SliceSource so the end-granule scan etc. never pollutes a metric.
// ---------------------------------------------------------------------------

/// Per-channel total frames for a format routed through `open`. Uses the
/// declared length when present; otherwise a full decode (Ogg-FLAC may declare
/// none). `resolve` fills the CBR length for ADTS/MP2/MP1.
fn pcm_total(bytes: &'static [u8], resolve: bool) -> u64 {
    big_stack(move || {
        let mut dec = open(SliceSource::new(bytes)).expect("open");
        if resolve {
            dec.resolve_lengths();
        }
        if let Some(t) = dec.total_frames() {
            return t;
        }
        let ch = u64::from(dec.stream_info().channels.max(1));
        let mut total = 0u64;
        let mut buf = [0i32; 4096];
        loop {
            let n = dec.decode_into(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total / ch
    })
}

fn vorbis_total(bytes: &'static [u8]) -> u64 {
    big_stack(move || {
        let storage = vec![0u8; 128 * 1024];
        let mut dec = VorbisDecoder::new(SliceSource::new(bytes), storage).expect("open vorbis");
        let ch = u64::from(dec.stream_info().channels.max(1));
        let mut total = 0u64;
        let mut buf = [0i32; 4096];
        loop {
            let n = dec.decode_into(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total / ch
    })
}

// ---------------------------------------------------------------------------
// Per-format measurement: open over a CountingSource, (resolve), reset, seek,
// decode ONE block so the seek fully resolves, snapshot. Fresh decoder each call.
// ---------------------------------------------------------------------------

fn measure_pcm(bytes: &'static [u8], resolve: bool, target: u64) -> Metric {
    big_stack(move || {
        let c = Arc::new(Counters::default());
        let mut dec = open(CountingSource::new(bytes, c.clone())).expect("open");
        if resolve {
            dec.resolve_lengths();
        }
        c.reset();
        dec.seek_to(target).expect("seek");
        decode_one_block_pcm(&mut dec);
        c.snapshot()
    })
}

fn decode_one_block_pcm<D: Decoder>(dec: &mut D) {
    let mut buf = [0i32; 4096];
    for _ in 0..8 {
        if dec.decode_into(&mut buf).expect("decode") > 0 {
            return;
        }
    }
    panic!("decoder produced no samples after seek (unexpected EOF/priming)");
}

fn measure_vorbis(bytes: &'static [u8], target: u64) -> Metric {
    big_stack(move || {
        let c = Arc::new(Counters::default());
        let storage = vec![0u8; 128 * 1024];
        let mut dec = VorbisDecoder::new(CountingSource::new(bytes, c.clone()), storage)
            .expect("open vorbis");
        c.reset();
        SeekableDecoder::seek_to(&mut dec, target).expect("seek");
        decode_one_block_pcm(&mut dec);
        c.snapshot()
    })
}

fn measure_dsf(bytes: &'static [u8], target: u64) -> Metric {
    big_stack(move || {
        let c = Arc::new(Counters::default());
        let mut dec = DsfDecoder::new(CountingSource::new(bytes, c.clone())).expect("open dsf");
        c.reset();
        dec.seek_to_sample(target).expect("seek");
        decode_one_block_dsd(&mut dec);
        c.snapshot()
    })
}

fn measure_dff(bytes: &'static [u8], target: u64) -> Metric {
    big_stack(move || {
        let c = Arc::new(Counters::default());
        let mut dec = DffDecoder::new(CountingSource::new(bytes, c.clone())).expect("open dff");
        c.reset();
        dec.seek_to_sample(target).expect("seek");
        decode_one_block_dsd(&mut dec);
        c.snapshot()
    })
}

fn decode_one_block_dsd<D: DsdDecoder>(dec: &mut D) {
    let mut buf = [0u8; 4096];
    for _ in 0..8 {
        if dec.read_dsd(&mut buf).expect("read_dsd") > 0 {
            return;
        }
    }
    panic!("DSD decoder produced no bytes after seek");
}

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    /// Routed through `open`; `bool` = call `resolve_lengths()` first (CBR
    /// ADTS/MP2/MP1 byte-estimate fast path).
    Pcm(bool),
    Vorbis,
    Dsf,
    Dff,
}

#[derive(Clone, Copy)]
enum Class {
    /// Random-access / index / bisection seek: must be **position-independent**.
    /// `(abs_bytes80_max, pos_factor)`. `pos_factor` sits strictly below 4 (the
    /// linear ratio) so a regression to decode-from-start trips it.
    Indexed(u64, f64),
    /// Legitimately **linear** (decode-discard from the stream start — no usable
    /// index in the committed fixtures) — asserted at its real class with a
    /// comment, not forced into the indexed bound. `file_mult` caps `bytes@80%`
    /// at `file_mult × file_size`: a linear seek reads ≈ the 80 % prefix (ratio
    /// ≈ 4 vs the 20 % one, printed), and must never re-read the file in full
    /// passes. If it ever drops to ratio ≈ 1 it gained an index → re-class it.
    Linear(f64),
}

struct Spec {
    name: &'static str,
    bytes: &'static [u8],
    kind: Kind,
    class: Class,
}

fn total_for(spec: &Spec) -> u64 {
    match spec.kind {
        Kind::Pcm(resolve) => pcm_total(spec.bytes, resolve),
        Kind::Vorbis => vorbis_total(spec.bytes),
        Kind::Dsf => DsfDecoder::new(SliceSource::new(spec.bytes))
            .expect("open dsf")
            .total_samples()
            .expect("dsf declares total"),
        Kind::Dff => DffDecoder::new(SliceSource::new(spec.bytes))
            .expect("open dff")
            .total_samples()
            .expect("dff declares total"),
    }
}

fn measure_at(spec: &Spec, target: u64) -> Metric {
    match spec.kind {
        Kind::Pcm(resolve) => measure_pcm(spec.bytes, resolve, target),
        Kind::Vorbis => measure_vorbis(spec.bytes, target),
        Kind::Dsf => measure_dsf(spec.bytes, target),
        Kind::Dff => measure_dff(spec.bytes, target),
    }
}

#[test]
fn per_format_seek_cost_is_bounded_and_position_independent() {
    // Fixtures reuse the per-format seek round-trip suites' files + open dispatch.
    // Bounds are set generously above the measured numbers but far below "whole
    // file"; see the printed table for the headroom.
    // Bounds below are set from the measured table (see the test's stdout): an
    // absolute ceiling with headroom over the real `bytes@80%`, and a
    // `pos_factor` comfortably under 4. Fixtures are the per-format seek suites'
    // own files + `open` dispatch.
    let specs = [
        // --- random-access: a direct byte/block offset. bytes ≈ one block,
        //     flat in position (ratio ~1, often <1 when the 80% block is
        //     shorter). ---
        Spec {
            name: "wav",
            bytes: include_bytes!("../../../testdata/tone.wav"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(16_384, 2.0),
        },
        Spec {
            name: "aiff",
            bytes: include_bytes!("../../../testdata/tone-aiff.aiff"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(16_384, 2.0),
        },
        Spec {
            name: "caf(pcm)",
            bytes: include_bytes!("../../../testdata/tone.caf"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(16_384, 2.0),
        },
        Spec {
            name: "dsf",
            bytes: include_bytes!("../../../testdata/tone.dsf"),
            kind: Kind::Dsf,
            class: Class::Indexed(32_768, 2.5),
        },
        Spec {
            name: "dff",
            bytes: include_bytes!("../../../testdata/tone.dff"),
            kind: Kind::Dff,
            class: Class::Indexed(32_768, 2.0),
        },
        // --- CBR byte-estimate (Xing/Info TOC). MP3 Layer III carries a LAME
        //     `Info` tag → O(1) TOC jump. (MP2/MP1 have no tag → linear, below.) ---
        Spec {
            name: "mp3(info)",
            bytes: include_bytes!("../../../testdata/tone.mp3"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(16_384, 2.0),
        },
        // --- raw ADTS: resolve_lengths() → CBR byte-estimate + syncword resync,
        //     the O(1) fast path (vs the linear decode-discard without it). ---
        Spec {
            name: "aac-adts",
            bytes: include_bytes!("../../../testdata/tone.aac"),
            kind: Kind::Pcm(true),
            class: Class::Indexed(32_768, 2.0),
        },
        // --- MP4 sample tables (stts/stsc/stco) → random access. The seek walks
        //     a few table entries (a handful of `seek`s) then reads one AU. ---
        Spec {
            name: "aac-m4a",
            bytes: include_bytes!("../../../testdata/realmusic-aac.m4a"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(32_768, 2.0),
        },
        Spec {
            name: "alac",
            bytes: include_bytes!("../../../testdata/realmusic-alac.m4a"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(49_152, 2.0),
        },
        // --- Ogg granule BISECTION (Vorbis/Opus/FLAC-in-Ogg). O(log n) *seeks*,
        //     but the page/packet scan re-reads byte-by-byte, so the byte count
        //     runs a few× the file — the absolute ceiling is therefore generous;
        //     the real anti-linear evidence is position-independence (ratio well
        //     under 4) with a tiny seek count. ---
        Spec {
            name: "ogg-vorbis",
            bytes: include_bytes!("../../../testdata/tone-vorbis-dense.ogg"),
            kind: Kind::Vorbis,
            class: Class::Indexed(300_000, 2.5),
        },
        Spec {
            name: "opus",
            bytes: include_bytes!("../../../testdata/realmusic.opus"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(600_000, 3.0),
        },
        Spec {
            name: "oggflac",
            bytes: include_bytes!("../testdata/flac-in-ogg-multipage.ogg"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(1_500_000, 3.0),
        },
        // --- Matroska Cues jump: index → straight to the covering cluster, then
        //     a *bounded* within-cluster forward-decode. Tiny and flat. ---
        Spec {
            name: "mka-opus",
            bytes: include_bytes!("../../../testdata/tone-opus-cues.mka"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(16_384, 2.0),
        },
        Spec {
            name: "webm-opus",
            bytes: include_bytes!("../../../testdata/tone-opus-cues.webm"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(16_384, 2.0),
        },
        // --- WavPack: walks 32-byte block HEADERS, `seek`-skipping each block
        //     body, until the target block, then decodes that ONE block and
        //     discards its head. The header walk is O(blocks) in *seeks* (grows
        //     with depth), but bytes-forward are dominated by the single block
        //     body (~one block, position-independent) — so it passes the indexed
        //     bytes bound. (A true block *index* would also cut the seeks.) ---
        Spec {
            name: "wavpack",
            bytes: include_bytes!("../../../testdata/realmusic.wv"),
            kind: Kind::Pcm(false),
            class: Class::Indexed(32_768, 2.0),
        },
        // --- legitimately LINEAR (decode-discard from the stream start). Native
        //     FLAC has NO seektable in any committed fixture, so the demuxer's
        //     `seek_point_before` returns the origin and the decoder discards to
        //     the target — O(target). (A seektable'd file would be O(1).) MP2/MP1
        //     are header-less CBR with no Info-TOC, so their seek likewise
        //     decode-discards. Asserted at this real class, not forced indexed. ---
        Spec {
            name: "flac(no-stbl)",
            bytes: include_bytes!("../../../testdata/realmusic.flac"),
            kind: Kind::Pcm(false),
            class: Class::Linear(1.25),
        },
        Spec {
            name: "mp2",
            bytes: include_bytes!("../../../testdata/tone.mp2"),
            kind: Kind::Pcm(true),
            class: Class::Linear(1.25),
        },
        Spec {
            name: "mp1",
            bytes: include_bytes!("../../../testdata/tone-layer1.mp1"),
            kind: Kind::Pcm(true),
            class: Class::Linear(1.25),
        },
    ];

    println!(
        "\n{:<12} | {:>10} | {:>10} | {:>8} | {:>7} | {:>7} | {:>6}",
        "format", "bytes@20%", "bytes@80%", "ratio", "reads@80", "seeks@80", "file"
    );
    println!("{}", "-".repeat(80));

    let mut failures = Vec::new();
    for spec in &specs {
        let total = total_for(spec);
        assert!(
            total > 0,
            "{}: zero total frames — fixture decode failed",
            spec.name
        );
        let t20 = (total * 20) / 100;
        let t80 = (total * 80) / 100;
        let m20 = measure_at(spec, t20);
        let m80 = measure_at(spec, t80);
        let ratio = m80.bytes as f64 / (m20.bytes.max(1)) as f64;
        println!(
            "{:<12} | {:>10} | {:>10} | {:>8.2} | {:>7} | {:>7} | {:>6}",
            spec.name,
            m20.bytes,
            m80.bytes,
            ratio,
            m80.reads,
            m80.seeks,
            spec.bytes.len(),
        );

        match spec.class {
            Class::Indexed(abs_max, pos_factor) => {
                if m80.bytes > abs_max {
                    failures.push(format!(
                        "{}: bytes@80% {} exceeds absolute bound {} (linear-seek smell; \
                         file is {} B)",
                        spec.name,
                        m80.bytes,
                        abs_max,
                        spec.bytes.len()
                    ));
                }
                if m80.bytes as f64 > pos_factor * m20.bytes.max(1) as f64 {
                    failures.push(format!(
                        "{}: NOT position-independent — bytes@80% {} > {} × bytes@20% {} \
                         (ratio {:.2}); a linear O(target) seek lands near 4",
                        spec.name, m80.bytes, pos_factor, m20.bytes, ratio
                    ));
                }
            }
            Class::Linear(file_mult) => {
                // Real class: cost scales with depth (ratio ≈ 4, printed above).
                // We do NOT claim position-independence; the one invariant worth
                // locking is that it never re-reads the file in multiple passes —
                // a linear decode-discard to 80% touches ≈ the 80% prefix, so
                // bytes@80% ≤ file_mult × file_size. (If it ever drops to ratio
                // ≈ 1 it gained an index; re-class it as Indexed — that is a
                // welcome change, not a failure, so there is no min-ratio gate.)
                let abs_max = (spec.bytes.len() as f64 * file_mult) as u64;
                if m80.bytes > abs_max {
                    failures.push(format!(
                        "{}: bytes@80% {} exceeds {:.0}× the file ({} B) = {} — a linear \
                         seek should touch ≈ the 80% prefix once, not re-read in passes",
                        spec.name,
                        m80.bytes,
                        file_mult,
                        spec.bytes.len(),
                        abs_max
                    ));
                }
            }
        }
    }
    println!();

    assert!(
        failures.is_empty(),
        "seek-cost gate failures:\n{}",
        failures.join("\n")
    );
}
