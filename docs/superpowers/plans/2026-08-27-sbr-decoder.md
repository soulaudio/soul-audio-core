# SBR Decoder Implementation Plan (Phase 2 of HE-AACv2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Algorithm bible:** every decoder task must read `.superpowers/sdd/sbr-reference.md` (source-verified SBR reference: bit layouts, tables, formulas, pipeline order, and the exact FFmpeg/faad2 source URLs) at the section it cites (`§N`). The big verbatim tables (QMF prototype filter, noise table, Huffman trees) live in `.superpowers/sdd/sbr-reference-verbatim.md` — **fetch/transcribe them from there + the cited source, with the size + anchor self-checks each table carries**; do NOT hand-type them and do NOT inline them into this plan. This plan gives structure, interfaces, and TDD steps; the reference is the single source of algorithm truth.

**Goal:** Extend the existing `no_std`, no-alloc AAC-LC decoder in `soul-audio-core` with **Spectral Band Replication (SBR)** so it decodes HE-AAC (object type 5, plus implicit backward-compatible signalling) end to end: LC core → SBR high-band reconstruction → PCM at **2× the core sample rate**. Mono + stereo including coupled-channel (`bs_coupling`) mode; explicit (AOT 5 / ASC) and implicit (fill-element discovery) signalling. **Dual-rate (2×) SBR only** (1× out of scope). Complex QMF throughout. The Parametric-Stereo (PS) seam is exposed but no PS is implemented.

**Architecture:** SBR is **additive** to the shipped Phase-1 pipeline. The LC core (`formats/aac/{syntax,dsp,filterbank}.rs`) decodes each channel to 1024 time-domain samples exactly as today. One seam changes: `decode_fil` becomes SBR-aware and, for `EXT_SBR_DATA`/`EXT_SBR_DATA_CRC`, hands the fill-element bits to a new `formats/aac/sbr/` submodule tree instead of skipping them. SBR then runs as an extra stage after IMDCT, per channel: **QMF-analyze** the LC PCM (32-band complex) → **HF-generate** the high band (LPC transposition + chirp) → **HF-adjust** (envelope/noise/sine mapping + gain/limiter) → **assemble** the full 64-band complex matrix (**the PS seam**) → **QMF-synthesize** to 2048 samples/channel. Data dependency is strictly linear: bitstream → freq tables → grid → dtdf/invf → envelope/noise → QMF analysis → HF gen → HF adjust → assemble/synthesize. New submodule tree under `formats/aac/sbr/`, wired from `formats/aac/mod.rs`. SBR const tables are generated host-side by a new `xtask gen-sbr-tables` subcommand (mirroring `gen-filterbank-tables`), CI-guarded by `check-sbr-tables`.

**Tech Stack:** Rust (edition 2024), `#![no_std]` + no-alloc + `#![forbid(unsafe_code)]`. Tests use ffmpeg golden fixtures self-encoded via the Windows MediaFoundation `aac_mf` encoder (which produces HE-AAC/SBR, verified); because SBR reconstruction is lossy and approximate, the golden gate is a **tolerance/segmental-SNR** comparison vs ffmpeg's native decode (calibrated from the measured value, not guessed), backed by a **structural sanity** gate (2× length, non-silence, finite) since no clean tone-FFT backstop is feasible.

## Global Constraints

- `#![no_std]` (outside `test`), no heap allocation in non-test code (fixed by-value buffers only), `#![forbid(unsafe_code)]`.
- **NO `libm`, NO new dependency** (the `architecture-tests` dependency allowlist is `embedded-io`-only). ALL transcendentals — QMF modulation `cos`/`sin`, master-table `log2`/power, noise `V[512]`, gain `sqrt` seed constants — are precomputed `const` tables emitted by `cargo xtask gen-sbr-tables`, guarded by a `check-sbr-tables` gate wired into `cargo xtask verify` (the general `sqrt(x)` over data-dependent domain is a no-`libm` Newton-Raphson **function**, unit-tested against known values — see Task 10).
- **Firmware no-panic invariant:** every SBR bitstream field is bounds-checked → `Err(DecodeError::Malformed)` (or `Unsupported`), never a panic / OOB index / infinite loop. On any SBR-side overrun or validation failure the frame's SBR is discarded and the decoder falls back to **upsample-only** (`kx=32, M=0`) or the last known-good header for that frame — never a hard decode error (reserved for LC-core failure, already handled). This spans `num_env`/`L_E`/`L_Q`, all band counts (`N_master`/`N_high`/`N_low`/`N_Q`/`M`/limiter bands), every frequency-table index (`k0`/`k2`/`kx`/patch indices), Huffman-decoded envelope/noise values (`≤127` / `≤30`), noise/sine flags, `bs_xover_band`, and the SBR extension byte-span clamp.
- Every task must pass, before its commit: `cargo test -p soul-audio-core`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all` (clean), `cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features`, and `cargo test -p architecture-tests`.
- Commit messages carry **NO AI-authorship trailers** (no `Co-Authored-By: Claude`, no `Claude-Session`) — lefthook rejects them.
- Read `.superpowers/sdd/sbr-reference.md` §N (the section each task cites) before writing that task's code. Big tables → fetch from `sbr-reference-verbatim.md` + the cited faad2/FFmpeg source, verify the size + anchor self-checks, do not hand-type.

---

## File structure

- `crates/soul-audio-core/testdata/{sbr.m4a,sbr.m4a.ref.pcm,sbr.aac,sbr.aac.ref.pcm}` (NEW) — self-encoded HE-AAC fixtures + ffmpeg reference PCM.
- `xtask/src/sbr_tables.rs` (NEW) — host-side generator for the SBR const tables (QMF prototype + modulation cos/sin, master-table `log2`/power, noise `V[512]`); `gen` + `check` entry points.
- `xtask/src/main.rs` (MODIFY) — register `gen-sbr-tables` / `check-sbr-tables`; wire `check-sbr-tables` into `verify`.
- `crates/soul-audio-core/src/formats/aac/sbr/mod.rs` (NEW) — `sbr_extension_data` entry point + per-channel `SbrState` + the §10 assembly drive; re-exports.
- `crates/soul-audio-core/src/formats/aac/sbr/tables.rs` (NEW) — the generated const tables (QMF prototype, modulation cos/sin, log2/power, `V[512]`), checked in.
- `crates/soul-audio-core/src/formats/aac/sbr/bitstream.rs` (NEW) — `sbr_extension_data`/fill dispatch, `sbr_data` framing, CRC skip, byte-span clamp, `bs_extension_id` loop.
- `crates/soul-audio-core/src/formats/aac/sbr/header.rs` (NEW) — `sbr_header` + reset/header-change comparison.
- `crates/soul-audio-core/src/formats/aac/sbr/freq_tables.rs` (NEW) — k0/k2/master/derived/noise/limiter frequency tables. **Verify-first.**
- `crates/soul-audio-core/src/formats/aac/sbr/grid.rs` (NEW) — `sbr_grid`, four frame classes, time borders.
- `crates/soul-audio-core/src/formats/aac/sbr/dtdf_invf.rs` (NEW) — `sbr_dtdf` + `invf_mode` + CPE coupling copy.
- `crates/soul-audio-core/src/formats/aac/sbr/huffman.rs` (NEW) — SBR envelope/noise Huffman trees + tree-walk decode.
- `crates/soul-audio-core/src/formats/aac/sbr/envelope.rs` (NEW) — envelope + noise decode, delta resolution, dequant.
- `crates/soul-audio-core/src/formats/aac/sbr/qmf.rs` (NEW) — complex 32-band analysis + 64-band synthesis QMF; defines the `Cpx` complex element. **Verify-first, hardest.**
- `crates/soul-audio-core/src/formats/aac/sbr/hfgen.rs` (NEW) — LPC inverse filter + chirp + patch construction + HF generation MAC.
- `crates/soul-audio-core/src/formats/aac/sbr/hfadj.rs` (NEW) — mapping + env estimate + gain/limiter + noise/sine assembly.
- `crates/soul-audio-core/src/formats/aac/sbr/assemble.rs` (NEW) — low/high splice + PS seam + synthesis drive + coupling un-map.
- `crates/soul-audio-core/src/formats/aac/mod.rs` (MODIFY) — carry `SbrState`, wire the LC-PCM→SBR hand-off in `decode_rdb`, 2× rate promotion in `stream_info`, grow `PCM_CAP`/scratch.
- `crates/soul-audio-core/src/formats/aac/syntax.rs` (MODIFY) — `decode_fil` inspects `extension_type`, dispatches SBR payload to `sbr::`.
- `crates/soul-audio-core/testdata/README.md` (MODIFY) — SBR fixture provenance.
- `crates/soul-audio-core/tests/robustness.rs` (MODIFY) — SBR fuzz seed.

---

## Task 0: Golden fixtures (self-encoded HE-AAC MP4 + ADTS + ffmpeg reference PCM)

**Files:** Create `crates/soul-audio-core/testdata/{sbr.m4a,sbr.m4a.ref.pcm,sbr.aac,sbr.aac.ref.pcm}`; MODIFY `crates/soul-audio-core/testdata/README.md`, `.gitattributes` (if needed).

The `aac_mf` (Windows MediaFoundation) encoder produces HE-AAC (SBR) — verified in this environment: `ffmpeg -i src.wav -c:a aac_mf -b:a 24k out.m4a` yields a stream ffprobe reports as `profile=HE-AAC`, and ffmpeg's native decoder round-trips it to full-rate PCM. This is **low-risk, self-encoded** (like the AAC-LC fixtures, just via `aac_mf`). The source **must be broadband** (real energy above the SBR crossover ~to 16 kHz) — NOT the low 440/659 Hz LC tone — because SBR reconstructs the *high* band and the test must be sensitive to high-band bugs.

- [ ] **Step 1: Generate** (from repo root; write the intermediate WAV to an absolute path — Git Bash `/tmp` isn't visible to native ffmpeg here). Broadband source = a wide linear chirp 200 Hz→16 kHz mixed with mild shaped noise, 0.75 s, 44100 Hz stereo:
```bash
python - <<'PY'
import struct, math, random
rate=44100; dur=0.75; n=int(rate*dur); random.seed(1); d=bytearray()
f0, f1 = 200.0, 16000.0
for i in range(n):
    t = i/rate
    # log chirp sweeps through the SBR crossover into the high band
    inst = f0*((f1/f0)**(t/dur))
    ph = 2*math.pi*f0*dur/math.log(f1/f0)*((f1/f0)**(t/dur)-1)
    chirp = math.sin(ph)
    noise = (random.random()*2-1)*0.15
    s = 0.5*(chirp*0.75 + noise)
    v = max(-1.0, min(1.0, s))
    smp = int(v*32767)
    d += struct.pack('<hh', smp, smp)
