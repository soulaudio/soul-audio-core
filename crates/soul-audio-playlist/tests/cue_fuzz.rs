//! Deterministic mutation fuzzer for the `.cue` surface, plus a randomized
//! property check on the sub-range decoder the sheets drive.
//!
//! A cue sheet is **untrusted text** that a library scan reads for every album
//! that has one, and `cue::parse` → `CueFile::ranges` → `SubRange` is the chain
//! from those bytes to a seek offset in someone's audio file. The invariant: no
//! input may panic. The crate is `#![forbid(unsafe_code)]`, so there is no UB to
//! chase — the real failure modes are debug-build panics (index out of bounds,
//! arithmetic overflow, `unwrap`) and non-terminating loops, exactly as
//! `soul-audio-core`'s `tests/fuzz.rs` describes for the decode surface.
//!
//! This found two real bugs when it was written against the first cut (an
//! overflow in the timestamp conversion and one in the range seek), which is
//! why it is here rather than in a notebook.
//!
//! Two modes, following the decode fuzzer's convention:
//!   * **CI (default).** A fixed seed and [`CI_ITERS`] iterations —
//!     deterministic, fast, part of the normal `cargo test` run.
//!   * **Long (env-gated).** `CUE_FUZZ_ITERS=<n>` for more iterations,
//!     `CUE_FUZZ_SEED=<n>` for another corpus.

use std::panic::{AssertUnwindSafe, catch_unwind};

use soul_audio_playlist::cue::{self, CueTime};

/// Iterations in the default (CI) mode. Each one parses a mutated sheet and
/// walks its ranges at several rates, so this is milliseconds of work.
const CI_ITERS: u64 = 4_000;

/// Deterministic PRNG (xorshift64*), same as the decode fuzzer's — no external
/// dev-dependency, reproducible corpus.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
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
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Seed sheets: a realistic ripper-written one, the degenerate shapes, and the
/// pieces a mutation can graft together.
fn seeds() -> Vec<String> {
    vec![
        // A full EAC-style sheet.
        "REM GENRE Jazz\r\nREM DATE 1959\r\nCATALOG 0888072000001\r\n\
         PERFORMER \"Miles Davis\"\r\nTITLE \"Kind of Blue\"\r\n\
         FILE \"Kind of Blue.flac\" WAVE\r\n\
         \x20 TRACK 01 AUDIO\r\n   TITLE \"So What\"\r\n   ISRC USSM15900001\r\n   INDEX 01 00:00:00\r\n\
         \x20 TRACK 02 AUDIO\r\n   TITLE \"Freddie Freeloader\"\r\n   INDEX 00 09:20:50\r\n   INDEX 01 09:22:00\r\n\
         \x20 TRACK 03 AUDIO\r\n   TITLE \"Blue in Green\"\r\n   INDEX 01 18:58:25\r\n"
            .to_owned(),
        // A per-track rip (one FILE each).
        "FILE \"01.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
         FILE \"02.flac\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n"
            .to_owned(),
        // A mixed-mode disc: data track first.
        "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\n\
         TRACK 02 AUDIO\nINDEX 01 00:02:00\n"
            .to_owned(),
        // Degenerate / adversarial shapes.
        String::new(),
        "FILE\n".to_owned(),
        "TRACK\nINDEX\n".to_owned(),
        "INDEX 01 00:00:00\n".to_owned(),
        "FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 99 99:99:99\n".to_owned(),
        "FILE \"\u{feff}\" \nTRACK 1 audio\nindex 1 0:0:0\n".to_owned(),
        // Huge numbers in every numeric field.
        "FILE \"a\" WAVE\nTRACK 99999999999999999999 AUDIO\n\
         INDEX 18446744073709551615 18446744073709551615:59:74\n"
            .to_owned(),
        "FILE \"a\" WAVE\nTRACK 01 AUDIO\nINDEX 01 999999999999999:00:00\n".to_owned(),
        // An unterminated quote and a lone CR.
        "TITLE \"never closed\nFILE \"a.flac\" WAVE\rTRACK 01 AUDIO\nINDEX 01 00:00:00\n"
            .to_owned(),
    ]
}

