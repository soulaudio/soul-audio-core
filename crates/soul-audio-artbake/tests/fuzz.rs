//! Deterministic mutation fuzzer for the **cover-art blob reader**.
//!
//! Invariant: [`ArtBlobReader::new`] plus [`ArtStore::art`] must **never panic**
//! for any blob bytes, any art id, and any caller buffer — the allowed outcomes
//! are `Ok(n)` with `n <= buf.len()`, or `Err(OutOfRange/Truncated/Corrupt)`.
//!
//! Same justification as the `.soulidx` fuzzer in `soul-audio-index`: this is a
//! file **on a removable SD card**, read by the browse UI for every album it
//! draws, on a device where a panic is a dead player. The two files are baked
//! together and corrupt together — a truncated sync writes a short art blob
//! exactly as easily as a short index.
//!
//! Unlike the index reader, this one was already written with `u64` entry
//! arithmetic and bounds checks, so this gate found nothing when it was added.
//! That is worth having anyway and worth saying plainly: the gate's value is
//! that the next change to this reader cannot quietly introduce what #153 and
//! #154 found elsewhere, and a clean first run is evidence, not a wasted file.
//!
//! No `build` feature needed: the format is a 16-byte header plus fixed
//! 8-byte entries, so the seeds are built here directly (the index fuzzer
//! bakes with the real baker because *its* layout — string heap, order tables,
//! per-album runs — is far too intricate to hand-roll honestly).

use std::panic::{AssertUnwindSafe, catch_unwind};

use soul_audio_artbake::{ArtBlobReader, ENTRY_LEN, HEADER_LEN, row_bytes};
use soul_audio_catalog::{ArtStore, CatalogError};
use soul_audio_core::SliceSource;

/// Fixed seed for the deterministic CI run.
const CI_SEED: u64 = 0x5341_5254_0001;

/// CI iterations. Each drives ~30 reads over a mutated blob — milliseconds.
const CI_ITERS: u64 = 20_000;

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        // SplitMix64 first: `seed | 1` alone would make consecutive seeds the
        // same corpus (see the sibling fuzzers), and CI seeds from a run number.
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
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Build a valid blob: header, one 8-byte entry per bitmap, then the bitmaps.
fn blob(width: u16, height: u16, bitmaps: &[Vec<u8>]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"SART");
    v.push(1); // version
    v.push(0); // flags
    v.extend_from_slice(&width.to_le_bytes());
    v.extend_from_slice(&height.to_le_bytes());
    v.extend_from_slice(&(bitmaps.len() as u32).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes()); // pad → HEADER_LEN
    debug_assert_eq!(v.len(), HEADER_LEN);
    let mut off = (HEADER_LEN + bitmaps.len() * ENTRY_LEN) as u32;
    for b in bitmaps {
        v.extend_from_slice(&off.to_le_bytes());
        v.extend_from_slice(&(b.len() as u32).to_le_bytes());
        off += b.len() as u32;
    }
    for b in bitmaps {
        v.extend_from_slice(b);
    }
    v
}

fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let full = {
        let w = 64u16;
        let h = 64u16;
        let one = vec![0xA5u8; row_bytes(w) * h as usize];
        let two = vec![0x3Cu8; row_bytes(w) * h as usize];
        blob(w, h, &[one, two])
    };
    vec![
        ("two-covers", full),
        ("one-cover", blob(8, 2, &[vec![0b1010_1010, 0b0101_0101]])),
        ("no-covers", blob(64, 64, &[])),
        ("odd-dims", blob(1, 1, &[vec![0x80]])),
        // A bitmap shorter than width×height implies — the shape a half-written
        // sync leaves, and the one a drawing loop is most likely to trust.
        ("short-bitmap", blob(64, 64, &[vec![0xFF; 8]])),
        (
            "header-only",
            b"SART\x01\x00\x40\x00\x40\x00\x02\x00\x00\x00\x00\x00".to_vec(),
        ),
        ("magic-only", b"SART".to_vec()),
        ("nothing", Vec::new()),
    ]
}

/// Header-biased corruption: the count, the dimensions, and the per-entry
/// `(offset, len)` pairs are every number the reader computes with.
fn corrupt_fields(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.len() < 8 {
        return v;
    }
    for _ in 0..rng.range(1, 5) {
        let width = if rng.below(4) == 0 { 2 } else { 4 };
        if v.len() <= width {
            break;
        }
        // Bias into the header + entry table, which is where the arithmetic is.
        let hot = (HEADER_LEN + 4 * ENTRY_LEN).min(v.len() - width);
        let at = if rng.below(5) < 4 {
            rng.below(hot.max(1))
        } else {
            rng.below(v.len() - width)
        };
        let val: u32 = match rng.below(7) {
            0 => u32::MAX,
            1 => 0,
            2 => u32::MAX - 1,
            3 => 0x7FFF_FFFF,
            4 => 1,
            5 => 0xFFFF_0000,
            _ => rng.next() as u32,
        };
        let b = val.to_le_bytes();
        v[at..at + width].copy_from_slice(&b[..width]);
    }
    v
}