hdr=b'RIFF'+struct.pack('<I',36+len(d))+b'WAVE'+b'fmt '+struct.pack('<IHHIIHH',16,1,2,rate,rate*4,4,16)+b'data'+struct.pack('<I',len(d))
open('sbr_src.wav','wb').write(hdr+d)
PY
ffmpeg -y -hide_banner -loglevel error -i sbr_src.wav -c:a aac_mf -b:a 24k -movflags +faststart crates/soul-audio-core/testdata/sbr.m4a
ffmpeg -y -hide_banner -loglevel error -i sbr_src.wav -c:a aac_mf -b:a 24k -f adts crates/soul-audio-core/testdata/sbr.aac
ffmpeg -y -hide_banner -loglevel error -i crates/soul-audio-core/testdata/sbr.m4a -f s16le crates/soul-audio-core/testdata/sbr.m4a.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i crates/soul-audio-core/testdata/sbr.aac -f s16le crates/soul-audio-core/testdata/sbr.aac.ref.pcm
```
- [ ] **Step 2: Verify HE-AAC + SBR present.**
  - `ffprobe -v error -show_entries stream=profile -of default=nw=1 crates/soul-audio-core/testdata/sbr.m4a` prints `profile=HE-AAC` (NOT `LC`). Same for `sbr.aac`.
  - Confirm the stream carries `EXT_SBR_DATA`: `ffprobe -v debug crates/soul-audio-core/testdata/sbr.aac 2>&1 | grep -i sbr` shows SBR activity (or `ffmpeg -v debug -i ... -f null -` mentions "SBR"); alternatively decode both `.m4a` and `.aac` and confirm the ref PCM sample rate is **88200 Hz** (2× core 44100), which only happens when SBR is active. If ffprobe reports the ref rate as 44100, the encoder did NOT emit SBR — lower the bitrate to `-b:a 20k` and retry.
  - Confirm the ref PCM is 16-bit stereo interleaved at the 2× rate. Sizes need not match the source exactly (encoder priming/padding).
  - `.gitattributes`: ensure `*.m4a`, `*.aac`, `*.pcm` are marked `binary` (add `*.aac binary` if missing; `*.m4a`/`*.pcm` already present per the AAC-LC task).
- [ ] **Step 3: Record provenance** in `crates/soul-audio-core/testdata/README.md`: add an SBR section noting the source (broadband chirp+noise 200 Hz→16 kHz, 0.75 s), the `aac_mf -b:a 24k` encode, the confirmed `profile=HE-AAC` + 2× (88200 Hz) decode, and that `sbr.*.ref.pcm` are ffmpeg's native s16le decode used as the conformance oracle.
- [ ] **Step 4: Commit** `test(sbr): self-encoded HE-AAC MP4 + ADTS fixtures + ffmpeg reference PCM` (no AI trailers).

> Note: SBR output is at **2× the core rate**; the reference PCM's sample rate is the primary quick confirmation that SBR is present. Like AAC-LC, HE-AAC decoders emit encoder-delay priming; the conformance test (Task 12) aligns by trimming leading priming and compares the overlapping region.

---

## Task 1: `xtask gen-sbr-tables` / `check-sbr-tables` + runtime table self-check

**Files:** Create `xtask/src/sbr_tables.rs`; MODIFY `xtask/src/main.rs`; Create `crates/soul-audio-core/src/formats/aac/sbr/mod.rs`, `crates/soul-audio-core/src/formats/aac/sbr/tables.rs` (generated, checked in); MODIFY `crates/soul-audio-core/src/formats/aac/mod.rs` (add `pub(crate) mod sbr;`).

Read `.superpowers/sdd/sbr-reference.md` §3.1–§3.3, §7. Exact precedent: `xtask/src/filterbank_tables.rs` (`run`/`check`/`generate`/`emit`/`lit`) + its `main.rs` wiring. This task emits **all** SBR const tables so no downstream task needs `libm`.

**Tables to generate** (host-side `f64` math; write as `const`/`static` arrays into `sbr/tables.rs`, module starts with `#![allow(dead_code)]` — a comment marks it removable once `qmf.rs`/`freq_tables.rs` consume them):
- `QMF_PROTO: [f32; 640]` — the prototype filter `qmf_c`/`sbr_qmf_window_us`. **Fetch verbatim from `sbr-reference-verbatim.md` §1** (do NOT recompute) and paste into the generator as an `f64` literal array; the generator re-emits it. Self-check anchors: `[0]==0`, `[1]==-0.00055252865047`, `[320]==0.85373856005937`, `[638]==-0.00056176925738`, `[639]==-0.00055252865047`, count 640.
- `QMF_ANA_COS: [[f32; 64]; 32]`, `QMF_ANA_SIN: [[f32; 64]; 32]` — `cos/sin(π·(k+0.5)·(2n−95)/64)`, `k=0..31`, `n=0..63` (analysis modulation, §7.1). Exact `2n−95` offset.
- `QMF_SYN_COS: [[f32; 64]; 128]`, `QMF_SYN_SIN: [[f32; 64]; 128]` — `cos/sin(π·(k+0.5)·(2n−63)/128)`, `n=0..127`, `k=0..63` (synthesis modulation, §7.2). Exact `2n−63` offset.
- `V_NOISE: [[f32; 2]; 512]` — the SBR noise table. **Fetch verbatim from `sbr-reference-verbatim.md` §2.** Anchors: `[0]==(-0.99948153278296,-0.59483417516607)`, `[1]==(0.97113454393991,-0.67528515225647)`, `[511]==(0.91599807087376,-0.98147830385781)`, count 512.
- `LOG2_INT: [f32; 65]` — `log2(i)` for `i=0..64` (`[0]` = 0.0, unused). For §3.3's `log2(k1/k0)` = `LOG2_INT[k1]−LOG2_INT[k0]`.
- `POW2_FRAC: [f32; 257]` — `2^(i/256)` for `i=0..256`. For §3.3's geometric spacing `k0·(k1/k0)^(i/N) = k0·2^(((i/N))·log2(k1/k0))`, evaluated as `2^floor(x)·POW2_FRAC[round(frac(x)·256)]`.

- [ ] **Step 1: Failing test** — the runtime self-check in `sbr/tables.rs` (tests may use `std` trig, so re-derive the closed form and compare):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use core::f64::consts::PI;
    #[test]
    fn qmf_proto_anchors() {
        assert_eq!(QMF_PROTO.len(), 640);
        assert_eq!(QMF_PROTO[0], 0.0);
        assert!((QMF_PROTO[320] - 0.85373856005937).abs() < 1e-9);
        assert!((QMF_PROTO[639] - (-0.00055252865047)).abs() < 1e-12);
    }
    #[test]
    fn qmf_analysis_modulation_matches_closed_form() {
        for k in 0..32usize {
            for n in 0..64usize {
                let a = PI * (k as f64 + 0.5) * (2.0 * n as f64 - 95.0) / 64.0;
                assert!((f64::from(QMF_ANA_COS[k][n]) - a.cos()).abs() < 1e-5);
                assert!((f64::from(QMF_ANA_SIN[k][n]) - a.sin()).abs() < 1e-5);
            }
        }
    }
    #[test]
    fn qmf_synthesis_modulation_matches_closed_form() {
        for n in 0..128usize {
            for k in 0..64usize {
                let a = PI * (k as f64 + 0.5) * (2.0 * n as f64 - 63.0) / 128.0;
                assert!((f64::from(QMF_SYN_COS[n][k]) - a.cos()).abs() < 1e-5);
                assert!((f64::from(QMF_SYN_SIN[n][k]) - a.sin()).abs() < 1e-5);
            }
        }
    }
    #[test]
    fn noise_and_log_power_tables() {
        assert_eq!(V_NOISE.len(), 512);
        assert!((f64::from(V_NOISE[0][0]) - (-0.99948153278296)).abs() < 1e-9);
        for i in 1..65usize {
            assert!((f64::from(LOG2_INT[i]) - (i as f64).log2()).abs() < 1e-5);
        }
        for i in 0..257usize {
            assert!((f64::from(POW2_FRAC[i]) - 2f64.powf(i as f64 / 256.0)).abs() < 1e-5);
        }
    }
}
```
- [ ] **Step 2:** Run `cargo test -p soul-audio-core --lib formats::aac::sbr::tables` → FAIL (`sbr` / tables undefined).
- [ ] **Step 3: Implement.**
  - `xtask/src/sbr_tables.rs`: mirror `filterbank_tables.rs` — `const TABLES_PATH = "crates/soul-audio-core/src/formats/aac/sbr/tables.rs"`, a `generate() -> String` that emits `#![allow(dead_code)]` + a `// @generated` banner, then each table via `emit`/`emit2d` helpers (reuse `lit`); `pub fn run()`/`pub fn check()` identical in shape to `filterbank_tables`. Paste `QMF_PROTO` and `V_NOISE` as `f64` literal source arrays transcribed from `sbr-reference-verbatim.md` §1/§2 (verify the anchors in a `#[test]` inside `xtask` too, so a bad paste fails host-side). Compute the modulation tables with `f64` `PI`, `cos`, `sin`; `LOG2_INT` with `f64::log2`; `POW2_FRAC` with `powf`.
  - `xtask/src/main.rs`: add `mod sbr_tables;`, arms `"gen-sbr-tables" => sbr_tables::run()`, `"check-sbr-tables" => sbr_tables::check()`, add `sbr_tables::check()?;` to `verify()` next to `filterbank_tables::check()?;`, and extend the `USAGE` string.
  - `sbr/mod.rs`: `pub(crate) mod tables;` (more submodules added by later tasks).
  - `formats/aac/mod.rs`: add `pub(crate) mod sbr;` alongside the existing `mod` lines.
  - Run `cargo xtask gen-sbr-tables` to write `sbr/tables.rs`.
- [ ] **Step 4:** Run the self-check tests → PASS. Run `cargo xtask check-sbr-tables` → OK (checked-in matches fresh generation).
- [ ] **Step 5:** thumbv7em build + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test -p architecture-tests` clean.
- [ ] **Step 6: Commit** `feat(xtask): SBR const tables (QMF prototype/modulation, noise, log2/pow2) + check gate`.

---

## Task 2: `sbr/bitstream.rs` — extension dispatch, framing, CRC-skip, byte-span clamp, extension loop

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/bitstream.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod bitstream;` + a skeleton `SbrState`).

