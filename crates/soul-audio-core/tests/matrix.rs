//! Declarative per-format test MATRIX.
//!
//! One [`Fmt`] row per format carries the ONLY per-format-specific data (fixture
//! bytes, how to open a decoder, channel/rate, lossless-vs-lossy tolerance, the
//! committed reference, seek targets, the seek-cost bound, …). Generic runners
//! iterate [`MATRIX`] with **zero per-format branching** beyond reading the row —
//! the [`Engine`] field encapsulates the one genuinely format-specific bit (which
//! decoder-construction path to take), so adding a format is one row and a
//! format's quirks live in its row, not scattered through test logic.
//!
//! Five runners, each folding in what used to be scattered bespoke tests:
//!   * [`decode_vs_reference`] — full decode vs the committed `.ref.pcm`/WAV:
//!     `BitExact` (len + FNV-1a) for lossless, segmental-`Snr` for lossy. Folds
//!     the `realmusic_*`, `open_decodes_*_bit_exact_vs_wav`, and the Matroska /
//!     ADTS decode goldens.
//!   * [`seek_round_trip`] — sample-accurate seek: `landed == target` and the
//!     post-seek tail is bit-identical to the full decode's suffix. Folds the
//!     `*_seek_tail_matches_full_decode` family (WavPack, CAF-pcm/alac, Matroska
//!     Opus/FLAC/AAC, Ogg-FLAC).
//!   * [`seek_cost_bounded`] — position-independence (bytes@20% vs @80% via a
//!     `CountingSource`). Folds the whole of the old `tests/seek_cost.rs`,
//!     **printed table included**.
//!   * [`declick`] — null-residual + bounded second-difference on real decoded
//!     audio through a `Deck`. Folds the whole of the old `tests/declick.rs`.
//!   * [`duration`] — `resolve_lengths()` ⇒ `total_frames()` ≈ the length a full
//!     decode actually yields, for the formats whose declared total is exact.
//!
//! Intentionally NOT folded (kept as bespoke tests — see `tests/decode.rs`):
//! the conformance / spec-vector suites (MP2/kjmp2, Layer-I, SBR/PS, Opus/MP3
//! conformance), the format-specific edge/variant decodes (24-bit, full-scale,
//! antiphase, mono, MPEG-2/2.5, float, companding, non-faststart, 5.1/6-ch …),
//! the robustness/malformed suites, the synthetic-ramp WAV/DSD seek suites
//! (`tests/seek.rs`, different oracle), and the page-granular Vorbis/Opus seek +
//! granule-exact duration tests (their SNR-"matches-somewhere" oracle and their
//! exact-ref-length checks are stronger/different than the generic runners).