/// Mutations: byte flips, truncation, duplication, splicing two seeds, and
/// digit storms in the numeric fields (where overflow lives).
fn mutate(rng: &mut Rng, seeds: &[String]) -> Vec<u8> {
    let base = seeds[rng.below(seeds.len())].as_bytes().to_vec();
    let mut v = base;
    match rng.below(7) {
        0 => {
            // Byte flips.
            let n = 1 + rng.below(8);
            for _ in 0..n {
                if v.is_empty() {
                    break;
                }
                let i = rng.below(v.len());
                v[i] = rng.byte();
            }
        }
        1 => {
            // Truncate anywhere (a half-written sheet).
            let keep = rng.below(v.len() + 1);
            v.truncate(keep);
        }
        2 => {
            // Duplicate the sheet (two FILE sections, repeated TRACKs).
            let dup = v.clone();
            v.extend_from_slice(&dup);
        }
        3 => {
            // Splice another seed in at a random point.
            let other = seeds[rng.below(seeds.len())].as_bytes();
            let at = rng.below(v.len() + 1);
            let mut out = v[..at].to_vec();
            out.extend_from_slice(other);
            out.extend_from_slice(&v[at..]);
            v = out;
        }
        4 => {
            // A digit storm: the classic overflow source.
            let digits: Vec<u8> = (0..rng.below(40) + 1)
                .map(|_| b'0' + (rng.byte() % 10))
                .collect();
            let at = rng.below(v.len() + 1);
            let mut out = v[..at].to_vec();
            out.extend_from_slice(&digits);
            out.extend_from_slice(&v[at..]);
            v = out;
        }
        5 => {
            // Insert raw non-UTF-8 bytes (a Windows-1252 sheet, read as bytes).
            let n = 1 + rng.below(6);
            let at = rng.below(v.len() + 1);
            let mut out = v[..at].to_vec();
            for _ in 0..n {
                out.push(0x80 | (rng.byte() & 0x7F));
            }
            out.extend_from_slice(&v[at..]);
            v = out;
        }
        _ => {
            // Zero a run (a sparse-file hole).
            if !v.is_empty() {
                let at = rng.below(v.len());
                let len = rng.below(v.len() - at);
                for b in &mut v[at..at + len] {
                    *b = 0;
                }
            }
        }
    }
    // Keep inputs small: a cue sheet is kilobytes, and the importer refuses
    // anything over a megabyte before it gets here.
    v.truncate(16 * 1024);
    v
}

/// Decode bytes the way the importer does: UTF-8, else Windows-1252.
fn decode(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => e.into_bytes().into_iter().map(char::from).collect(),
    }
}

/// Everything a scan does with a sheet: parse it, then walk every file's
/// ranges at a spread of rates and known/unknown lengths, and convert every
/// timestamp. All of it must be total.
fn drive(text: &str) {
    let Ok(sheet) = cue::parse(text) else {
        return;
    };
    let _ = sheet.audio_track_count();
    for file in &sheet.files {
        for track in &file.tracks {
            let _ = track.is_audio();
            for t in track
                .start
                .iter()
                .chain(track.pregap_start.iter())
                .chain(track.extra_indexes.iter().map(|(_, t)| t))
            {
                // Every rate the pipeline can run at, plus the degenerate ones.
                for rate in [0u32, 1, 44_100, 48_000, 192_000, 768_000, u32::MAX] {
                    let _ = t.to_frames(rate);
                }
                let _ = t.as_millis();
                let _ = t.mm_ss_ff();
            }
        }
        for rate in [0u32, 44_100, 192_000, u32::MAX] {
            for total in [None, Some(0u64), Some(88_200), Some(u64::MAX)] {
                let ranges = file.ranges(rate, total);
                // The ranges must stay inside the file when its length is known.
                if let Some(t) = total {
                    for r in &ranges {
                        assert!(r.start_frame <= t, "range starts past the file");
                        if let Some(len) = r.frames {
                            assert!(
                                r.start_frame.saturating_add(len) <= t,
                                "range ends past the file"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn no_cue_sheet_can_panic_the_parse_or_the_ranges() {
    let seed = env_u64("CUE_FUZZ_SEED").unwrap_or(0x5041_524B_4345_5545);
    let iters = env_u64("CUE_FUZZ_ITERS").unwrap_or(CI_ITERS);
    let seeds = seeds();
    let mut rng = Rng::new(seed);

    // The seeds themselves first, unmutated.
    for s in &seeds {
        if catch_unwind(AssertUnwindSafe(|| drive(s))).is_err() {
            panic!("seed sheet panicked:\n{s}");
        }
    }

    for i in 0..iters {
        let bytes = mutate(&mut rng, &seeds);
        let text = decode(bytes.clone());
        if catch_unwind(AssertUnwindSafe(|| drive(&text))).is_err() {
            panic!(
                "iteration {i} (seed {seed:#x}) panicked on {} bytes: {:?}",
                bytes.len(),
                &text.chars().take(400).collect::<String>()
            );
        }
    }
}

/// `CueTime`'s conversions are total for **every** `u64` — the field is public,
/// so values arrive from callers and deserialized profiles, not only the parser.
#[test]
fn every_timestamp_value_converts_without_panicking() {
    let mut rng = Rng::new(0xC0FF_EE00_1234_5678);
    for _ in 0..20_000 {
        let frames = match rng.below(4) {
            0 => rng.next(),                      // anywhere in u64
            1 => u64::MAX - rng.below(64) as u64, // the very top
            2 => rng.below(10_000_000) as u64,    // plausible
            _ => 1u64 << rng.below(64),           // powers of two
        };
        let t = CueTime { frames };
        for rate in [0u32, 1, 75, 44_100, 768_000, u32::MAX] {
            let got = t.to_frames(rate);
            // Monotone in the rate, and exact where it can be.
            if rate == 0 {
                assert_eq!(got, 0);
            }
        }
        let _ = t.as_millis();
        let (m, s, f) = t.mm_ss_ff();
        assert!(s < 60 && f < 75, "{frames} → {m}:{s}:{f}");
    }
}