Read `.superpowers/sdd/sbr-reference.md` §1. This task builds the *frame envelope* around the (not-yet-written) header/data parsers: it establishes the byte-span clamp and the `bs_extension_id` loop so every later parser slots in with no-panic bookkeeping. The actual `sbr_header`/`sbr_data` calls are stubbed here (return "not active") and filled by Tasks 3–11.

**Interfaces — Produces:**
```rust
pub(super) struct SbrPayload<'a> {
    pub crc: bool,           // EXT_SBR_DATA_CRC vs EXT_SBR_DATA
    pub bits: BitReader<'a>, // positioned at the SBR payload start
    pub end_bit: usize,      // exclusive clamp = payload_start + count*8
}
/// Parse the fill-element SBR envelope: consume the optional CRC field and the
/// `bs_header_flag`, dispatch the header (Task 3) + `sbr_data` (Tasks 5–11),
/// then walk the trailing `bs_extension` loop. Returns `Ok(true)` if SBR was
/// applied this frame, `Ok(false)` for upsample-only fallback (overrun / no
/// header yet). Never reads past `payload.end_bit`.
pub(super) fn sbr_extension_data(
    payload: &mut SbrPayload,
    id_aac: IdAac,          // Sce or Cpe — from the element preceding this fill
    state: &mut SbrState,
    core_pcm: &[[f32; 1024]; 2],
    n_ch: usize,
    out: &mut SbrOutput,    // 2048/ch scratch, filled on success
) -> Result<bool, DecodeError>;

pub(super) enum IdAac { Sce, Cpe }
```
**Consumes:** `crate::formats::aac::bits::BitReader` (existing; note its `bit_pos`/`bits_left`). The `count` and payload framing come from `decode_fil` (wired in Task 11); for this task's tests, construct `SbrPayload` directly.

The `bs_extension` trailing loop (§1 / §2.4): after `sbr_data`, while `end_bit − bit_pos ≥ 8`, read `bs_extension_id = read(2)` then a length and **length-skip** the sub-extension in Phase 2 (Phase 3 will dispatch `bs_extension_id == EXTENSION_ID_PS (2)` to `ps::parse_frame` here). Then byte-align to `end_bit`. Define `const EXTENSION_ID_PS: u32 = 2;` with a comment that Phase 2 skips it. **Clamp: if any read would cross `end_bit`, stop and return `Ok(false)` (discard SBR, upsample-only)** — faad2 `if (8*cnt < num_sbr_bits) return 1;`, FFmpeg `sbr_turnoff()`.

- [ ] **Step 1: Failing tests** — framing + clamp, with the header/data path stubbed:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    #[test]
    fn crc_field_consumed_when_flagged() {
        // 10-bit CRC then bs_header_flag=0 → no header yet → upsample-only.
        let buf = [0u8; 4];
        let mut p = SbrPayload { crc: true, bits: BitReader::new(&buf), end_bit: 32 };
        let mut st = SbrState::default();
        let core = [[0f32; 1024]; 2];
        let mut out = SbrOutput::default();
        assert_eq!(sbr_extension_data(&mut p, IdAac::Sce, &mut st, &core, 1, &mut out).unwrap(), false);
        assert!(p.bits.bit_pos() >= 11); // 10 CRC + 1 header_flag consumed
    }
    #[test]
    fn overrun_falls_back_to_upsample() {
        // Declare a tiny span but a header_flag=1 that would demand more bits.
        let buf = [0b1000_0000u8]; // header_flag=1 at first bit
        let mut p = SbrPayload { crc: false, bits: BitReader::new(&buf), end_bit: 3 };
        let mut st = SbrState::default();
        let core = [[0f32; 1024]; 2];
        let mut out = SbrOutput::default();
        // Must not panic / must not read past end_bit.
        let r = sbr_extension_data(&mut p, IdAac::Sce, &mut st, &core, 1, &mut out).unwrap();
        assert_eq!(r, false);
        assert!(p.bits.bit_pos() <= 3);
    }
}
```
- [ ] **Step 2:** Run `cargo test -p soul-audio-core --lib formats::aac::sbr::bitstream` → FAIL.
- [ ] **Step 3: Implement** per §1. Add a `clamped_read(bits, end_bit, n) -> Option<u32>` helper returning `None` on overrun; every read in the SBR path goes through it (or checks `bit_pos + n <= end_bit`). CRC field = `read(10)` when `crc`. `bs_header_flag = read(1)`; if 1, call `header::sbr_header(...)` (stub in this task: `todo`→ actually a real call once Task 3 lands; for Task 2, gate behind `state.header.is_some()` and treat header_flag=1 as "would-parse" by returning `Ok(false)` via a `#[allow(unused)]` stub `fn parse_header_stub() -> Result<(),()> { Err(()) }`). Then `sbr_data` (stub returns immediately in Task 2). Walk the `bs_extension` loop with the clamp. On any `None` from `clamped_read`, return `Ok(false)`. `SbrState::default()` = not active (`kx=32, M=0`). `SbrOutput` = `{ pcm: [[f32; 2048]; 2], written: usize }`.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): fill-element dispatch + byte-span clamp + bs_extension loop`.

> Interface note: Tasks 3–11 progressively replace the stubs inside `sbr_extension_data` with the real `sbr_header`/`sbr_data` drive. `SbrState` and `SbrOutput` grow field-by-field per task; each keeps `Default`.

---

## Task 3: `sbr/header.rs` — `sbr_header()` + reset/header-change comparison

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/header.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod header;`), `sbr/bitstream.rs` (call the real header parser).

Read `.superpowers/sdd/sbr-reference.md` §2. Fields in order (all fixed-width, MSB-first), with the exact default rules when the `extra` gates are 0.

**Interfaces — Produces:**
```rust
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SbrHeader {
    pub amp_res: u8,        // 1 bit
    pub start_freq: u8,     // 4 bits, 0..15
    pub stop_freq: u8,      // 4 bits, 0..15
    pub xover_band: u8,     // 3 bits, 0..7
    pub freq_scale: u8,     // default 2
    pub alter_scale: u8,    // default 1
    pub noise_bands: u8,    // default 2
    pub limiter_bands: u8,  // default 2
    pub limiter_gains: u8,  // default 2
    pub interpol_freq: u8,  // default 1
    pub smoothing_mode: u8, // default 1
}
#[derive(Clone, Copy, Default)]
pub(super) struct HeaderReset { pub reset: bool, pub limiter_only: bool }

/// Parse `sbr_header()` (§2). `prev` is the last accepted header (for the reset
/// diff). Reads exactly the fixed field layout; `bs_reserved` (2 bits) skipped.
pub(super) fn sbr_header(
    bits: &mut BitReader,
    end_bit: usize,
    prev: Option<&SbrHeader>,
) -> Result<(SbrHeader, HeaderReset), DecodeError>;
```
**Consumes:** `bits::BitReader`, the `end_bit` clamp from Task 2.

`reset = true` if any of `start_freq`, `stop_freq`, `freq_scale`, `alter_scale`, `xover_band`, `noise_bands` differ from `prev` (or `prev` is `None`). `limiter_only = true` if only `limiter_bands` changed. Reading past `end_bit` → `Err(DecodeError::Malformed)` (caught upstream → upsample-only).

- [ ] **Step 1: Failing tests:**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    #[test]
    fn parses_minimal_header_with_defaults() {
        // amp_res=1, start=5(0101), stop=6(0110), xover=3(011), reserved=00,
        // extra_1=0, extra_2=0 → defaults applied. Bit stream MSB-first:
        // 1 0101 0110 011 00 0 0  = 1010 1011 0011 0000 (pad) = 0xAB,0x30
        let buf = [0b1010_1011, 0b0011_0000];
        let mut b = BitReader::new(&buf);
        let (h, rst) = sbr_header(&mut b, 16, None).unwrap();
        assert_eq!(h.amp_res, 1);
        assert_eq!(h.start_freq, 5);
        assert_eq!(h.stop_freq, 6);
        assert_eq!(h.xover_band, 3);
        assert_eq!(h.freq_scale, 2);   // default
        assert_eq!(h.alter_scale, 1);  // default
        assert_eq!(h.noise_bands, 2);  // default
        assert_eq!(h.limiter_bands, 2);
        assert_eq!(h.interpol_freq, 1);
        assert_eq!(h.smoothing_mode, 1);
        assert!(rst.reset); // prev None → reset
    }
    #[test]
    fn limiter_only_change_flagged() {
        let buf = [0b1010_1011, 0b0011_0000];
        let mut b = BitReader::new(&buf);
        let (h0, _) = sbr_header(&mut b, 16, None).unwrap();
        let mut h1 = h0; h1.limiter_bands = 3;
        // Re-parse identical bytes but diff against h1: only limiter differs.
        let mut b2 = BitReader::new(&buf);
        let (_h, rst) = sbr_header(&mut b2, 16, Some(&h1)).unwrap();
        assert!(!rst.reset);
        assert!(rst.limiter_only);
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** the field reads in §2 order: `amp_res=read(1)`, `start_freq=read(4)`, `stop_freq=read(4)`, `xover_band=read(3)`, `read(2)` (reserved, discard), `extra_1=read(1)`, `extra_2=read(1)`; if `extra_1`: `freq_scale=read(2)`, `alter_scale=read(1)`, `noise_bands=read(2)` else defaults `2/1/2`; if `extra_2`: `limiter_bands=read(2)`, `limiter_gains=read(2)`, `interpol_freq=read(1)`, `smoothing_mode=read(1)` else defaults `2/2/1/1`. Compute the reset diff. Wire `bitstream.rs` to call this when `bs_header_flag==1` and store into `state.header` (promoting `state.active = true`).
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): sbr_header parse + reset/header-change diff`.

---

## Task 4: `sbr/freq_tables.rs` — frequency band tables (**VERIFY-FIRST**)

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/freq_tables.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod freq_tables;`).

Read `.superpowers/sdd/sbr-reference.md` §3 (all of §3.1–§3.5). **This is a verify-first unit** (like the LC IMDCT): validate against §3.5's two worked examples *before* anything downstream trusts it — a table bug here silently corrupts every later stage. The limiter table has a two-pass relationship with `hfgen.rs`'s patch borders (§8.3), so this task builds everything **except** the limiter table; a separate `derive_limiter` is called after patch construction (Task 9/11).

**Interfaces — Produces:**
```rust
#[derive(Clone, Copy, Default)]
pub(super) struct FreqTables {
    pub k0: u8, pub k2: u8, pub kx: u8, pub m: u8,
    pub n_master: u8, pub n_high: u8, pub n_low: u8, pub n_q: u8,
    pub f_master: [u8; 64],
    pub f_high: [u8; 64],           // f_table_res[HI_RES], len n_high+1
    pub f_low: [u8; 64],            // f_table_res[LO_RES], len n_low+1
    pub f_noise: [u8; 64],          // len n_q+1
    pub table_map_k_to_g: [u8; 64], // per high band m in 0..M
    pub f_lim: [u8; 64],            // limiter borders (filled by derive_limiter)
    pub n_l: u8,                    // limiter band count
}
impl FreqTables {
    /// Bands for envelope resolution `res` (0 = LO → n_low, 1 = HI → n_high).
    pub fn n(&self, res: u8) -> usize { if res == 0 { self.n_low as usize } else { self.n_high as usize } }
}