use embedded_io::{BufRead, ErrorType, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{
    AnyDecoder, Deck, Decoder, DsdDecoder, DsdSeek, Progress, Quality, SampleFormat,
    SeekableDecoder, Sink, SinkError, SliceSource, StreamInfo, open,
};

// ===========================================================================
// Shared helpers (ported verbatim from tests/decode.rs + tests/seek_cost.rs).
// ===========================================================================

/// Run `f` on a 16 MiB stack — an `AnyDecoder`/Opus/AAC/Vorbis carries large
/// by-value state that overflows the default test-thread stack in debug.
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

fn fnv1a(samples: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &s in samples {
        h ^= u64::from(s as u32);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Interpret a little-endian signed-16-bit PCM blob (ffmpeg's `-f s16le`) as
/// interleaved `i32` samples — the representation our decoders emit.
fn load_s16le(bytes: &[u8]) -> Vec<i32> {
    bytes
        .chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect()
}

/// Per-channel priming offset (a multiple of `channels`) that best aligns `ours`
/// to `refr`, with its segmental SNR in dB. Lossy encoders prepend decoder-delay
/// samples ffmpeg trims via the edit list while our raw decode keeps them, so
/// `ours` leads `refr` by up to a few frames; slide and keep the best.
fn best_align_segsnr(ours: &[i32], refr: &[i32], channels: usize) -> (usize, f64) {
    let mut best = (0usize, f64::NEG_INFINITY);
    for off_frames in 0..=3072usize {
        let off = off_frames * channels;
        if off >= ours.len() {
            break;
        }
        let overlap = (ours.len() - off).min(refr.len());
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

/// Decode a whole stream through the public `open()` API → interleaved PCM.
fn decode_pcm_all(bytes: &'static [u8]) -> Vec<i32> {
    big_stack(move || {
        let mut dec = open(SliceSource::new(bytes)).expect("probe + construct");
        drain_pcm(&mut dec)
    })
}

/// Decode a bare Ogg-Vorbis stream via the arena path (`open()` can't — Vorbis
/// needs a caller-provided arena).
fn decode_vorbis_all(bytes: &'static [u8]) -> Vec<i32> {
    big_stack(move || {
        let mut dec = VorbisDecoder::new(SliceSource::new(bytes), vec![0u8; 128 * 1024])
            .expect("open vorbis");
        drain_pcm(&mut dec)
    })
}

fn drain_pcm<D: Decoder>(dec: &mut D) -> Vec<i32> {
    let mut out = Vec::new();
    let mut buf = [0i32; 4096];
    loop {
        let n = dec.decode_into(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Full decode for a row, honouring its [`Engine`] (Vorbis uses the arena path).
fn decode_all_for(engine: Engine, bytes: &'static [u8]) -> Vec<i32> {
    match engine {
        Engine::Vorbis => decode_vorbis_all(bytes),
        _ => decode_pcm_all(bytes),
    }
}

// ===========================================================================
// The MATRIX schema. Each dimension is an `Option<…Case>`: a runner iterates the
// whole MATRIX and acts only on rows that carry that dimension's data (reading
// the row — never branching on the format name).
// ===========================================================================

/// The one genuinely format-specific bit: which decoder-construction path to
/// take. Everything else a runner needs is plain data in the row.
#[derive(Clone, Copy)]
enum Engine {
    /// Public `open()` → `AnyDecoder` (WAV/FLAC/MP3/AAC-m4a/ALAC/AIFF/CAF/WavPack
    /// /Opus/Ogg-FLAC/Matroska/WebM/MP2/MP1).
    Pcm,
    /// Arena `VorbisDecoder` (Ogg-Vorbis).
    Vorbis,
    /// DSD `DsfDecoder` (`read_dsd` bytes, `seek_to_sample`).
    Dsf,
    /// DSD `DffDecoder`.
    Dff,
}

/// Decode tolerance: lossless must be bit-exact; lossy clears an SNR floor.
#[derive(Clone, Copy)]
enum Tol {
    BitExact,
    Snr(f64),
}

/// The committed reference a full decode is compared against.
#[derive(Clone, Copy)]
enum Reference {
    /// Decode these bytes through `open()` too and compare (the WAV the lossless
    /// codec was encoded from, or the native stream a remux copied).
    OpenPcm(&'static [u8]),
    /// ffmpeg `-f s16le` reference PCM.
    S16(&'static [u8]),
}

/// Seek-cost class — informs the cost bound's narrative. Every committed fixture
/// here is random-access / indexed / bisection, so all are position-independent.
#[derive(Clone, Copy)]
enum SeekClass {
    Indexed,
    Bisect,
}

#[derive(Clone, Copy)]
struct DecodeCase {
    fixture: &'static [u8],
    reference: Reference,
    tol: Tol,
    /// Assert `total_frames()` is `None` straight after `open()` (streaming
    /// formats with no in-band length: raw ADTS).
    expect_total_none: bool,
}

#[derive(Clone, Copy)]
struct SeekCase {
    fixture: &'static [u8],
    /// Per-channel sample targets; the post-seek tail must be bit-identical to
    /// `full[target*ch..]` and the landing sample-accurate (`landed == target`).
    targets: &'static [u64],
    /// Also assert a beyond-end seek clamps to the total and yields no tail.
    clamp: bool,
    /// Assert `total_frames()` is `None` (a valid streaming case — Ogg-FLAC).
    total_none: bool,
}

#[derive(Clone, Copy)]
struct CostCase {
    fixture: &'static [u8],
    /// Absolute upper bound on `bytes@80%` (generous, but far below the whole
    /// file, so a linear seek trips it).
    abs80: u64,
    /// Position-independence factor: `bytes@80% ≤ pos_factor × bytes@20%`. Sits
    /// strictly below 4 (the linear O(target) ratio).
    pos_factor: f64,
}

#[derive(Clone, Copy)]
struct DurCase {
    fixture: &'static [u8],
    /// `|total_frames() − decoded_frames|` must be `≤ tol_frames` (0 for the
    /// formats whose declared total is exact).
    tol_frames: u64,
}

#[derive(Clone, Copy)]
struct DeclickCase {
    fixture: &'static [u8],
}

/// One row per format — the only place per-format data lives.
struct Fmt {
    name: &'static str,
    engine: Engine,
    sample_rate: u32,
    channels: u16,
    #[allow(dead_code)] // documents the row; runners read `Tol`/`SeekClass`.
    lossy: bool,
    /// `resolve_lengths()` before a seek/cost measurement (CBR ADTS/MP2/MP1).
    resolve: bool,
    #[allow(dead_code)] // narrative for the cost table; not a logic branch.
    seek_class: SeekClass,
    decode: Option<DecodeCase>,
    seek: Option<SeekCase>,
    cost: Option<CostCase>,
    duration: Option<DurCase>,
    declick: Option<DeclickCase>,
}

// Root `testdata/` (repo-wide, 3 levels up from this source file) and the
// crate-local `testdata/` (`../testdata/`, conformance vectors).
macro_rules! root {
    ($p:literal) => {
        include_bytes!(concat!("../../../testdata/", $p))
    };
}
macro_rules! crate_td {
    ($p:literal) => {
        include_bytes!(concat!("../testdata/", $p))
    };
}

const MATRIX: &[Fmt] = &[
    // --- WAV: the lossless PCM oracle. Decode-vs-ref is vacuous (it IS the
    //     reference for FLAC/AIFF/CAF/…), so no decode row; it anchors cost,
    //     declick and duration. ---
    Fmt {
        name: "wav",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: None,
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone.wav"),
            abs80: 16_384,
            pos_factor: 2.0,
        }),
        duration: Some(DurCase {
            fixture: root!("tone.wav"),
            tol_frames: 0,
        }),
        declick: Some(DeclickCase {
            fixture: root!("tone.wav"),
        }),
    },
    // --- FLAC (native): lossless, bit-exact vs the source WAV; frame bisection
    //     seek (no SEEKTABLE in the fixture). ---
    Fmt {
        name: "flac",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Bisect,
        decode: Some(DecodeCase {
            fixture: root!("realmusic.flac"),
            reference: Reference::OpenPcm(root!("realmusic.wav")),
            tol: Tol::BitExact,
            expect_total_none: false,
        }),
        seek: None,
        cost: Some(CostCase {
            fixture: root!("realmusic.flac"),
            abs80: 65_536,
            pos_factor: 2.0,
        }),
        duration: Some(DurCase {
            fixture: root!("realmusic.flac"),
            tol_frames: 0,
        }),
        declick: None,
    },
    // --- ALAC (Apple Lossless, MP4): bit-exact vs source WAV; MP4 sample table. ---
    Fmt {
        name: "alac",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("realmusic-alac.m4a"),
            reference: Reference::OpenPcm(root!("realmusic.wav")),
            tol: Tol::BitExact,
            expect_total_none: false,
        }),
        seek: None,
        cost: Some(CostCase {
            fixture: root!("realmusic-alac.m4a"),
            abs80: 49_152,
            pos_factor: 2.0,
        }),
        duration: Some(DurCase {
            fixture: root!("realmusic-alac.m4a"),
            tol_frames: 0,
        }),
        declick: None,
    },
    // --- WavPack: lossless, bit-exact; block-header walk seek, sample-accurate. ---
    Fmt {
        name: "wavpack",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("realmusic.wv"),
            reference: Reference::OpenPcm(root!("realmusic.wav")),
            tol: Tol::BitExact,
            expect_total_none: false,
        }),
        seek: Some(SeekCase {
            fixture: root!("tone.wv"),
            targets: &[0, 30_000, 44_100, 70_000],
            clamp: true,
            total_none: false,
        }),
        cost: Some(CostCase {
            fixture: root!("realmusic.wv"),
            abs80: 32_768,
            pos_factor: 2.0,
        }),
        duration: Some(DurCase {
            fixture: root!("realmusic.wv"),
            tol_frames: 0,
        }),
        declick: None,
    },
    // --- AIFF (big-endian PCM): lossless, bit-exact vs source WAV. ---
    Fmt {
        name: "aiff",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("tone-aiff.aiff"),
            reference: Reference::OpenPcm(root!("tone.wav")),
            tol: Tol::BitExact,
            expect_total_none: false,
        }),
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone-aiff.aiff"),
            abs80: 16_384,
            pos_factor: 2.0,
        }),
        duration: Some(DurCase {
            fixture: root!("tone-aiff.aiff"),
            tol_frames: 0,
        }),
        declick: None,
    },
    // --- CAF (big-endian PCM): lossless, bit-exact vs source WAV; byte-offset
    //     seek, sample-accurate. ---
    Fmt {
        name: "caf",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("tone.caf"),
            reference: Reference::OpenPcm(root!("tone.wav")),
            tol: Tol::BitExact,
            expect_total_none: false,
        }),
        seek: Some(SeekCase {
            fixture: root!("tone.caf"),
            targets: &[25_000],
            clamp: false,
            total_none: false,
        }),
        cost: Some(CostCase {
            fixture: root!("tone.caf"),
            abs80: 16_384,
            pos_factor: 2.0,
        }),
        duration: Some(DurCase {
            fixture: root!("tone.caf"),
            tol_frames: 0,
        }),
        declick: None,
    },
    // --- CAF-ALAC: lossless; `pakt` packet-table seek, sample-accurate. ---
    Fmt {
        name: "caf-alac",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: None,
        seek: Some(SeekCase {
            fixture: root!("tone-alac.caf"),
            targets: &[0, 25_000, 49_152, 70_000],
            clamp: true,
            total_none: false,
        }),
        cost: None,
        duration: None,
        declick: None,
    },
    // --- MP3 (MPEG-1 Layer III, LAME `Info` TOC): lossy; O(1) TOC-jump seek. ---
    Fmt {
        name: "mp3",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: true,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("realmusic.mp3"),
            reference: Reference::S16(root!("realmusic.mp3.ref.pcm")),
            tol: Tol::Snr(55.0),
            expect_total_none: false,
        }),
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone.mp3"),
            abs80: 16_384,
            pos_factor: 2.0,
        }),
        duration: None,
        declick: None,
    },
    // --- AAC-LC (MP4/.m4a): lossy; MP4 sample-table seek. ---
    Fmt {
        name: "aac-m4a",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: true,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("realmusic-aac.m4a"),
            reference: Reference::S16(root!("realmusic-aac.m4a.ref.pcm")),
            tol: Tol::Snr(50.0),
            expect_total_none: false,
        }),
        seek: None,
        cost: Some(CostCase {
            fixture: root!("realmusic-aac.m4a"),
            abs80: 32_768,
            pos_factor: 2.0,
        }),
        duration: None,
        declick: None,
    },
    // --- AAC-LC raw ADTS (.aac): lossy, streaming (no in-band length);
    //     resolve_lengths() → CBR byte-estimate seek. ---
    Fmt {
        name: "aac-adts",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: true,
        resolve: true,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: crate_td!("aac.aac"),
            reference: Reference::S16(crate_td!("aac.aac.ref.pcm")),
            tol: Tol::Snr(45.0),
            expect_total_none: true,
        }),
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone.aac"),
            abs80: 32_768,
            pos_factor: 2.0,
        }),
        duration: None,
        declick: None,
    },
    // --- Ogg-Vorbis: lossy; arena decoder; Ogg-granule bisection seek. ---
    Fmt {
        name: "ogg-vorbis",
        engine: Engine::Vorbis,
        sample_rate: 44_100,
        channels: 2,
        lossy: true,
        resolve: false,
        seek_class: SeekClass::Bisect,
        decode: Some(DecodeCase {
            fixture: root!("realmusic-vorbis.ogg"),
            reference: Reference::S16(root!("realmusic-vorbis.ogg.ref.pcm")),
            tol: Tol::Snr(55.0),
            expect_total_none: false,
        }),
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone-vorbis-dense.ogg"),
            abs80: 300_000,
            pos_factor: 2.5,
        }),
        duration: None,
        declick: None,
    },
    // --- Opus (Ogg, CELT fullband): lossy; Ogg-granule bisection seek. ---
    Fmt {
        name: "opus",
        engine: Engine::Pcm,
        sample_rate: 48_000,
        channels: 2,
        lossy: true,
        resolve: false,
        seek_class: SeekClass::Bisect,
        decode: Some(DecodeCase {
            fixture: root!("realmusic.opus"),
            reference: Reference::S16(root!("realmusic.opus.ref.pcm")),
            tol: Tol::Snr(55.0),
            expect_total_none: false,
        }),
        seek: None,
        cost: Some(CostCase {
            fixture: root!("realmusic.opus"),
            abs80: 600_000,
            pos_factor: 3.0,
        }),
        duration: None,
        declick: None,
    },
    // --- Ogg-FLAC: lossless; sample-accurate seek; STREAMINFO declares no total
    //     (a valid streaming case — total_frames() must be None and not panic). ---
    Fmt {
        name: "oggflac",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 1,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Bisect,
        decode: None,
        seek: Some(SeekCase {
            fixture: crate_td!("flac-in-ogg.ogg"),
            targets: &[0, 1_000, 5_000],
            clamp: false,
            total_none: true,
        }),
        cost: Some(CostCase {
            fixture: crate_td!("flac-in-ogg-multipage.ogg"),
            abs80: 1_500_000,
            pos_factor: 3.0,
        }),
        duration: None,
        declick: None,
    },
    // --- Opus-in-Matroska (.mka): lossy; Cues-jump seek, sample-accurate. ---
    Fmt {
        name: "mka-opus",
        engine: Engine::Pcm,
        sample_rate: 48_000,
        channels: 2,
        lossy: true,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: None,
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone-opus-cues.mka"),
            abs80: 16_384,
            pos_factor: 2.0,
        }),
        duration: None,
        declick: None,
    },
    // --- Opus-in-WebM (.webm): lossy; Cues-jump seek. Decode golden + seek tail. ---
    Fmt {
        name: "webm-opus",
        engine: Engine::Pcm,
        sample_rate: 48_000,
        channels: 2,
        lossy: true,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("tone-opus.webm"),
            reference: Reference::S16(root!("tone-opus.webm.ref.pcm")),
            tol: Tol::Snr(60.0),
            expect_total_none: false,
        }),
        seek: Some(SeekCase {
            fixture: root!("tone-opus.webm"),
            targets: &[24_000],
            clamp: false,
            total_none: false,
        }),
        cost: Some(CostCase {
            fixture: root!("tone-opus-cues.webm"),
            abs80: 16_384,
            pos_factor: 2.0,
        }),
        duration: None,
        declick: None,
    },
    // --- AAC-in-Matroska (.mka): lossy; Cues-jump seek, sample-accurate. ---
    Fmt {
        name: "mka-aac",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: true,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("tone-aac.mka"),
            reference: Reference::S16(root!("tone-aac.mka.ref.pcm")),
            tol: Tol::Snr(45.0),
            expect_total_none: false,
        }),
        seek: Some(SeekCase {
            fixture: root!("tone-aac.mka"),
            targets: &[20_000],
            clamp: false,
            total_none: false,
        }),
        cost: None,
        duration: None,
        declick: None,
    },
    // --- FLAC-in-Matroska (.mka): lossless; bit-exact vs the native FLAC it was
    //     remuxed from; sample-accurate seek. ---
    Fmt {
        name: "mka-flac",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: Some(DecodeCase {
            fixture: root!("tone-flac.mka"),
            reference: Reference::OpenPcm(root!("tone.flac")),
            tol: Tol::BitExact,
            expect_total_none: false,
        }),
        seek: Some(SeekCase {
            fixture: root!("tone-flac.mka"),
            targets: &[20_000],
            clamp: false,
            total_none: false,
        }),
        cost: None,
        duration: None,
        declick: None,
    },
    // --- DSF (DSD, planar): block-offset seek. ---
    Fmt {
        name: "dsf",
        engine: Engine::Dsf,
        sample_rate: 2_822_400,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: None,
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone.dsf"),
            abs80: 32_768,
            pos_factor: 2.5,
        }),
        duration: None,
        declick: None,
    },
    // --- DFF (DSD, interleaved): block-offset seek. ---
    Fmt {
        name: "dff",
        engine: Engine::Dff,
        sample_rate: 2_822_400,
        channels: 2,
        lossy: false,
        resolve: false,
        seek_class: SeekClass::Indexed,
        decode: None,
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone.dff"),
            abs80: 32_768,
            pos_factor: 2.0,
        }),
        duration: None,
        declick: None,
    },
    // --- MP2 (MPEG-1 Layer II, header-less CBR): byte-estimate + resync seek
    //     after resolve_lengths(). ---
    Fmt {
        name: "mp2",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: true,
        resolve: true,
        seek_class: SeekClass::Indexed,
        decode: None,
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone.mp2"),
            abs80: 24_576,
            pos_factor: 2.0,
        }),
        duration: None,
        declick: None,
    },
    // --- MP1 (MPEG-1 Layer I, header-less CBR): byte-estimate + resync seek. ---
    Fmt {
        name: "mp1",
        engine: Engine::Pcm,
        sample_rate: 44_100,
        channels: 2,
        lossy: true,
        resolve: true,
        seek_class: SeekClass::Indexed,
        decode: None,
        seek: None,
        cost: Some(CostCase {
            fixture: root!("tone-layer1.mp1"),
            abs80: 20_480,
            pos_factor: 2.0,
        }),
        duration: None,
        declick: None,
    },
];