fn mutate(rng: &mut Rng, corpus: &[(&'static str, Vec<u8>)]) -> Vec<u8> {
    let (_, base) = &corpus[rng.below(corpus.len())];
    let mut v = base.clone();
    match rng.below(6) {
        0 => return corrupt_fields(rng, base),
        1 => {
            for _ in 0..rng.range(1, 16) {
                if v.is_empty() {
                    break;
                }
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
        }
        2 => {
            for _ in 0..rng.range(1, 16) {
                if v.is_empty() {
                    break;
                }
                let i = rng.below(v.len());
                v[i] = rng.byte();
            }
        }
        3 => v.truncate(rng.below(v.len() + 1)),
        4 => {
            if !v.is_empty() {
                let at = rng.below(v.len());
                let len = rng.below(v.len() - at);
                for b in &mut v[at..at + len] {
                    *b = 0;
                }
            }
        }
        _ => {
            let (_, other) = &corpus[rng.below(corpus.len())];
            let cut = rng.below(v.len() + 1);
            v.truncate(cut);
            v.extend_from_slice(other);
        }
    }
    v
}

/// Everything the browse UI does with a blob: open it, read its dimensions, and
/// pull covers into a buffer — at every id and buffer size that matters.
fn drive(bytes: &[u8]) {
    let r = ArtBlobReader::new(SliceSource::new(bytes));
    let (w, h) = r.dimensions();
    let count = r.count();
    // A caller sizes its buffer from the declared dimensions, so compute that
    // the same way (and don't let a `u16::MAX` square ask for 500 MB).
    let implied = row_bytes(w).saturating_mul(h as usize).min(1 << 16);

    let mut big = vec![0u8; implied.max(1)];
    let mut exact = [0u8; 512];
    let mut tiny = [0u8; 1];
    let mut none: [u8; 0] = [];

    for &id in &[
        0,
        1,
        count,
        count.saturating_add(1),
        count.saturating_sub(1),
        u32::MAX / 8,
        u32::MAX / 2,
        u32::MAX,
    ] {
        for (buf, label) in [
            (&mut big[..], "implied"),
            (&mut exact[..], "512"),
            (&mut tiny[..], "1"),
            (&mut none[..], "0"),
        ] {
            let cap = buf.len();
            match r.art(id, buf) {
                Ok(n) => {
                    assert!(
                        n <= cap,
                        "art({id}) into a {label} buffer reported {n} > {cap} bytes"
                    );
                    // Id 0 is "no art" by contract, not a lookup.
                    if id == 0 {
                        assert_eq!(n, 0, "art id 0 must read as absent");
                    }
                }
                Err(CatalogError::OutOfRange | CatalogError::Truncated | CatalogError::Corrupt) => {
                }
                Err(CatalogError::Io) => {}
            }
        }
    }
}

#[test]
fn no_art_blob_bytes_can_panic_the_reader() {
    let seed = env_u64("ART_FUZZ_SEED").unwrap_or(CI_SEED);
    let iters = env_u64("ART_FUZZ_ITERS").unwrap_or(CI_ITERS);
    let corpus = seeds();
    let mut rng = Rng::new(seed);

    for (name, bytes) in &corpus {
        if catch_unwind(AssertUnwindSafe(|| drive(bytes))).is_err() {
            panic!("seed blob {name} panicked ({} bytes)", bytes.len());
        }
    }
    for i in 0..iters {
        let bytes = mutate(&mut rng, &corpus);
        if catch_unwind(AssertUnwindSafe(|| drive(&bytes))).is_err() {
            let hex: String = bytes.iter().take(96).map(|b| format!("{b:02x}")).collect();
            panic!(
                "iteration {i} (seed {seed:#x}) panicked on {} bytes: {hex}…",
                bytes.len()
            );
        }
    }
}

/// An unmutated blob must still *work* — otherwise "no panics" could be
/// satisfied by a reader that refuses everything, and the gate would quietly
/// stop testing anything.
#[test]
fn the_unmutated_seed_is_a_working_blob() {
    let w = 8u16;
    let h = 2u16;
    let first = vec![0b1010_1010u8, 0b0101_0101];
    let second = vec![0xFFu8, 0x00];
    let bytes = blob(w, h, &[first.clone(), second.clone()]);
    let r = ArtBlobReader::new(SliceSource::new(&bytes));

    assert_eq!(r.dimensions(), (w, h));
    assert_eq!(r.count(), 2);

    let mut buf = [0u8; 16];
    assert_eq!(r.art(0, &mut buf), Ok(0), "id 0 is 'no art'");
    let n = r.art(1, &mut buf).expect("first cover");
    assert_eq!(&buf[..n], &first[..]);
    let n = r.art(2, &mut buf).expect("second cover");
    assert_eq!(&buf[..n], &second[..]);
    assert_eq!(r.art(3, &mut buf), Err(CatalogError::OutOfRange));
    // A buffer smaller than the cover is refused, not truncated silently.
    let mut small = [0u8; 1];
    assert_eq!(r.art(1, &mut small), Err(CatalogError::Truncated));
}

#[test]
fn generator_is_deterministic_and_seeds_do_not_collide() {
    let corpus = seeds();
    let fingerprint = |seed: u64| -> Vec<(usize, u64)> {
        let mut rng = Rng::new(seed);
        (0..256)
            .map(|_| {
                let b = mutate(&mut rng, &corpus);
                let h = b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| {
                    (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01B3)
                });
                (b.len(), h)
            })
            .collect()
    };
    assert_eq!(fingerprint(CI_SEED), fingerprint(CI_SEED));
    for base in [CI_SEED, 0, 1, 2, 1000] {
        assert_ne!(
            fingerprint(base),
            fingerprint(base + 1),
            "seeds {base} and {} generate the same inputs",
            base + 1
        );
    }
}