/// Build master + derived + noise tables (§3.1–§3.4) from the header at the
/// **output** sample rate `output_rate` (= 2×core). Validates every bound
/// (§3.5 "No-panic"); on any failure returns `Err` (→ upsample-only).
///
/// Uses the **FFmpeg live path** for `k0`/`k2` (compute `start_min` from
/// `A(fs)` and `fs`, and the stop power series via `make_bands`), NOT faad2's
/// doubled-rate `startMinTable` — the §3.5 worked numbers (k0=14/13) are the
/// FFmpeg-live values, and the conformance fixtures (Task 12) are ffmpeg-decoded.
pub(super) fn derive(hdr: &SbrHeader, output_rate: u32) -> Result<FreqTables, DecodeError>;

/// Fill `f_lim`/`n_l` (§3.4) by merging LO borders with the HF-gen patch borders
/// (§8.3), sorting, and deleting borders closer than the octave threshold.
pub(super) fn derive_limiter(ft: &mut FreqTables, patch_borders: &[u8]) -> Result<(), DecodeError>;
```
**Consumes:** `header::SbrHeader`; `tables::{LOG2_INT, POW2_FRAC}` for the §3.3 `log2`/power math; the `output_rate` = `2 * core_rate` (validated via the existing `crate::formats::aac::tables::resolve_sfi` for the stop-table row selection). `derive_limiter` consumes `hfgen::Patches::borders()` (Task 9).

Big lookup rows in §3.1/§3.2 (`startMinTable`, `offsetIndexTable`, `offset[7][16]`, `stopMinTable`, and the stop power series) are small — transcribe the numeric rows directly from §3.1/§3.2 (they are given inline in the reference, with a self-check against §3.5). Use `LOG2_INT`/`POW2_FRAC` for Path B's warped-log spacing and the noise-band `round(bs_noise_bands·log2(k2/kx))`.

- [ ] **Step 1: Failing tests** — the two §3.5 worked examples exactly:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::sbr::header::SbrHeader;
    fn hdr(start: u8, stop: u8, fscale: u8, alter: u8, xover: u8, nbands: u8) -> SbrHeader {
        SbrHeader { amp_res: 1, start_freq: start, stop_freq: stop, xover_band: xover,
            freq_scale: fscale, alter_scale: alter, noise_bands: nbands,
            limiter_bands: 2, limiter_gains: 2, interpol_freq: 1, smoothing_mode: 1 }
    }
    #[test]
    fn worked_example_linear_44100() {
        // §3.5 first example: output fs=44100, start=5, stop=6,
        // freq_scale=0, alter=0, xover=3, noise=2.
        let ft = derive(&hdr(5, 6, 0, 0, 3, 2), 44100).unwrap();
        assert_eq!(ft.k0, 14);
        assert_eq!(ft.k2, 37);
        assert_eq!(ft.n_master, 22);
        assert_eq!(&ft.f_master[..23], &[14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,37]);
        assert_eq!(ft.kx, 17);
        assert_eq!(ft.m, 20);
        assert_eq!(ft.n_high, 19);
        assert_eq!(ft.n_low, 10);
        assert_eq!(&ft.f_high[..20], &[17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,37]);
        assert_eq!(&ft.f_low[..11], &[17,18,20,22,24,26,28,30,32,34,37]);
        assert_eq!(ft.n_q, 2);
        assert_eq!(&ft.f_noise[..3], &[17,26,37]);
    }
    #[test]
    fn worked_example_warped_48000() {
        // §3.5 second example: output fs=48000, start=5, stop=3,
        // freq_scale=2, alter=1, xover=4, noise=2.
        let ft = derive(&hdr(5, 3, 2, 1, 4, 2), 48000).unwrap();
        assert_eq!(ft.k0, 13);
        assert_eq!(ft.k2, 27);
        assert_eq!(ft.n_master, 10);
        assert_eq!(&ft.f_master[..11], &[13,14,15,16,17,18,19,21,23,25,27]);
        assert_eq!(ft.kx, 17);
        assert_eq!(ft.m, 10);
        assert_eq!(ft.n_high, 6);
        assert_eq!(ft.n_low, 3);
        assert_eq!(&ft.f_high[..7], &[17,18,19,21,23,25,27]);
        assert_eq!(&ft.f_low[..4], &[17,19,23,27]);
        assert_eq!(ft.n_q, 1);
        assert_eq!(&ft.f_noise[..2], &[17,27]);
    }
    #[test]
    fn rejects_xover_ge_n_master() {
        // xover_band must be < N_master.
        let mut h = hdr(5, 6, 0, 0, 7, 2); h.xover_band = 30;
        assert!(derive(&h, 44100).is_err());
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** §3.1 (k0 via the FFmpeg live `start_min` from `A(fs)`/`fs` + `offset` row — matching k0=14/13), §3.2 (k2 via the live stop power series + span validation), §3.3 (master, both linear Path A and warped-log Path B via `LOG2_INT`/`POW2_FRAC`), §3.4 (derived HI/LO tables, `kx`, `M`, `N_high`/`N_low`, `f_noise` + `table_map_k_to_g`). Fixed ≤64-element sort for the warped path. Validate every bound: `k2>k0`, span limits (§3.2), `xover_band < n_master`, `M≤49`, `kx≤32`, `kx+M≤64`, `n_q≤5`, all indices `<64` → else `Err`. `derive_limiter` per §3.4 (merge/sort/prune with the `limiterBandsCompare` octave thresholds).
- [ ] **Step 4:** Run → PASS (both worked examples exact). This is the verify-first gate — do not proceed until both match.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): frequency band tables (master/derived/noise) + verify-first worked examples`.

> **Resolved ambiguity (applies from Task 5 on):** the reference's §4 ("t_E/t_Q stored ×RATE") and §9.2 ("Σ … t_env[e]·RATE …") are internally inconsistent. This plan stores `t_env`/`t_noise` in **raw slot units** (0..`numTimeSlots`=16) and multiplies by `RATE` (2) at the QMF-slot use sites in §8/§9. All grid tests below use raw-slot borders.

---

## Task 5: `sbr/grid.rs` — `sbr_grid()`, four frame classes, time borders

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/grid.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod grid;`).

Read `.superpowers/sdd/sbr-reference.md` §4. Per channel. `numTimeSlots = 16`. The four `bs_frame_class` variants (FIXFIX/FIXVAR/VARFIX/VARVAR) each set the envelope/noise time borders differently; derive `L_E`, `L_Q`, `l_A`, `bs_freq_res[]`.

**Interfaces — Produces:**
```rust
pub(super) const FIXFIX: u8 = 0;
pub(super) const FIXVAR: u8 = 1;
pub(super) const VARFIX: u8 = 2;
pub(super) const VARVAR: u8 = 3;

#[derive(Clone, Copy, Default)]
pub(super) struct SbrGrid {
    pub frame_class: u8,     // 0..3
    pub num_env: u8,         // L_E, 1..5
    pub num_noise: u8,       // L_Q, 1..2
    pub t_env: [u8; 6],      // raw-slot borders, valid indices 0..=L_E
    pub t_noise: [u8; 3],    // raw-slot borders, valid indices 0..=L_Q
    pub freq_res: [u8; 6],   // per envelope, LO_RES=0 / HI_RES=1
    pub pointer: u8,         // bs_pointer
    pub l_a: i8,             // "short envelope" index, -1 when none
}

/// Parse `sbr_grid()` (§4). Reads `bs_frame_class` then the class-specific
/// border layout; validates monotonicity + ranges (§4 "No-panic ranges").
pub(super) fn sbr_grid(bits: &mut BitReader, end_bit: usize) -> Result<SbrGrid, DecodeError>;
```
**Consumes:** `bits::BitReader`, the `end_bit` clamp. `ceil_log2 = [0,1,2,2,3,3]` for `bs_pointer` widths.

- [ ] **Step 1: Failing test** — a FIXFIX single-envelope frame (raw-slot borders):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    #[test]
    fn fixfix_single_env() {
        // frame_class=00, bs_num_env pow=00 → L_E=1, bs_freq_res=1.
        // bits MSB-first: 00 00 1 → 0b00001000 = 0x08.
        let buf = [0x08];
        let g = sbr_grid(&mut BitReader::new(&buf), 8).unwrap();
        assert_eq!(g.frame_class, FIXFIX);
        assert_eq!(g.num_env, 1);
        assert_eq!(g.freq_res[0], 1);
        assert_eq!(g.num_noise, 1);              // L_E==1 → L_Q=1
        assert_eq!(&g.t_env[..2], &[0, 16]);      // raw slots 0..numTimeSlots
        assert_eq!(&g.t_noise[..2], &[0, 16]);
        assert_eq!(g.l_a, -1);                    // FIXFIX
    }
    #[test]
    fn fixfix_four_env_equal_spacing() {
        // frame_class=00, pow=10 → L_E=4; freq_res=0. bits: 00 10 0 = 0b00100000 = 0x20.
        let buf = [0x20];
        let g = sbr_grid(&mut BitReader::new(&buf), 8).unwrap();
        assert_eq!(g.num_env, 4);
        assert_eq!(&g.t_env[..5], &[0, 4, 8, 12, 16]); // equally spaced
        assert_eq!(g.num_noise, 2);                    // L_E>1 → L_Q=2
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** all four classes per §4: FIXFIX (`bs_num_env = 1<<read(2)`, clamp ≤5, single `bs_freq_res`, equally-spaced interior borders); FIXVAR/VARFIX (`bs_abs_bord`, `bs_num_env`, `bs_rel_bord[]∈{2,4,6,8}`, `bs_pointer` width `ceil_log2[num_env]`, reversed/forward `bs_freq_res`); VARVAR (`bs_abs_bord_0/1`, `bs_num_rel_0/1`, borders up/down). Derive `L_E=min(num_env, class==VARVAR?5:4)` (≥1 else `Err`), `L_Q=(L_E>1)?2:1`, the `t_env`/`t_noise` vectors (strict monotone within `(t_env[0], t_env[L_E])` → else `Err`), the middle noise border (`middleBorder` rule), and `l_A` (per-class rule). Validate `bs_pointer ≤ num_env+1`.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): sbr_grid time-frequency grid (4 frame classes)`.

