//! Robustness sweep: the decoders must never **panic** or **hang** on malformed
//! input — on the embedded target a panic aborts and an unbounded loop wedges the
//! device, so graceful `Err`/`Ok(0)` is a hard requirement, not a nicety.
//!
//! The crate is `#![forbid(unsafe_code)]`, so there is no UB to chase; the failure
//! modes are index-out-of-bounds, `unwrap`, debug-mode arithmetic overflow, and
//! non-terminating decode loops. This test feeds a large deterministic corpus of
//! truncated / bit-flipped / random inputs (each also prefixed with real container
//! magic to drive the parsers past their sync checks) to every decoder entry
//! point, catching panics and capping iteration to catch hangs. Passing means the
//! whole sweep completed with zero panics and every decode made progress.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::formats::flac::FlacDecoder;
use soul_audio_core::formats::mp3::Mp3Decoder;
use soul_audio_core::formats::opus::OpusDecoder;
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::formats::wav::WavDecoder;
use soul_audio_core::{
    DecimateAdapter, Decoder, DopAdapter, DsdDecoder, DsdSeek, SeekableDecoder, SliceSource, open,
};

/// Deterministic xorshift64* PRNG (no external dev-deps; reproducible corpus).
struct Rng(u64);
impl Rng {
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

/// Cap on `decode_into`/`read_dsd` calls. Every frame consumes ≥1 byte of the
/// finite `SliceSource`, so a decoder that yields more non-empty reads than the
/// input length (plus slack) is not making progress — a hang, which we flag.
fn cap(len: usize) -> usize {
    len + 4096
}

/// Drive a PCM `Decoder` to exhaustion under the progress cap. Returns `false` if
/// the cap was hit (a suspected hang).
fn drain_pcm(dec: &mut dyn Decoder, len: usize) -> bool {
    let mut buf = [0i32; 2048];
    for _ in 0..cap(len) {
        match dec.decode_into(&mut buf) {
            Ok(0) | Err(_) => return true,
            Ok(_) => {}
        }
    }
    false
}

/// Drive a raw DSD reader to exhaustion under the progress cap.
fn drain_dsd(dec: &mut dyn DsdDecoder, len: usize) -> bool {
    let mut buf = [0u8; 4096];
    for _ in 0..cap(len) {
        match dec.read_dsd(&mut buf) {
            Ok(0) | Err(_) => return true,
            Ok(_) => {}
        }
    }
    false
}

/// Seek targets: start, a mid position, and the overflow-prone extreme.
const SEEK_TARGETS: [u64; 3] = [0, 44_100, u64::MAX];

/// Bounded post-seek decode probe (a seek bug shows up in the seek call itself or
/// the first reads after it — no need to drain fully).
fn probe_pcm(dec: &mut dyn Decoder) {
    let mut buf = [0i32; 512];
    for _ in 0..8 {
        match dec.decode_into(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}
fn probe_dsd(dec: &mut dyn DsdDecoder) {
    let mut buf = [0u8; 1024];
    for _ in 0..8 {
        match dec.read_dsd(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// Every decoder entry point exercised for one input. `label` on any hang tells us
/// which path failed to terminate.
fn drive_all(bytes: &[u8]) -> Result<(), &'static str> {
    let len = bytes.len();

    // Dispatch path (probe → AnyDecoder).
    if let Ok(mut dec) = open(SliceSource::new(bytes)) {
        if !drain_pcm(&mut dec, len) {
            return Err("open");
        }
    }
    // Each PCM codec directly (bypassing probe, so the codec sees the raw bytes).
    if let Ok(mut d) = WavDecoder::new(SliceSource::new(bytes)) {
        if !drain_pcm(&mut d, len) {
            return Err("wav");
        }
    }
    if let Ok(mut d) = FlacDecoder::new(SliceSource::new(bytes)) {
        if !drain_pcm(&mut d, len) {
            return Err("flac");
        }
    }
    if let Ok(mut d) = Mp3Decoder::new(SliceSource::new(bytes)) {
        if !drain_pcm(&mut d, len) {
            return Err("mp3");
        }
    }
    // DSD: raw reader plus both PCM-producing adapters.
    if let Ok(mut d) = DsfDecoder::new(SliceSource::new(bytes)) {
        if !drain_dsd(&mut d, len) {
            return Err("dsf");
        }
    }
    if let Ok(mut d) = DffDecoder::new(SliceSource::new(bytes)) {
        if !drain_dsd(&mut d, len) {
            return Err("dff");
        }
    }
    if let Ok(inner) = DsfDecoder::new(SliceSource::new(bytes)) {
        if !drain_pcm(&mut DecimateAdapter::new(inner), len) {
            return Err("dsf-decimate");
        }
    }
    if let Ok(inner) = DffDecoder::new(SliceSource::new(bytes)) {
        if !drain_pcm(&mut DopAdapter::new(inner), len) {
            return Err("dff-dop");
        }
    }
    // Opus (Ogg): the whole range-decoder → CELT/SILK → resampler pipeline, which
    // has no error channel in the CELT core — any out-of-range decoded value must
    // stay a bounded `Ok`/graceful `Err`, never a debug-assert panic or OOB.
    if let Ok(mut d) = OpusDecoder::new(SliceSource::new(bytes)) {
        if !drain_pcm(&mut d, len) {
            return Err("opus");
        }
    }
    // Vorbis (Ogg): needs a caller arena. Fuzzes the audio-packet value decode
    // (residue scalar-Huffman / VQ, floor-1 synthesis, inverse coupling, IMDCT)
    // that setup-header validation can't backstop.
    if let Ok(mut d) = VorbisDecoder::new(SliceSource::new(bytes), vec![0u8; 128 * 1024]) {
        if !drain_pcm(&mut d, len) {
            return Err("vorbis");
        }
    }

    // --- seek surface: seek to each target (incl. overflow-prone extremes) on a
    // fresh decoder, then probe a few reads. Seek panics fire in the seek call or
    // the first post-seek reads; the whole `drive_all` is under `catch_unwind`.
    for &t in &SEEK_TARGETS {
        if let Ok(mut d) = WavDecoder::new(SliceSource::new(bytes)) {
            let _ = d.seek_to(t);
            probe_pcm(&mut d);
        }
        if let Ok(mut d) = FlacDecoder::new(SliceSource::new(bytes)) {
            let _ = d.seek_to(t);
            probe_pcm(&mut d);
        }
        if let Ok(mut d) = Mp3Decoder::new(SliceSource::new(bytes)) {
            let _ = d.seek_to(t);
            probe_pcm(&mut d);
        }
        if let Ok(mut d) = DsfDecoder::new(SliceSource::new(bytes)) {
            let _ = d.seek_to_sample(t);
            probe_dsd(&mut d);
        }
        if let Ok(mut d) = DffDecoder::new(SliceSource::new(bytes)) {
            let _ = d.seek_to_sample(t);
            probe_dsd(&mut d);
        }
        if let Ok(inner) = DffDecoder::new(SliceSource::new(bytes)) {
            let mut a = DopAdapter::new(inner);
            let _ = a.seek_to_frame(t);
            probe_pcm(&mut a);
        }
        if let Ok(inner) = DsfDecoder::new(SliceSource::new(bytes)) {
            let mut a = DecimateAdapter::new(inner);
            let _ = a.seek_to_frame(t);
            probe_pcm(&mut a);
        }
        // Dispatch-level seek: reaches ALAC/AAC/Opus/OggFlac seek on mutated
        // input — i.e. the MP4 `stsc`/`stsz`/`stco` sample-table arithmetic and
        // the Ogg-granule bisection running over corrupt streams, a surface the
        // per-codec seeks above don't touch.
        if let Ok(mut d) = open(SliceSource::new(bytes)) {
            let _ = d.seek_to(t);
            probe_pcm(&mut d);
        }
        // Vorbis seek (not routed through `open`): Ogg-granule bisection + the
        // decode-discard-to-target value path on a fuzzed stream.
        if let Ok(mut d) = VorbisDecoder::new(SliceSource::new(bytes), vec![0u8; 128 * 1024]) {
            let _ = SeekableDecoder::seek_to(&mut d, t);
            probe_pcm(&mut d);
        }
    }
    // Time-based seeks: the edge cases (start, negative, non-finite).
    for &secs in &[0.0f64, -1.0, f64::NAN] {
        if let Ok(mut d) = WavDecoder::new(SliceSource::new(bytes)) {
            let _ = d.seek_to_time(secs);
            probe_pcm(&mut d);
        }
        if let Ok(mut d) = FlacDecoder::new(SliceSource::new(bytes)) {
            let _ = d.seek_to_time(secs);
            probe_pcm(&mut d);
        }
        if let Ok(mut d) = Mp3Decoder::new(SliceSource::new(bytes)) {
            let _ = d.seek_to_time(secs);
            probe_pcm(&mut d);
        }
    }
    Ok(())
}

thread_local! {
    /// Source location of the most recent panic, recorded by the sweep's hook.
    static LAST_LOC: RefCell<String> = const { RefCell::new(String::new()) };
}

/// A recorded failure: which input and which decoder path panicked or hung.
struct Failure {
    seed: &'static str,
    mutation: String,
    kind: String,
    detail: String,
    input_hex: String,
}

fn hex(bytes: &[u8]) -> String {
    let n = bytes.len().min(96);
    let mut s = String::new();
    for b in &bytes[..n] {
        s.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > n {
        s.push_str("...");
    }
    s
}

fn check(failures: &mut Vec<Failure>, seed: &'static str, mutation: String, bytes: &[u8]) {
    let outcome = catch_unwind(AssertUnwindSafe(|| drive_all(bytes)));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(path)) => failures.push(Failure {
            seed,
            mutation,
            kind: format!("hang in {path}"),
            detail: String::new(),
            input_hex: hex(bytes),
        }),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "?".to_string());
            let loc = LAST_LOC.with(|l| l.borrow().clone());
            failures.push(Failure {
                seed,
                mutation,
                kind: "PANIC".to_string(),
                detail: format!("{loc}: {msg}"),
                input_hex: hex(bytes),
            });
        }
    }
}

/// Valid seed corpus: real fixtures spanning every container + codec.
///
/// The HE-AACv2 Parametric Stereo `parse_extensions`/`parse_frame` `[NOPANIC]`
/// path is reachable only inside an SBR-active frame (PS lives in the trailing
/// `bs_extension` structure of the SBR fill-element payload). The committed
/// `sbr-m4a`/`sbr-adts` seeds carry exactly that region, so the sweep's
/// truncations and bit-flips over it drive `bs_extended_data`/`cnt`/
/// `bs_extension_id` and — whenever a mutation routes into `EXTENSION_ID_PS` —
/// every §8 PS field (`iid_mode`/`icc_mode`/`frame_class`/`num_env`/
/// `border_position`/Huffman deltas/`ps_extension_size`) to `Err`/disable-PS,
/// never a panic/OOB/hang. A committed, *valid-baseline* PS bitstream is not
/// added: reaching `parse_frame` from the public API needs a working SBR header
/// and data, and injecting a conformant PS sub-extension at the
/// (non-byte-aligned) `bs_extended_data` offset inside compressed SBR data is
/// impractical without decoder instrumentation. The gitignored `ps-local.*`
/// fixture, when present, is added below as a genuine valid-baseline PS seed
/// (local-only, never committed — `std::fs::read`, never `include_bytes!`).
fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let mut v: Vec<(&'static str, Vec<u8>)> = vec![
        ("wav-min", minimal_wav()),
        ("flac-min", minimal_flac()),
        (
            "l2-mono",
            include_bytes!("../testdata/l2_m44_128.mp2").to_vec(),
        ),
        (
            "l2-stereo",
            include_bytes!("../testdata/l2_s44_128.mp2").to_vec(),
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
            "adpcm-mono",
            include_bytes!("../testdata/ima-adpcm-mono.wav").to_vec(),
        ),
        (
            "adpcm-stereo",
            include_bytes!("../testdata/ima-adpcm-stereo.wav").to_vec(),
        ),
        (
            "dff",
            include_bytes!("../testdata/dst-64fs44-2ch.slice.dff").to_vec(),
        ),
        ("alac", include_bytes!("../testdata/alac.m4a").to_vec()),
        ("aac-m4a", include_bytes!("../testdata/aac.m4a").to_vec()),
        ("aac-adts", include_bytes!("../testdata/aac.aac").to_vec()),
        // Fragmented AAC (empty_moov + moof/mdat fragments): truncations and
        // bit-flips land in the inter-fragment box walk (`advance_to_next_mdat`)
        // — a corrupt `moof` length or a mid-box EOF must end the stream cleanly
        // (Err/short decode), never a panic, OOB, or runaway skip.
        (
            "frag-aac-m4a",
            include_bytes!("../testdata/frag-aac.m4a").to_vec(),
        ),
        // 5.1 AAC-LC: mutations drive the multichannel element loop (SCE/CPE/LFE
        // sequence, per-CPE M/S, channel counts up toward MAX_CHANNELS) that the
        // stereo seeds never reach — every out-of-range element tag/count must
        // stay a bounded Err, never a panic or OOB channel index.
        (
            "aac-5p1-m4a",
            include_bytes!("../testdata/aac-5p1.m4a").to_vec(),
        ),
        // HE-AAC (SBR): mutating the fill-element SBR payload exercises the
        // sbr_header/grid/envelope-Huffman bounds checks → Err/upsample-only
        // fallback, never a panic/OOB (the §3.3 firmware invariant). The same
        // region carries the trailing `bs_extension` structure, so these seeds
        // also drive the PS `parse_extensions`/`parse_frame` `[NOPANIC]` path
        // (see the `seeds()` doc-comment).
        ("sbr-m4a", include_bytes!("../testdata/sbr.m4a").to_vec()),
        ("sbr-adts", include_bytes!("../testdata/sbr.aac").to_vec()),
        // Opus + Vorbis: the previously-unfuzzed Ogg codec bodies. Mutating a
        // valid baseline drives the range-decoder/CELT/SILK (Opus) and the
        // residue/floor/IMDCT value decode (Vorbis) to `Err`/`Ok(0)`, never a
        // panic — the surfaces the setup/TOC unit tests can't reach.
        (
            "opus",
            include_bytes!("../../../testdata/tone.opus").to_vec(),
        ),
        (
            "vorbis-mono",
            include_bytes!("../testdata/tone-vorbis-mono.ogg").to_vec(),
        ),
        (
            "vorbis-blocks",
            include_bytes!("../testdata/tone-vorbis-blocks.ogg").to_vec(),
        ),
        // Multipage Ogg-FLAC: fuzzes the cross-page packet-continuation (255-byte
        // lacing) path the single-page `ogg-flac` seed can't reach.
        (
            "ogg-flac-multipage",
            include_bytes!("../testdata/flac-in-ogg-multipage.ogg").to_vec(),
        ),
    ];

    // Genuine valid-baseline PS fuzz, local-only: the license-restricted
    // `ps-local.m4a` is gitignored and NEVER committed / NEVER `include_bytes!`d,
    // so read it at runtime and add it only when present (a no-op on CI / any
    // clean checkout). Its mutations perturb real, in-range PS fields, driving
    // the §8 `[NOPANIC]` checks from a working HE-AACv2 baseline.
    let dir = env!("CARGO_MANIFEST_DIR");
    if let Ok(b) = std::fs::read(format!("{dir}/testdata/ps-local.m4a")) {
        v.push(("ps-local", b));
    }

    v
}

/// Container magics — random bodies behind these drive parsers past sync.
const MAGICS: &[&[u8]] = &[
    b"RIFF\x24\x00\x00\x00WAVEfmt ",
    b"RF64\xff\xff\xff\xffWAVEfmt ",
    b"fLaC",
    b"OggS\x00\x02",
    b"OggS\x00\x02____________________________\x7fFLAC",
    b"OggS\x00\x02____________________________OpusHead", // Ogg Opus
    b"DSD ",
    b"FRM8",
    b"ID3\x04\x00\x00",
    b"\xff\xfb\x90\x00",         // MPEG-1 Layer III
    b"\xff\xfd\x80\x04",         // MPEG-1 Layer II
    b"\xff\xfe\xc8\x04",         // MPEG-1 Layer I
    b"\xff\xf3\x00\x00",         // MPEG-2 Layer III
    b"\x00\x00\x00\x18ftypM4A ", // MP4/M4A (ALAC/AAC) — 4-byte box size then `ftyp`
    b"\xff\xf1\x50\x80",         // ADTS AAC-LC, MPEG-4 ID, no CRC (44.1 kHz stereo)
    b"\xff\xf9\x50\x80",         // ADTS AAC-LC, MPEG-2 ID, no CRC (44.1 kHz stereo)
];

fn run_sweep() -> Vec<Failure> {
    let mut failures = Vec::new();
    let mut rng = Rng(0x1234_5678_9abc_def0);

    for (name, data) in seeds() {
        // Whole file (baseline: must decode cleanly).
        check(&mut failures, name, "full".into(), &data);

        // Truncation prefixes (sampled to bound runtime).
        let step = (data.len() / 20).max(1);
        let mut n = 0;
        while n <= data.len() {
            check(&mut failures, name, format!("trunc={n}"), &data[..n]);
            n += step;
        }

        // Single-byte perturbations at sampled positions.
        for _ in 0..20 {
            let mut m = data.clone();
            if m.is_empty() {
                break;
            }
            let pos = rng.below(m.len());
            m[pos] = m[pos].wrapping_add(rng.byte() | 1); // odd delta ⇒ always changes
            check(&mut failures, name, format!("flip@{pos}"), &m);
        }

        // Multi-byte random splatter over a copy.
        for _ in 0..8 {
            let mut m = data.clone();
            for _ in 0..(1 + rng.below(16)) {
                if m.is_empty() {
                    break;
                }
                let pos = rng.below(m.len());
                m[pos] = rng.byte();
            }
            check(&mut failures, name, "splatter".into(), &m);
        }
    }

    // Magic-prefixed random buffers: drive the parsers with plausible headers and
    // hostile bodies of varied lengths.
    for magic in MAGICS {
        for _ in 0..60 {
            let body = 4 + rng.below(1024);
            let mut buf = magic.to_vec();
            buf.extend((0..body).map(|_| rng.byte()));
            check(&mut failures, "magic", "prefixed".into(), &buf);
        }
    }

    // Pure random buffers (short and long).
    for _ in 0..400 {
        let n = rng.below(4096);
        let buf: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
        check(&mut failures, "random", "junk".into(), &buf);
    }

    failures
}

#[test]
fn decoders_never_panic_or_hang_on_malformed_input() {
    // Replace the panic hook: `drive_all` runs under `catch_unwind`, so caught
    // panics are expected data points, not noise. The hook records each panic's
    // source location (into thread-local storage on the panicking thread) and
    // prints nothing — a per-panic backtrace would swamp output and slow the sweep.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        LAST_LOC.with(|l| *l.borrow_mut() = loc);
    }));

    // Run on a large stack: the FLAC variant of AnyDecoder is big (multichannel
    // planar buffers), so a corrupt high-channel header could otherwise overflow
    // the default test-thread stack — a false positive unrelated to robustness.
    let handle = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run_sweep)
        .unwrap();
    let failures = handle.join().expect("sweep thread panicked");

    std::panic::set_hook(prev);

    if !failures.is_empty() {
        let shown = failures.len().min(20);
        let mut msg = format!("{} malformed inputs panicked/hung:\n", failures.len());
        for f in failures.iter().take(shown) {
            msg.push_str(&format!(
                "  [{}] {} — {} {}\n    input={}\n",
                f.seed, f.mutation, f.kind, f.detail, f.input_hex
            ));
        }
        panic!("{msg}");
    }
}

// --- minimal valid seeds -----------------------------------------------------

fn minimal_wav() -> Vec<u8> {
    let data: [u8; 8] = [0, 0, 1, 0, 2, 0, 3, 0]; // four 16-bit mono samples
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
    let v: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36); // ch-1=1, bps-1=15
    si[10..18].copy_from_slice(&v.to_be_bytes());
    let mut b = Vec::new();
    b.extend_from_slice(b"fLaC");
    b.push(0x80); // last block + STREAMINFO
    b.extend_from_slice(&[0, 0, 34]);
    b.extend_from_slice(&si);
    b
}