// ===========================================================================
// RUNNER 1 — decode_vs_reference.
// ===========================================================================

#[test]
fn decode_vs_reference() {
    let mut failures = Vec::new();
    for f in MATRIX {
        let Some(case) = f.decode else { continue };

        // Stream info + (streaming) no-declared-length, mirroring the bespoke
        // decode goldens' own stream_info asserts.
        let (rate, chn, total_none) = big_stack(move || match f.engine {
            Engine::Vorbis => {
                let d = VorbisDecoder::new(SliceSource::new(case.fixture), vec![0u8; 128 * 1024])
                    .expect("open vorbis");
                let si = d.stream_info();
                (si.sample_rate, si.channels, d.total_frames().is_none())
            }
            _ => {
                let d = open(SliceSource::new(case.fixture)).expect("probe + construct");
                let si = d.stream_info();
                (si.sample_rate, si.channels, d.total_frames().is_none())
            }
        });
        if rate != f.sample_rate || chn != f.channels {
            failures.push(format!(
                "{}: stream_info {}Hz/{}ch != expected {}Hz/{}ch",
                f.name, rate, chn, f.sample_rate, f.channels
            ));
        }
        if case.expect_total_none && !total_none {
            failures.push(format!(
                "{}: total_frames() must be None before resolve",
                f.name
            ));
        }

        let ours = decode_all_for(f.engine, case.fixture);
        let refr = match case.reference {
            Reference::OpenPcm(b) => decode_pcm_all(b),
            Reference::S16(b) => load_s16le(b),
        };

        match case.tol {
            Tol::BitExact => {
                if ours.len() != refr.len() {
                    failures.push(format!(
                        "{}: length {} != reference {} (not bit-exact)",
                        f.name,
                        ours.len(),
                        refr.len()
                    ));
                } else if fnv1a(&ours) != fnv1a(&refr) {
                    failures.push(format!("{}: decode != reference (not bit-exact)", f.name));
                }
            }
            Tol::Snr(bar) => {
                let (off, snr) = best_align_segsnr(&ours, &refr, usize::from(f.channels));
                // Fail when SNR doesn't clear the bar — including a NaN SNR
                // (a degenerate decode), which a bare `snr <= bar` would pass.
                if snr <= bar || snr.is_nan() {
                    failures.push(format!(
                        "{}: SNR vs reference {:.1} dB ≤ {:.1} dB bar (offset {} frames)",
                        f.name, snr, bar, off
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "decode_vs_reference failures:\n{}",
        failures.join("\n")
    );
}

// ===========================================================================
// RUNNER 2 — seek_round_trip (sample-accurate, bit-exact tail).
// ===========================================================================

/// Open `bytes` through `open()`, seek to `target`, drain the tail. Returns
/// `(channels, landed, tail)`.
fn pcm_seek_tail(bytes: &'static [u8], target: u64) -> (u16, u64, Vec<i32>) {
    big_stack(move || {
        let mut dec = open(SliceSource::new(bytes)).expect("open");
        let ch = dec.stream_info().channels;
        let landed = dec.seek_to(target).expect("seek");
        (ch, landed, drain_pcm(&mut dec))
    })
}

#[test]
fn seek_round_trip() {
    let mut failures = Vec::new();
    for f in MATRIX {
        let Some(case) = f.seek else { continue };
        let full = decode_pcm_all(case.fixture);

        if case.total_none {
            let declared =
                big_stack(move || open(SliceSource::new(case.fixture)).unwrap().total_frames());
            if declared.is_some() {
                failures.push(format!(
                    "{}: expected total_frames() == None, got {declared:?}",
                    f.name
                ));
            }
        }

        let ch = pcm_seek_tail(case.fixture, 0).0;
        for &target in case.targets {
            let (_, landed, tail) = pcm_seek_tail(case.fixture, target);
            if landed != target {
                failures.push(format!(
                    "{}: seek to {target} landed {landed} (not sample-accurate)",
                    f.name
                ));
                continue;
            }
            let base = target as usize * usize::from(ch);
            if tail.as_slice() != &full[base..] {
                failures.push(format!(
                    "{}: post-seek tail != full decode suffix (target {target})",
                    f.name
                ));
            }
        }

        if case.clamp {
            let total = (full.len() / usize::from(ch)) as u64;
            let (_, landed, tail) = pcm_seek_tail(case.fixture, total + 10_000);
            if landed != total {
                failures.push(format!(
                    "{}: beyond-end seek landed {landed}, expected clamp to {total}",
                    f.name
                ));
            }
            if !tail.is_empty() {
                failures.push(format!(
                    "{}: beyond-end seek yielded {} samples (expected none)",
                    f.name,
                    tail.len()
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "seek_round_trip failures:\n{}",
        failures.join("\n")
    );
}

// ===========================================================================
// RUNNER 3 — seek_cost_bounded (position-independence; printed table).
//
// Ported from the former tests/seek_cost.rs: a CountingSource over an in-memory
// fixture counts bytes pulled forward (read returns + consume amounts; fill_buf
// peeks and seek jumps are NOT forward bytes), seeks to ~20% and ~80% of the
// total (a FRESH decoder each, so each is the full open→depth cost), and asserts
// (i) bytes@80% ≤ abs80 and (ii) bytes@80% ≤ pos_factor × bytes@20%.
// ===========================================================================

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

/// Per-channel total frames (ground truth for the 20%/80% targets), off a plain
/// uncounted source so the end scan never pollutes a metric.
fn cost_total(f: &Fmt, bytes: &'static [u8]) -> u64 {
    match f.engine {
        Engine::Pcm => big_stack(move || {
            let mut dec = open(SliceSource::new(bytes)).expect("open");
            if f.resolve {
                dec.resolve_lengths();
            }
            if let Some(t) = dec.total_frames() {
                return t;
            }
            let ch = u64::from(dec.stream_info().channels.max(1));
            drain_pcm(&mut dec).len() as u64 / ch
        }),
        Engine::Vorbis => big_stack(move || {
            let mut dec = VorbisDecoder::new(SliceSource::new(bytes), vec![0u8; 128 * 1024])
                .expect("open vorbis");
            let ch = u64::from(dec.stream_info().channels.max(1));
            drain_pcm(&mut dec).len() as u64 / ch
        }),
        Engine::Dsf => DsfDecoder::new(SliceSource::new(bytes))
            .expect("open dsf")
            .total_samples()
            .expect("dsf declares total"),
        Engine::Dff => DffDecoder::new(SliceSource::new(bytes))
            .expect("open dff")
            .total_samples()
            .expect("dff declares total"),
    }
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

fn decode_one_block_dsd<D: DsdDecoder>(dec: &mut D) {
    let mut buf = [0u8; 4096];
    for _ in 0..8 {
        if dec.read_dsd(&mut buf).expect("read_dsd") > 0 {
            return;
        }
    }
    panic!("DSD decoder produced no bytes after seek");
}

/// Open over a CountingSource, (resolve), reset, seek, decode ONE block so the
/// seek fully resolves, snapshot. Fresh decoder each call.
fn cost_measure(f: &Fmt, bytes: &'static [u8], target: u64) -> Metric {
    let resolve = f.resolve;
    match f.engine {
        Engine::Pcm => big_stack(move || {
            let c = Arc::new(Counters::default());
            let mut dec = open(CountingSource::new(bytes, c.clone())).expect("open");
            if resolve {
                dec.resolve_lengths();
            }
            c.reset();
            dec.seek_to(target).expect("seek");
            decode_one_block_pcm(&mut dec);
            c.snapshot()
        }),
        Engine::Vorbis => big_stack(move || {
            let c = Arc::new(Counters::default());
            let mut dec =
                VorbisDecoder::new(CountingSource::new(bytes, c.clone()), vec![0u8; 128 * 1024])
                    .expect("open vorbis");
            c.reset();
            SeekableDecoder::seek_to(&mut dec, target).expect("seek");
            decode_one_block_pcm(&mut dec);
            c.snapshot()
        }),
        Engine::Dsf => big_stack(move || {
            let c = Arc::new(Counters::default());
            let mut dec = DsfDecoder::new(CountingSource::new(bytes, c.clone())).expect("open dsf");
            c.reset();
            dec.seek_to_sample(target).expect("seek");
            decode_one_block_dsd(&mut dec);
            c.snapshot()
        }),
        Engine::Dff => big_stack(move || {
            let c = Arc::new(Counters::default());
            let mut dec = DffDecoder::new(CountingSource::new(bytes, c.clone())).expect("open dff");
            c.reset();
            dec.seek_to_sample(target).expect("seek");
            decode_one_block_dsd(&mut dec);
            c.snapshot()
        }),
    }
}

#[test]
fn seek_cost_bounded() {
    println!(
        "\n{:<12} | {:>10} | {:>10} | {:>8} | {:>7} | {:>7} | {:>6}",
        "format", "bytes@20%", "bytes@80%", "ratio", "reads@80", "seeks@80", "file"
    );
    println!("{}", "-".repeat(80));

    let mut failures = Vec::new();
    for f in MATRIX {
        let Some(case) = f.cost else { continue };
        let total = cost_total(f, case.fixture);
        assert!(
            total > 0,
            "{}: zero total frames — fixture decode failed",
            f.name
        );
        let t20 = (total * 20) / 100;
        let t80 = (total * 80) / 100;
        let m20 = cost_measure(f, case.fixture, t20);
        let m80 = cost_measure(f, case.fixture, t80);
        let ratio = m80.bytes as f64 / (m20.bytes.max(1)) as f64;
        println!(
            "{:<12} | {:>10} | {:>10} | {:>8.2} | {:>7} | {:>7} | {:>6}",
            f.name,
            m20.bytes,
            m80.bytes,
            ratio,
            m80.reads,
            m80.seeks,
            case.fixture.len(),
        );

        if m80.bytes > case.abs80 {
            failures.push(format!(
                "{}: bytes@80% {} exceeds absolute bound {} (linear-seek smell; file is {} B)",
                f.name,
                m80.bytes,
                case.abs80,
                case.fixture.len()
            ));
        }
        if m80.bytes as f64 > case.pos_factor * m20.bytes.max(1) as f64 {
            failures.push(format!(
                "{}: NOT position-independent — bytes@80% {} > {} × bytes@20% {} (ratio {:.2}); \
                 a linear O(target) seek lands near 4",
                f.name, m80.bytes, case.pos_factor, m20.bytes, ratio
            ));
        }
    }
    println!();
    assert!(
        failures.is_empty(),
        "seek-cost gate failures:\n{}",
        failures.join("\n")
    );
}

// ===========================================================================
// RUNNER 4 — declick (null-residual + bounded second-difference on real audio).
//
// Ported from the former tests/declick.rs. Drives a real Deck with a real
// decoded lossless-PCM fixture; the seek must reproduce the natural signal
// modulated only by the known smoothstep fade (null-residual, 1 LSB), and the
// splice-window second-difference peak must stay within K× a clean interior
// window's.
// ===========================================================================

const FADE_MS: u32 = 8; // mirrors deck.rs FADE_MS

struct CaptureSink {
    rate: u32,
    ch: u16,
    written: Vec<i32>,
}
impl Sink for CaptureSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.ch,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        self.written.extend_from_slice(frames);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

fn second_difference_peak(xs: &[i32]) -> i64 {
    xs.windows(3)
        .map(|w| (i64::from(w[2]) - 2 * i64::from(w[1]) + i64::from(w[0])).abs())
        .max()
        .unwrap_or(0)
}

fn fade_gain(j: usize, fade_len: usize) -> f32 {
    if j >= fade_len {
        return 1.0;
    }
    let t = j as f32 / fade_len as f32;
    t * t * (3.0 - 2.0 * t)
}

fn apply_fade(natural_sample: i32, g: f32) -> i32 {
    let s = natural_sample as f32 * g;
    let r = if s >= 0.0 { s + 0.5 } else { s - 0.5 };
    r as i32
}

fn channel(xs: &[i32], ch: usize, c: usize) -> Vec<i32> {
    xs.chunks_exact(ch).map(|f| f[c]).collect()
}

fn declick_decoder(bytes: &'static [u8]) -> AnyDecoder<SliceSource<'static>> {
    open(SliceSource::new(bytes)).expect("open declick fixture")
}

fn natural_playthrough(bytes: &'static [u8], rate: u32, ch: u16) -> Vec<i32> {
    let mut deck: Deck<_, _, 1> = Deck::new(
        CaptureSink {
            rate,
            ch,
            written: Vec::new(),
        },
        Quality::Fast,
    );
    deck.load(declick_decoder(bytes)).expect("load");
    for _ in 0..100_000 {
        match deck.pump().expect("pump") {
            Progress::Ended | Progress::Idle => break,
            _ => {}
        }
    }
    deck.sink().written.clone()
}

fn seek_playthrough(bytes: &'static [u8], rate: u32, ch: u16, p: u64, blocks: usize) -> Vec<i32> {
    let mut deck: Deck<_, _, 1> = Deck::new(
        CaptureSink {
            rate,
            ch,
            written: Vec::new(),
        },
        Quality::Fast,
    );
    deck.load(declick_decoder(bytes)).expect("load");
    deck.pump().expect("pre-seek pump");
    let pre = deck.sink().written.len();
    assert_eq!(
        deck.seek(p).expect("seek"),
        p,
        "WAV seek is sample-accurate"
    );
    for _ in 0..blocks {
        deck.pump().expect("post-seek pump");
    }
    deck.sink().written[pre..].to_vec()
}

#[test]
fn declick() {
    for f in MATRIX {
        let Some(case) = f.declick else { continue };
        let fixture = case.fixture;
        big_stack(move || {
            let si = declick_decoder(fixture).stream_info();
            let (rate, chn) = (si.sample_rate, si.channels);
            let ch = usize::from(chn);
            assert_eq!(rate, 44_100);
            assert_eq!(ch, 2);
            let fade_len = ((FADE_MS * rate + 500) / 1000) as usize;

            let natural = natural_playthrough(fixture, rate, chn);
            let total_frames = natural.len() / ch;
            assert!(total_frames > 60_000, "fixture too short: {total_frames}");

            let p = 40_000u64;
            let window = fade_len + 2_000;
            let blocks = window / 256 + 4;
            let post = seek_playthrough(fixture, rate, chn, p, blocks);
            assert!(
                post.len() >= window * ch,
                "post-seek capture too short: {} frames",
                post.len() / ch
            );

            // (1) Null-residual.
            assert_eq!(&post[..ch], &vec![0i32; ch][..], "fade starts at zero");
            let mut max_fade_resid = 0i64;
            for j in 0..window {
                let g = fade_gain(j, fade_len);
                for c in 0..ch {
                    let seek_s = post[j * ch + c];
                    let nat_s = natural[(p as usize + j) * ch + c];
                    if j >= fade_len {
                        assert_eq!(
                            seek_s, nat_s,
                            "post-fade frame {j} ch {c}: seek {seek_s} != natural {nat_s} (a residual = a click)"
                        );
                    } else {
                        let expect = apply_fade(nat_s, g);
                        let resid = i64::from(seek_s) - i64::from(expect);
                        max_fade_resid = max_fade_resid.max(resid.abs());
                        assert!(
                            resid.abs() <= 1,
                            "fade frame {j} ch {c}: seek {seek_s} vs natural·gain {expect} (resid {resid})"
                        );
                    }
                }
            }

            // (2) Second-difference outlier.
            const WIN: usize = 2_048;
            const LEAD: usize = 4;
            const K: i64 = 3;
            let clean_start = 20_000usize;
            let clean = &natural[clean_start * ch..(clean_start + WIN) * ch];
            let mut splice = vec![0i32; LEAD * ch];
            splice.extend_from_slice(&post[..(WIN - LEAD) * ch]);

            let mut clean_d2 = 0i64;
            let mut splice_d2 = 0i64;
            for c in 0..ch {
                clean_d2 = clean_d2.max(second_difference_peak(&channel(clean, ch, c)));
                splice_d2 = splice_d2.max(second_difference_peak(&channel(&splice, ch, c)));
            }

            println!(
                "declick[{}]: fade_len={fade_len} max_fade_residual={max_fade_resid} LSB  \
                 clean_d2={clean_d2}  splice_d2={splice_d2}  ratio={:.3}  (K={K})",
                f.name,
                splice_d2 as f64 / clean_d2.max(1) as f64
            );

            assert!(
                clean_d2 > 100,
                "clean-window d2 {clean_d2} is ~0 — fixture not tonal enough"
            );
            assert!(
                splice_d2 <= K * clean_d2,
                "splice d2 {splice_d2} exceeds {K}× the clean d2 {clean_d2} — a slope break (click)"
            );
        });
    }
}

// ===========================================================================
// RUNNER 5 — duration (resolve_lengths ⇒ total_frames ≈ the decoded length).
// ===========================================================================

#[test]
fn duration() {
    let mut failures = Vec::new();
    for f in MATRIX {
        let Some(case) = f.duration else { continue };

        let (pcm, chn) = big_stack(move || {
            let mut dec = open(SliceSource::new(case.fixture)).expect("open");
            let pcm = drain_pcm(&mut dec);
            (pcm, dec.stream_info().channels)
        });
        let decoded = (pcm.len() / usize::from(chn.max(1))) as u64;

        let total = big_stack(move || {
            let mut dec = open(SliceSource::new(case.fixture)).expect("open");
            dec.resolve_lengths();
            dec.total_frames()
        });
        let Some(total) = total else {
            failures.push(format!(
                "{}: total_frames() is None after resolve_lengths()",
                f.name
            ));
            continue;
        };
        let diff = total.abs_diff(decoded);
        if diff > case.tol_frames {
            failures.push(format!(
                "{}: total_frames() {} differs from decoded {} by {} (> tol {})",
                f.name, total, decoded, diff, case.tol_frames
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "duration failures:\n{}",
        failures.join("\n")
    );
}