---

## Task 6: `sbr/dtdf_invf.rs` — delta-coding flags + inverse-filter modes

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/dtdf_invf.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod dtdf_invf;`).

Read `.superpowers/sdd/sbr-reference.md` §5. Two small reads per channel, plus the CPE coupling copy (`invf_mode` ch0→ch1).

**Interfaces — Produces:**
```rust
/// `sbr_dtdf()` (§5): delta direction flags. `df_env[i]`/`df_noise[i]`:
/// 0 = delta-freq, 1 = delta-time. Read per channel.
pub(super) fn read_dtdf(
    bits: &mut BitReader, end_bit: usize, l_e: u8, l_q: u8,
) -> Result<([u8; 5], [u8; 2]), DecodeError>;   // (df_env, df_noise)

/// `invf_mode()` (§5): per-noise-band inverse-filter mode 0..3 (OFF/LOW/MID/HIGH).
/// Read for ch0; the CPE coupled path copies the result to ch1 (caller does the
/// copy — this returns ch0's array).
pub(super) fn read_invf(
    bits: &mut BitReader, end_bit: usize, n_q: u8,
) -> Result<[u8; 5], DecodeError>;
```
**Consumes:** `bits::BitReader`; `l_e`/`l_q` from `grid::SbrGrid`; `n_q` from `freq_tables::FreqTables`.

- [ ] **Step 1: Failing test:**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    #[test]
    fn reads_dtdf_flags() {
        // L_E=2, L_Q=1 → df_env[0]=1, df_env[1]=0, df_noise[0]=1.
        // bits: 1 0 1 = 0b10100000 = 0xA0.
        let buf = [0xA0];
        let (df_env, df_noise) = read_dtdf(&mut BitReader::new(&buf), 8, 2, 1).unwrap();
        assert_eq!(&df_env[..2], &[1, 0]);
        assert_eq!(df_noise[0], 1);
    }
    #[test]
    fn reads_invf_modes() {
        // N_Q=2 → invf[0]=read(2)=10=2(MID), invf[1]=read(2)=01=1(LOW).
        // bits: 10 01 = 0b10010000 = 0x90.
        let buf = [0x90];
        let invf = read_invf(&mut BitReader::new(&buf), 8, 2).unwrap();
        assert_eq!(&invf[..2], &[2, 1]);
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** the two loops (§5): `df_env[i]=read(1)` for `i in 0..l_e`, `df_noise[i]=read(1)` for `i in 0..l_q`; `invf_mode[n]=read(2)` for `n in 0..n_q`. Loops bounded by `l_e≤5`, `l_q≤2`, `n_q≤5`; any read past `end_bit` → `Err`. Document that the coupled ch1 copies `invf_mode` (and the grid/`t_E`/`t_Q`/`bs_pointer`/`bs_freq_res`) from ch0 — that copy lives in the Task 11 orchestration, not here.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): sbr_dtdf + invf_mode`.

---

## Task 7: `sbr/huffman.rs` + `sbr/envelope.rs` — SBR Huffman, envelope/noise decode + dequant

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/huffman.rs`, `crates/soul-audio-core/src/formats/aac/sbr/envelope.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod huffman; pub(super) mod envelope;`).

Read `.superpowers/sdd/sbr-reference.md` §6 (all of §6.1–§6.4). The 10 SBR Huffman trees are **verbatim in `sbr-reference-verbatim.md` §3** — fetch/transcribe them (each is `[[i8;2]; N]` with the sizes given: `t/f_huffman_env_1_5dB[120]`, `_bal_1_5dB[48]`, `_3_0dB[62]`, `_bal_3_0dB[24]`, `t_huffman_noise_3_0dB[62]`, `t_huffman_noise_bal_3_0dB[24]`). Verify the count anchors.

**Interfaces — Produces (`huffman.rs`):**
```rust
pub(super) type SbrHuff = &'static [[i16; 2]];
pub(super) static T_HUFF_ENV_1_5DB: [[i16; 2]; 120]; // (+ the other 9 trees, transcribed)
// ...
/// Walk a binary tree (faad2 `sbr_huff_dec`): start at node 0, follow bits while
/// the node value is ≥ 0; the leaf value + 64 is the signed delta. Reading past
/// `end_bit` or an out-of-range node index → `Err(Malformed)` (no infinite loop).
pub(super) fn sbr_huff_dec(bits: &mut BitReader, end_bit: usize, t: SbrHuff) -> Result<i32, DecodeError>;
```
**Interfaces — Produces (`envelope.rs`):**
```rust
#[derive(Clone, Copy, Default)]
pub(super) struct EnvQuant {
    pub env_q: [[u8; 48]; 6],   // env_facs_q[env][band], 0..=127
    pub noise_q: [[u8; 5]; 6],  // noise_facs_q[noise_env][band], 0..=30
    pub amp_res: u8,            // effective amp_res for this channel/frame
}
#[derive(Clone, Copy, Default)]
pub(super) struct EnvLinear {
    pub env: [[f32; 48]; 6],
    pub noise: [[f32; 5]; 6],
}
/// Previous-frame delta-time base, per channel (state carried across frames).
#[derive(Clone, Copy, Default)]
pub(super) struct EnvPrev { pub e_prev: [u8; 64], pub q_prev: [u8; 64], pub freq_res_prev: u8 }

/// Read + delta-resolve envelope & noise quantized levels (§6.1–§6.3). `coupling`
/// + `ch` select the Huffman table set + the `delta` shift; updates `prev`.
pub(super) fn read_env_noise(
    bits: &mut BitReader, end_bit: usize,
    grid: &SbrGrid, df_env: &[u8; 5], df_noise: &[u8; 2],
    ft: &FreqTables, hdr_amp_res: u8, coupling: bool, ch: usize,
    prev: &mut EnvPrev,
) -> Result<EnvQuant, DecodeError>;

/// Dequant to linear energy (§6.4). SCE / non-coupled CPE.
pub(super) fn dequant_sce(q: &EnvQuant) -> EnvLinear;
/// Coupled CPE: un-map ch0(left) + ch1(pan) quantized values to L/R energies.
pub(super) fn dequant_coupled(q0: &EnvQuant, q1: &EnvQuant) -> (EnvLinear, EnvLinear);
```
**Consumes:** `bits::BitReader`; `grid::SbrGrid`, `dtdf_invf`'s `df_env`/`df_noise`; `freq_tables::FreqTables` (`n(res)`, `n_q`); `header`'s `amp_res`.

Huffman selection (§6.1): `amp_res = (L_E==1 && frame_class==FIXFIX) ? 0 : hdr_amp_res`; `delta = (coupling && ch==1) ? 1 : 0`. Pick the env table by (delta-time vs -freq, amp_res, coupled-ch1) per §6.1's grid; noise delta-time uses `t_huffman_noise_*`, noise delta-freq **reuses** `f_huffman_env_3_0dB`/`_bal_3_0dB`. Read procedure §6.2/§6.3, delta resolution with the HI↔LO resolution remap (§6.2), validate `env_facs_q ≤ 127` / `noise_facs_q ≤ 30` each step. Dequant §6.4 uses `2^x` (integer x → bit-pack, no libm) and the `√2` literal.

- [ ] **Step 1: Failing tests:**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn huff_tree_walk_and_offset() {
        // Synthetic 3-node tree: node0 -> bit0 leaf(-64)→value 0, bit1 leaf(-63)→value 1.
        static T: [[i16; 2]; 1] = [[-64, -63]];
        use crate::formats::aac::bits::BitReader;
        let buf = [0b0100_0000]; // bit0=0 then bit0=1
        let mut b = BitReader::new(&buf);
        assert_eq!(sbr_huff_dec(&mut b, 8, &T).unwrap(), 0);   // -64 + 64
        assert_eq!(sbr_huff_dec(&mut b, 8, &T).unwrap(), 1);   // -63 + 64
    }
    #[test]
    fn env_huffman_table_sizes() {
        // Transcription anchors from -verbatim.md §3.
        assert_eq!(T_HUFF_ENV_1_5DB.len(), 120);
        assert_eq!(T_HUFF_ENV_3_0DB.len(), 62);
        assert_eq!(T_HUFF_ENV_BAL_3_0DB.len(), 24);
    }
    #[test]
    fn dequant_sce_amp_res_1() {
        // amp_res=1 → E = 2^(q+6). q=1 → 128.
        let mut q = EnvQuant::default();
        q.amp_res = 1; q.env_q[0][0] = 1;
        let lin = dequant_sce(&q);
        assert!((lin.env[0][0] - 128.0).abs() < 1e-3);
    }
    #[test]
    fn dequant_sce_noise() {
        // Q = 2^(6 - q). q=6 → 1.0.
        let mut q = EnvQuant::default();
        q.noise_q[0][0] = 6;
        let lin = dequant_sce(&q);
        assert!((lin.noise[0][0] - 1.0).abs() < 1e-4);
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `sbr_huff_dec` (bounded tree walk, `+64` offset), transcribe the 10 trees (verify count anchors host-side too), then `read_env_noise` (§6.1 table selection + §6.2/§6.3 read + delta resolution + `≤127`/`≤30` validation + `prev` update) and `dequant_sce`/`dequant_coupled` (§6.4 exact forms, `2^x` via exponent bit-pack, `panOffset` for coupling).
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): SBR Huffman + envelope/noise decode + dequant`.

---

## Task 8: `sbr/qmf.rs` — complex QMF analysis + synthesis (**VERIFY-FIRST, hardest**)

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/qmf.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod qmf;`).

Read `.superpowers/sdd/sbr-reference.md` §7 (§7.1 analysis, §7.2 synthesis; §7.3 downsampled is OUT of scope). This is the SBR analogue of the LC IMDCT — the **heaviest DSP unit** and a **verify-first** unit: the `(2n−95)`/`(2n−63)` modulation offsets are easy to get subtly wrong, so validate the analysis/synthesis against localization reference vectors *before* any downstream task trusts them. Uses `tables::{QMF_PROTO, QMF_ANA_COS, QMF_ANA_SIN, QMF_SYN_COS, QMF_SYN_SIN}` (Task 1, already self-checked vs closed form). This module **defines the `Cpx` complex element** reused by HF gen/adjust/assemble and (Phase 3) PS.

**Interfaces — Produces:**
```rust
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub(crate) struct Cpx { pub re: f32, pub im: f32 }

pub(super) const N_ANA: usize = 32;         // analysis subbands
pub(super) const N_SYN: usize = 64;         // synthesis subbands (assembled matrix width)
pub(super) const TIME_SLOTS_RATE: usize = 32; // numTimeSlotsRate at 2×
pub(super) const T_HFGEN: usize = 8;        // LPC lookback slots
pub(super) const T_HFADJ: usize = 2;        // adjusted-region slot offset
pub(super) const MAX_SLOTS: usize = 40;     // TIME_SLOTS_RATE + T_HFGEN

#[derive(Clone)]
pub(super) struct QmfAnalysisState { pub x: [f32; 320] }   // input delay line
impl Default for QmfAnalysisState { /* zeroed */ }
#[derive(Clone)]
pub(super) struct QmfSynthesisState { pub v: [f32; 1280] } // synthesis delay line
impl Default for QmfSynthesisState { /* zeroed */ }

/// Analysis QMF (§7.1): 32 time-slots, each consuming 32 core samples, producing
/// 32 complex subbands per slot. Writes `x_low[slot][band]`, slot 0..32.
pub(super) fn qmf_analysis(
    core_pcm: &[f32; 1024],
    st: &mut QmfAnalysisState,
    x_low: &mut [[Cpx; N_ANA]; TIME_SLOTS_RATE],
);

/// Synthesis QMF (§7.2, dual-rate 2×): from 32 assembled 64-band slots → 2048
/// real time samples. `x[slot][band]`.
pub(super) fn qmf_synthesis(
    x: &[[Cpx; N_SYN]; TIME_SLOTS_RATE],
    st: &mut QmfSynthesisState,
    out: &mut [f32; 2048],
);
```
**Consumes:** `tables::*` (the QMF const tables). Nothing else in `sbr/`.

Analysis per slot (§7.1): shift `x` up by 32, write 32 new (time-reversed) core samples; window with `QMF_PROTO[2m]` (320-tap even subset) + fold to 64 (`u[n]=Σ_{j<5} z[n+64j]`); modulate `X[k]=Σ_n u[n]·(QMF_ANA_COS[k][n] + i·QMF_ANA_SIN[k][n])`, `k=0..31`. Synthesis per slot (§7.2): modulate the 64 complex bands to 128 real `w[n]=Re{Σ_k X[k]·(QMF_SYN_COS[n][k]+i·QMF_SYN_SIN[n][k])}`; shift `v` by 128, write `w`; windowed overlap-add (`out[k]=Σ_{j<10} v[k+Voff[j]]·QMF_PROTO[k+64j]`, `Voff={0,192,256,448,512,704,768,960,1024,1216}`). The direct matrix form given here is the specification; a DCT/MDCT-fast equivalent is optional and must reproduce the same result.

- [ ] **Step 1: Failing tests** — reference-vector localization (tests may use `std` for a small DFT oracle):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn dominant_bin(sig: &[f32]) -> usize {
        // naive DFT magnitude argmax over sig.len() bins (std math ok in tests).
        let n = sig.len();
        let mut best = (0usize, -1.0f64);
        for k in 0..n/2 {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, &s) in sig.iter().enumerate() {
                let a = -2.0 * core::f64::consts::PI * (k * i) as f64 / n as f64;
                re += f64::from(s) * a.cos(); im += f64::from(s) * a.sin();
            }
            let m = re*re + im*im;
            if m > best.1 { best = (k, m); }
        }
        best.0
    }
    #[test]
    fn analysis_zero_in_zero_out() {
        let mut st = QmfAnalysisState::default();
        let mut xl = [[Cpx::default(); N_ANA]; TIME_SLOTS_RATE];
        qmf_analysis(&[0.0; 1024], &mut st, &mut xl);
        for slot in &xl { for c in slot { assert_eq!(*c, Cpx::default()); } }
    }
    #[test]
    fn analysis_localizes_tone_to_expected_band() {
        // A tone at the centre of analysis band kb → that band should dominate.
        let kb = 5usize;
        let mut core = [0.0f32; 1024];
        for (i, s) in core.iter_mut().enumerate() {
            // band centre (normalised): (kb+0.5)/(2*N_ANA) cycles/sample.
            let f = (kb as f64 + 0.5) / (2.0 * N_ANA as f64);
            *s = (2.0 * core::f64::consts::PI * f * i as f64).sin() as f32;
        }
        let mut st = QmfAnalysisState::default();
        let mut xl = [[Cpx::default(); N_ANA]; TIME_SLOTS_RATE];
        qmf_analysis(&core, &mut st, &mut xl);
        // Steady-state slot (after the delay line fills): magnitude peaks at kb.
        let slot = &xl[TIME_SLOTS_RATE - 1];
        let mag = |c: &Cpx| c.re*c.re + c.im*c.im;
        let peak = (0..N_ANA).max_by(|&a, &b| mag(&slot[a]).partial_cmp(&mag(&slot[b])).unwrap()).unwrap();
        assert_eq!(peak, kb);
    }
    #[test]
    fn synthesis_single_band_produces_tone() {
        // Drive synthesis band kb with unit magnitude across all slots → the
        // 2048-sample output is a sinusoid whose dominant bin matches kb.
        let kb = 10usize;
        let mut x = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        for slot in x.iter_mut() { slot[kb] = Cpx { re: 1.0, im: 0.0 }; }
        let mut st = QmfSynthesisState::default();
        let mut out = [0.0f32; 2048];
        qmf_synthesis(&x, &mut st, &mut out);
        // Expected output frequency = (kb+0.5)/128 cycles/sample → bin over 2048.
        let expect_bin = (((kb as f64 + 0.5) / 128.0) * 2048.0).round() as usize;
        let got = dominant_bin(&out[512..1536]); // steady-state window
        let scaled = ((got as f64) * 2048.0 / 1024.0).round() as usize;
        assert!((scaled as i64 - expect_bin as i64).abs() <= 2, "got {scaled}, expect {expect_bin}");
    }
}
```
- [ ] **Step 2:** Run `cargo test -p soul-audio-core --lib formats::aac::sbr::qmf` → FAIL.
- [ ] **Step 3: Implement** analysis + synthesis exactly per §7.1/§7.2 with the const tables. Keep the delay-line shifts and the `Voff`/prototype-stride offsets exactly as given. No `unsafe`, no `libm`.
- [ ] **Step 4:** Run → PASS. **Verify-first gate:** both localization tests must pass (they pin the exact modulation offsets) before Tasks 9–11 build on this. If a band is off-by-one or mirrored, re-check the `(2n−95)`/`(2n−63)` sign and the fold/stride.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): complex QMF analysis + synthesis (verify-first localization)`.

---

## Task 9: `sbr/hfgen.rs` — LPC inverse filter + chirp + patch + HF generation

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/hfgen.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod hfgen;`).

Read `.superpowers/sdd/sbr-reference.md` §8 (§8.1 LPC, §8.2 chirp, §8.3 patch, §8.4 generation). Runs on the 40-slot `X_low` buffer to fill `X_high[kx..kx+M−1]`.

**Interfaces — Produces:**
```rust
#[derive(Clone, Copy, Default)]
pub(super) struct Patches {
    pub n_patches: usize,       // ≤ 5
    pub num_subbands: [u8; 5],  // patchNoSubbands[p]
    pub start_subband: [u8; 5], // patchStartSubband[p]
}
impl Patches {
    /// Sorted patch borders (QMF channels, relative to kx) for the §3.4 limiter
    /// table; returns (borders, len).
    pub fn borders(&self, kx: u8) -> ([u8; 6], usize);
}
/// Patch construction (§8.3). `output_rate` = 2×core (for `goalSb`).
pub(super) fn patch_construction(ft: &FreqTables, output_rate: u32) -> Result<Patches, DecodeError>;

/// Chirp state carried across frames (per channel).
#[derive(Clone, Copy, Default)]
pub(super) struct ChirpState { pub bw_prev: [f32; 5], pub invf_prev: [u8; 5] }

/// 2nd-order complex LPC per low band (§8.1); fills `alpha0`/`alpha1` for k=0..k0.
pub(super) fn calc_prediction_coef(
    x_low: &[[Cpx; N_ANA]; MAX_SLOTS], k0: usize,
    alpha0: &mut [Cpx; N_ANA], alpha1: &mut [Cpx; N_ANA],
);

/// Chirp factors (§8.2) per noise band; updates `st`, writes `bw`.
pub(super) fn calc_chirp_factors(invf: &[u8; 5], n_q: usize, st: &mut ChirpState, bw: &mut [f32; 5]);

/// HF generation MAC (§8.4): fills `x_high[slot][band]`, band kx..kx+M−1, over
/// the current-frame slots, via the patch map + per-band predictor (α·bw).
pub(super) fn hf_generation(
    x_low: &[[Cpx; N_ANA]; MAX_SLOTS],
    alpha0: &[Cpx; N_ANA], alpha1: &[Cpx; N_ANA],
    bw: &[f32; 5], ft: &FreqTables, patches: &Patches, grid: &SbrGrid,
    x_high: &mut [[Cpx; N_SYN]; MAX_SLOTS],
);
```
**Consumes:** `qmf::{Cpx, N_ANA, N_SYN, MAX_SLOTS}`; `freq_tables::FreqTables` (`k0`/`kx`/`M`/`f_master`/`table_map_k_to_g`/`n_q`); `dtdf_invf`'s `invf_mode`; `grid::SbrGrid` (current-frame slot span). Produces the patch borders consumed by `freq_tables::derive_limiter` (Task 4).

- [ ] **Step 1: Failing tests** — chirp math is deterministic and hand-checkable:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chirp_off_is_zero() {
        // invf OFF (0), prev OFF → new_bw=0 → bw < 0.015625 → 0.
        let mut st = ChirpState::default();
        let mut bw = [0.0f32; 5];
        calc_chirp_factors(&[0, 0, 0, 0, 0], 2, &mut st, &mut bw);
        assert_eq!(bw[0], 0.0);
        assert_eq!(bw[1], 0.0);
    }
    #[test]
    fn chirp_high_from_zero_prev() {
        // invf HIGH (3) → new_bw=0.98; new_bw ≥ bw_prev(0) → bw = 0.90625*0.98 + 0 = 0.888125.
        let mut st = ChirpState::default();
        let mut bw = [0.0f32; 5];
        calc_chirp_factors(&[3, 0, 0, 0, 0], 1, &mut st, &mut bw);
        assert!((bw[0] - 0.888125).abs() < 1e-5);
        assert!((st.bw_prev[0] - 0.888125).abs() < 1e-5);
    }
    #[test]
    fn patches_are_bounded() {
        use crate::formats::aac::sbr::{freq_tables::derive, header::SbrHeader};
        let h = SbrHeader { amp_res: 1, start_freq: 5, stop_freq: 6, xover_band: 3,
            freq_scale: 0, alter_scale: 0, noise_bands: 2, limiter_bands: 2,
            limiter_gains: 2, interpol_freq: 1, smoothing_mode: 1 };
        let ft = derive(&h, 44100).unwrap();
        let p = patch_construction(&ft, 44100).unwrap();
        assert!(p.n_patches >= 1 && p.n_patches <= 5);
        let mut total = 0u32;
        for i in 0..p.n_patches {
            assert!(p.start_subband[i] as usize + p.num_subbands[i] as usize <= 64);
            total += u32::from(p.num_subbands[i]);
        }
        assert_eq!(total, u32::from(ft.m)); // patches cover exactly M high bands
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** §8.1 (complex covariance autocorrelation φ, `d`/`α1`/`α0`, `|α|²≥16` stability fallback), §8.2 (`mapNewBw` + the two smoothing blends + the `0.015625`/`0.99609375` caps), §8.3 (`goalSb`/`patch_construction` loop, drop-tiny-last-patch, `min(.,5)`), §8.4 (per-high-band predictor MAC `X_high[k][l] = X_low[p][l] + α0'·X_low[p][l−1] + α1'·X_low[p][l−2]`, guarded lookback via `T_HFGEN`/`T_HFADJ`). Bound `n_patches≤5`, `k<64`, `p<32`; reject `k≥kx+M`.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): HF generation (LPC inverse filter + chirp + patch)`.

---

## Task 10: `sbr/hfadj.rs` — mapping + env estimate + gain/limiter + noise/sine assembly

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/hfadj.rs`; MODIFY `sbr/mod.rs` (`pub(super) mod hfadj;`).

Read `.superpowers/sdd/sbr-reference.md` §9 (§9.1 mapping, §9.2 estimate, §9.3 gain/limiter, §9.4 assembly). Uses the FFmpeg linear-float form. The only transcendental is `√`, provided as a no-`libm` function here.

**Interfaces — Produces:**
```rust
/// No-libm sqrt: bit-hack seed + 2 Newton-Raphson iterations. `x<0`/NaN → 0.
pub(super) fn sqrtf(x: f32) -> f32;

/// §9.3 per-band base gain (pre limiter/boost) — pure, for unit testing.
pub(super) fn base_gain(e_orig: f32, e_curr: f32, q_mapped: f32, s_mapped: bool, delta: bool) -> f32;

/// Per-channel HF-adjustment state carried across frames.
#[derive(Clone, Copy, Default)]
pub(super) struct HfAdjState {
    pub g_temp: [[f32; 48]; 42],
    pub q_temp: [[f32; 48]; 42],
    pub index_noise: u16, // 0..511, carried
    pub index_sine: u8,   // 0..3, carried
    pub l_a_prev: i8,
}

/// Full HF adjustment (§9.1–§9.4): map envelope/noise onto bins, estimate the
/// current envelope, compute gains + limiter, and assemble Y (gain + smoothed
/// noise + sine) for the current frame. Fills `y[slot][band]`, band kx..kx+M−1.
pub(super) fn hf_adjustment(
    x_high: &[[Cpx; N_SYN]; MAX_SLOTS],
    env: &EnvLinear,
    grid: &SbrGrid, ft: &FreqTables, hdr: &SbrHeader,
    add_harmonic: &[u8; 48],   // bs_add_harmonic per HI-RES band (0 if absent)
    st: &mut HfAdjState,
    y: &mut [[Cpx; N_SYN]; TIME_SLOTS_RATE],
) -> Result<(), DecodeError>;
```
**Consumes:** `qmf::{Cpx, N_SYN, MAX_SLOTS, TIME_SLOTS_RATE}`; `hfgen`'s `x_high`; `envelope::EnvLinear`; `freq_tables::FreqTables` (limiter table, `f_high`/`f_low`/`f_noise`, `kx`/`M`); `grid::SbrGrid`; `header::SbrHeader` (`interpol_freq`, `smoothing_mode`, `limiter_gains`); `tables::V_NOISE`.

- [ ] **Step 1: Failing tests:**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sqrtf_known_values() {
        assert!((sqrtf(4.0) - 2.0).abs() < 1e-4);
        assert!((sqrtf(2.0) - 1.4142135).abs() < 1e-4);
        assert_eq!(sqrtf(0.0), 0.0);
        assert_eq!(sqrtf(-1.0), 0.0); // guarded
    }
    #[test]
    fn base_gain_no_noise_no_sine() {
        // q_mapped=0, s_mapped=false, delta=true, e_curr=e_orig=3
        // → sqrt(3 / ((1+3)(1+0))) = sqrt(0.75) ≈ 0.8660254.
        assert!((base_gain(3.0, 3.0, 0.0, false, true) - 0.8660254).abs() < 1e-4);
    }
    #[test]
    fn base_gain_with_sine() {
        // s_mapped=true: sqrt(e_orig*q_mapped / ((1+e_curr)(1+q_mapped)))
        // e_orig=4, e_curr=1, q_mapped=1 → sqrt(4*1/((2)(2))) = sqrt(1) = 1.
        assert!((base_gain(4.0, 1.0, 1.0, true, false) - 1.0).abs() < 1e-4);
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `sqrtf` (bit-hack seed from the IEEE-754 exponent + 2–3 Newton-Raphson steps; document the derivation), `base_gain` (§9.3 exact both branches, `+FLT_MIN`), then `hf_adjustment`: §9.1 `sbr_mapping` (`e_origmapped`/`q_mapped`/`s_indexmapped`/`s_mapped` spread over bins), §9.2 `sbr_env_estimate` (`e_curr`, both `bs_interpol_freq` branches, `env_size = 2·(t_env[e+1]−t_env[e])`), §9.3 `sbr_gain_calc` (`limgain` table, per-band `q_m`/`s_m`/`gain`, `gain_max` limiter, `gain_boost`), §9.4 `sbr_hf_assemble` (`h_smooth[5]` smoothing rings `g_temp`/`q_temp`, `Y = G·X_high + Q·V_NOISE[indexNoise] + s_m·φ[indexSine]`, `no_noise`/`l_A` rules, `indexNoise`/`indexSine` cross-frame carry). Bound `M≤49`, band `<64`, ring depth 5, `indexNoise&511`, `indexSine&3`.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): HF adjustment (mapping + gain/limiter + noise/sine)`.

---

## Task 11: `sbr/assemble.rs` + `AacDecoder`/`decode_fil` wiring

**Files:** Create `crates/soul-audio-core/src/formats/aac/sbr/assemble.rs`; MODIFY `sbr/mod.rs` (grow `SbrState`, wire the §10 drive), `sbr/bitstream.rs` (call the real `sbr_data` drive), `formats/aac/mod.rs` (thread `SbrState`, output 2× PCM, `stream_info` rate, `PCM_CAP`), `formats/aac/syntax.rs` (`decode_fil` dispatch), `demux/mp4.rs` (explicit-SBR ASC signalling — optional rate hint).

Read `.superpowers/sdd/sbr-reference.md` §10 + §2.4 (PS seam). This task closes the loop: splice low+high, run synthesis, and wire SBR into the shipped LC pipeline.

**Interfaces — Produces:**
```rust
/// Splice the delayed low band (X_low[0..kx], offset by T_HFADJ) with the
/// adjusted high band (Y[kx..kx+M]) into the assembled 64-band synthesis matrix
/// (§10 step 5). **This is the PS seam (§2.4):** Phase 3 intercepts the
/// assembled matrix here (mono → L/R) before synthesis; Phase 2 passes it
/// straight through. The 6-slot history for PS's 13-tap FIR is retained in
/// per-channel state (`SbrState`), so Phase 3 reconstructs the full X[38][64].
pub(super) fn assemble(
    x_low: &[[Cpx; N_ANA]; MAX_SLOTS],
    y: &[[Cpx; N_SYN]; TIME_SLOTS_RATE],
    ft: &FreqTables,
    x_out: &mut [[Cpx; N_SYN]; TIME_SLOTS_RATE],
);
```
And the per-channel drive in `sbr/mod.rs` (`sbr_data` → the §10 step list), producing 2048 samples/channel into `SbrOutput`. **Coupling (CPE `bs_coupling==1`):** ch0 carries grid/`t_E`/`t_Q`/`bs_pointer`/`bs_freq_res`/`invf` (copied to ch1); both channels read envelope+noise; `dequant_coupled` un-maps to L/R (§6.4); each channel still runs its own analysis→gen→adj→assemble→synthesis chain.

**Consumes:** every prior `sbr::*` unit; `qmf::qmf_synthesis`; `formats/aac/mod.rs`'s LC core PCM (`[[f32; 1024]; 2]`).

**Wiring changes:**
- `syntax.rs::decode_fil` — signature grows to inspect `extension_type = read(4)`; for `EXT_SBR_DATA (13)`/`EXT_SBR_DATA_CRC (14)`, build an `SbrPayload` over the remaining `count·8−4` bits and hand it (plus `id_aac` from the preceding element) to `sbr::sbr_extension_data`; every other type is skipped exactly as today. `decode_raw_data_block` threads the preceding element's `id_aac` and (via a new out-param or a field on the returned frame) the SBR-decoded PCM / "SBR applied" flag.
- `formats/aac/mod.rs` — add `sbr: sbr::SbrState` to `AacDecoder` (always present, starts inactive = `kx=32, M=0` upsample-only). `decode_rdb` gains a `&mut SbrState` + the core LC `outs: [[f32;1024];2]`; when SBR is applied this frame, it writes **2048 interleaved samples/channel** instead of 1024. Grow `PCM_CAP` to `2048 * 2 * 2 = 8192` (2× frame × 2 ch × existing headroom convention). `stream_info().sample_rate` returns `2×core` once SBR is confirmed (explicit ASC signal, or after the first `EXT_SBR_DATA` frame); `channels` unchanged.
- `demux/mp4.rs` (+ `AacConfig`) — extend the ASC parse to detect explicit SBR (`audioObjectType==5`, or the `0x2B7` sync extension with `sbrPresentFlag`), carrying an `sbr_signalled: bool` + optional `ext_sample_rate` so `stream_info` reports 2× up front for explicit streams. Implicit ADTS discovery still works via the fill element.

- [ ] **Step 1: Failing test** — assemble splices bands + an end-to-end smoke test (decode a few SBR frames without panic, output length 2× core):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assemble_splices_low_and_high() {
        // Low band (below kx) copied through; high band (kx..kx+M) taken from Y.
        let mut ft = crate::formats::aac::sbr::freq_tables::FreqTables::default();
        ft.kx = 4; ft.m = 3; // low bands 0..4 from X_low, high bands 4..7 from Y
        let mut x_low = [[Cpx::default(); N_ANA]; MAX_SLOTS];
        let mut y = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        for s in 0..TIME_SLOTS_RATE {
            x_low[s + T_HFADJ][2] = Cpx { re: 1.0, im: 0.0 };  // a low band
            y[s][5] = Cpx { re: 2.0, im: 0.0 };                 // a high band
        }
        let mut out = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        assemble(&x_low, &y, &ft, &mut out);
        assert_eq!(out[0][2], Cpx { re: 1.0, im: 0.0 });        // low passed through
        assert_eq!(out[0][5], Cpx { re: 2.0, im: 0.0 });        // high from Y
        assert_eq!(out[0][10], Cpx::default());                 // above kx+M zeroed
    }
}
```
(Plus a top-level smoke test in `formats/aac` tests: decode the first ~10 frames of `sbr.aac`, assert no panic and each frame yields 2048 samples/channel.)
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `assemble` (§10 step 5 low/high splice, `T_HFADJ` low-band delay, zero above `kx+M`), the `sbr/mod.rs` per-channel drive (§10 steps 1–6, coupling copy + `dequant_coupled`), and all wiring changes above. Keep the LC-only path byte-identical when no `EXT_SBR_DATA` is present (backward compatibility).
- [ ] **Step 4:** Run → PASS (assemble unit + smoke). Full `cargo test -p soul-audio-core`.
- [ ] **Step 5:** thumbv7em + `cargo clippy --workspace --all-targets -- -D warnings` + arch-tests clean.
- [ ] **Step 6: Commit** `feat(sbr): assemble + PS seam + AacDecoder/decode_fil wiring`.

---

## Task 12: Conformance golden + structural sanity + robustness fuzz + docs

**Files:** MODIFY `crates/soul-audio-core/tests/decode.rs` (SBR conformance + structural), `crates/soul-audio-core/tests/robustness.rs` (SBR fuzz seed), `crates/soul-audio-core/testdata/README.md` (already seeded in Task 0 — finalize the measured-SNR note).

Read `.superpowers/sdd/sbr-reference.md` §5 (design testing plan). Reuse the existing `decode_all`/`load_s16le`/`best_align_segsnr(ours, refr, channels)` helpers in `tests/decode.rs` (the AAC-LC golden path).

- [ ] **Step 1: Conformance golden test** — segmental SNR vs ffmpeg, priming-aligned, threshold calibrated from the measured value (SBR is approximate, so expect a **lower** bar than pure LC's 45 dB):
```rust
#[test]
fn sbr_m4a_matches_ffmpeg_within_tolerance() {
    let m4a = include_bytes!("../testdata/sbr.m4a");
    let refr = load_s16le(include_bytes!("../testdata/sbr.m4a.ref.pcm"));
    let mut dec = open(SliceSource::new(m4a)).unwrap();
    assert_eq!(dec.stream_info().channels, 2);
    let ours = decode_all(m4a);
    let (off, snr) = best_align_segsnr(&ours, &refr, 2);
    // Calibrated from the measured value (fill in after first green decode);
    // SBR reconstruction is lossy → margin below the measured number, e.g. if
    // measured ≈ 22 dB, gate at 12 dB. Document the measured number here.
    assert!(snr > SBR_SNR_BAR, "SBR m4a SNR vs ffmpeg too low: {snr:.1} dB (offset {off})");
}
// Repeat for sbr.aac (ADTS).
```
- [ ] **Step 2: Structural sanity test** (backstop, ffmpeg-independent):
```rust
#[test]
fn sbr_output_is_2x_and_non_silent() {
    let aac = include_bytes!("../testdata/sbr.aac");
    let ours = decode_all(aac);            // interleaved i32, 2 ch
    // 2× length: our per-channel sample count ≈ 2× the core frame count.
    // Compare against a plain-LC decode of the same core would give — here assert
    // the decoded per-channel length matches the ffmpeg 2× ref length within a
    // frame, and RMS > 0 (non-silent), and all finite / not all-clipped.
    let refr = load_s16le(include_bytes!("../testdata/sbr.aac.ref.pcm"));
    let ours_per_ch = ours.len() / 2;
    let ref_per_ch = refr.len() / 2;
    assert!((ours_per_ch as i64 - ref_per_ch as i64).abs() <= 4096, "length not ~2×");
    let rms: f64 = (ours.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / ours.len() as f64).sqrt();
    assert!(rms > 1.0, "output silent (rms {rms})");
    assert!(ours.iter().all(|&s| (-32768..=32767).contains(&s)), "clipped/garbage");
    assert!(!ours.iter().all(|&s| s.abs() >= 32767), "all-clipped");
}
```
- [ ] **Step 3:** Run → FAIL (bar const undefined / first decode). Iterate the integration until segmental SNR clears a sane bar. If early samples align but drift, suspect the QMF synthesis delay/`Voff` or the assemble slot bookkeeping; if garbage, suspect the freq-table `k0`/`kx` (must match ffmpeg's live path) or a modulation-offset sign. Measure the SNR, set `SBR_SNR_BAR` with margin, document the measured number.
- [ ] **Step 4: Robustness fuzz seed** — add `("sbr-m4a", include_bytes!("../testdata/sbr.m4a").to_vec())` and `("sbr-adts", include_bytes!("../testdata/sbr.aac").to_vec())` to `seeds()` in `tests/robustness.rs`, so the SBR bitstream fields (header/grid/envelope Huffman) join the standing no-panic sweep. Confirm `decoders_never_panic_or_hang_on_malformed_input` passes (every SBR field bounds-checked → `Err`/upsample-fallback, never panic/OOB — §3.3).
- [ ] **Step 5:** Finalize `testdata/README.md` with the measured SNR + chosen bar. Run `cargo test -p soul-audio-core`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, thumbv7em build, `cargo test -p architecture-tests`, and `cargo xtask verify` — all clean.
- [ ] **Step 6: Commit** `test(sbr): conformance golden + structural sanity + robustness fuzz seed`.

---

## Self-review notes (for the executor)

- **Spec §4 unit → task coverage:** §4 `sbr/bitstream.rs`→T2; `sbr/header.rs`→T3; `sbr/freq_tables.rs`→T4 (verify-first); `sbr/grid.rs`→T5; `sbr/dtdf_invf.rs`→T6; `sbr/envelope.rs`(+SBR Huffman)→T7; `sbr/qmf.rs`→T8 (verify-first); `sbr/hfgen.rs`→T9; `sbr/hfadj.rs`→T10; `sbr/assemble.rs` + `AacDecoder`/`decode_fil` wiring→T11; `sbr/mod.rs` orchestration→T2 skeleton grown through T11. Reference §0 (RAM sizing) is baked into every buffer dim; §1→T2, §2→T3, §3→T4, §4→T5, §5→T6, §6→T7, §7→T8, §8→T9, §9→T10, §10→T11. Const tables (§3.1)→T1. Testing (§5)→T0 fixtures + T12 golden/structural/fuzz. All 10 reference sections + the PS seam (§2.4, exposed in T11's `assemble`) are covered.
- **Type consistency (Produces in Task N = Consumes in Task M):** `SbrHeader` (T3) → consumed by `freq_tables::derive` (T4), `envelope` (T7), `hfadj` (T10). `FreqTables` (T4) → `dtdf_invf` (T6, `n_q`), `envelope` (T7), `hfgen`/`hfadj`/`assemble` (T9–T11); `derive_limiter` fed by `Patches::borders` (T9). `SbrGrid` (T5) → T6/T7/T9/T10. `Cpx`/`N_ANA`/`N_SYN`/`MAX_SLOTS`/`TIME_SLOTS_RATE`/`T_HFGEN`/`T_HFADJ` defined once in `qmf.rs` (T8) → T9/T10/T11. `EnvLinear` (T7) → `hfadj` (T10). `Patches` (T9) → `freq_tables::derive_limiter` (T4) + `hf_generation` (T9). `x_low: [[Cpx; N_ANA]; MAX_SLOTS]`, `x_high: [[Cpx; N_SYN]; MAX_SLOTS]`, `y: [[Cpx; N_SYN]; TIME_SLOTS_RATE]`, assembled `[[Cpx; N_SYN]; TIME_SLOTS_RATE]` shapes are consistent across T8–T11. The QMF const-table names (`QMF_PROTO`, `QMF_ANA_COS/SIN`, `QMF_SYN_COS/SIN`, `V_NOISE`, `LOG2_INT`, `POW2_FRAC`, T1) match their consumers (T4, T8, T10).
- **Placeholder scan:** every task has concrete Create/Modify paths, real Rust interface signatures, and runnable TDD steps with expected FAIL/PASS — no "add validation"/"handle edge cases"/"similar to Task N". Big tables (QMF prototype, noise `V[512]`, 10 Huffman trees) are explicitly "fetch from `sbr-reference-verbatim.md` §1/§2/§3 + verify size/anchor", not inlined.
- **Resolved ambiguities (flagged for the executor):** (1) `t_env`/`t_noise` stored in **raw-slot units**, ×RATE at use — the reference's §4/§9.2 conflict; resolved after Task 4. (2) `k0`/`k2` use the **FFmpeg live path** (start_min computed from `A(fs)`/output fs), NOT faad2's doubled-rate `startMinTable` — the §3.5 worked numbers (k0=14/13) are the live values, and conformance is vs ffmpeg. (3) The PS seam's `X[38][64]` = 32 current slots (synthesized) + 6-slot history retained in state; Phase 2 synthesizes the 32, Phase 3 reconstructs the 38.
- **Tasks still large for one review gate:** **T11** (assemble + all `AacDecoder`/`decode_fil`/`demux` wiring + coupling) and **T10** (all of §9.1–§9.4 in one unit) are the heaviest. Both are kept whole because the reference presents them as single coherent units and the coupling/wiring only makes sense assembled; if a reviewer wants finer gates, T10 splits at the §9.3/§9.4 boundary (gain-calc vs assembly) and T11 splits at assemble-unit vs decoder-wiring. **T8 (QMF)** is the single hardest-to-get-right unit (verify-first); the spec (§7 Notes) even floats hard-splitting it into its own PR — left in-plan per the brief's one-plan default, but an executor may PR it separately.
