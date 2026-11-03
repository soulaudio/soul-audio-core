# AAC Parametric Stereo (PS) Decoder Implementation Plan (Phase 3 of HE-AACv2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Algorithm bible:** every decoder task must read `.superpowers/sdd/ps-reference.md` (source-verified PS reference: 8 numbered sections `§1`..`§8` covering the `ps_data` bitstream, frame/bands, param Huffman+dequant, band mapping, hybrid filterbank, decorrelation, stereo mixing, assembly+seam) at the section it cites (`§N`). The big verbatim tables (Huffman trees, dequant/angle tables, band maps, hybrid FIR prototypes, allpass/phase tables, constants) live VERBATIM in `.superpowers/sdd/ps-reference-verbatim.md` (referenced as `V1`..`V6`) — **fetch/transcribe them from there + the cited faad2/FFmpeg source, with the size + anchor self-checks each table carries**; do NOT hand-type them and do NOT inline them into this plan. This plan gives structure, interfaces, and TDD steps; the reference is the single source of algorithm truth.

**Goal:** Extend the merged `no_std`, no-alloc HE-AAC (SBR) decoder in `soul-audio-core` with **Parametric Stereo (PS)** so it decodes **HE-AACv2** end to end: a **mono** AAC-LC core → SBR high-band reconstruction (complex 64-band QMF domain) → PS parametric mono→stereo upmix inside that QMF domain → two synthesis-QMF banks → stereo PCM at **2× the core rate**. **Full PS**, not a subset: IID + ICC + IPD/OPD, both the 20-band and 34-band parameter configs including the fine-IID modes (`iid_mode`/`icc_mode` 0..5), Type A and Type B stereo mixing, the hybrid filterbank for both configs, decorrelation (allpass + pure delay + transient attenuation). PS is the **only** stage in the whole pipeline that turns 1 channel into 2.

**Architecture:** PS is **additive** to the merged Phase-2 SBR pipeline. It touches SBR at exactly **two** points, both inside `formats/aac/sbr/`:

1. **Parse-time** — inside SBR's trailing `bs_extension` region (`sbr/bitstream.rs` → `super::drive`): when a sub-extension is tagged `EXTENSION_ID_PS (== 2)`, its `ps_data()` payload is handed to `PsDecoder::parse_frame`, which delta-decodes and dequantizes this frame's IID/ICC/IPD/OPD parameters and returns the exact bit count consumed.
2. **QMF-domain** — right after SBR assembles the **mono** 64-band matrix for the current frame (`sbr/mod.rs::drive`, the point just before `qmf_synthesis`): when PS is active, the assembled mono matrix is handed to `PsDecoder::apply`, which returns **two** matrices `X_left`, `X_right`; the driver then calls `qmf_synthesis` **twice** (one synthesis-QMF state per output channel) to emit stereo PCM.

New submodule tree under `formats/aac/ps/` (all `no_std`, no-alloc, `#![forbid(unsafe_code)]`), wired from `formats/aac/sbr/` (parse + apply seams) and owned by `SbrState` (one `PsDecoder` per stream — the core is always mono, so there is exactly one PS stage). PS const tables are generated host-side by a new `cargo xtask gen-ps-tables` subcommand (mirroring `gen-sbr-tables`), CI-guarded by `check-ps-tables` wired into `cargo xtask verify`.

**Tech Stack:** Rust (edition 2024), `#![no_std]` + no-alloc + `#![forbid(unsafe_code)]`. The real HE-AACv2/PS conformance vector is **license-restricted** and NEVER committed; committed regression coverage is per-unit synthetic-input DSP tests verified against faad2/FFmpeg values. Local dev verification (segmental SNR vs ffmpeg, per-channel; a structural stereo gate) reads a **gitignored local fixture at test runtime and SKIPS if it is absent** — a no-op in CI/clean checkout (see the Testing strategy below and Task 0/Task 10).

## Global Constraints

- `#![no_std]` (outside `test`), no heap allocation in non-test code (fixed by-value buffers only), `#![forbid(unsafe_code)]`.
- **NO `libm`, NO new dependency** (the `architecture-tests` dependency allowlist is `embedded-io`-only). ALL trig / allpass / phase / dequant values PS needs — Huffman trees (V1), IID/ICC/IPD/OPD dequant + mixing-angle tables (V2), flattened↔parameter band maps (V3), the modulated complex hybrid FIR coefficients (from the V4 real prototypes), the `Phi_Fract`/`Q_Fract_allpass`/`filter_a` allpass tables (V5), and the config constants (V6) — are precomputed `const` tables emitted by `cargo xtask gen-ps-tables`, guarded by a `check-ps-tables` gate wired into `cargo xtask verify`. **IPD/OPD uses FFmpeg's `atan2`-free precomputed complex smoothing tables `pd_re_smooth`/`pd_im_smooth[8×8]`, NOT faad2's runtime `atan2`.** Adopt **FFmpeg's compile-time constants as authoritative**: `NR_ALLPASS_BANDS={30,50}`, `DECAY_CUTOFF={10,32}`, `SHORT_DELAY_BAND={42,62}`, and FFmpeg's direct-6 hybrid form for the 20-config band-0 split.
- **Firmware no-panic invariant:** every PS bitstream field is bounds-checked → `Err(DecodeError::Malformed)`, never a panic / OOB index / infinite loop. Use `ps-reference.md` §8's `[NOPANIC]` checklist directly (it is a complete per-field enumeration: `bs_extension_id`, extension byte-count vs `nr_bits_left`, `enable_ps_header`/header-seen gating, `iid_mode`/`icc_mode` range, `nr_ipdopd_par` derivation, `frame_class`, `num_env`, `border_position[]` monotonicity, Huffman leaf-walk termination, post-accumulate IID/ICC/IPD/OPD clip/wrap, `nr_*_par` sourced only from V6 tables, `k_to_i[k]` bounds, the transient `G_trans` divide guard). A malformed PS header or any subsequent derived-table/parse failure **disables PS for that frame** (mono passthrough via SBR's existing single-`qmf_synthesis` path) rather than propagating a hard decode error — mirroring SBR's graceful-degradation stance and extending `ps_data_available`'s "reuse last header / no-op" semantics.
- Every task must pass, before its commit: `cargo test -p soul-audio-core`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all` (clean), `cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features`, and `cargo test -p architecture-tests`.
- Commit messages carry **NO AI-authorship trailers** (no `Co-Authored-By: Claude`, no `Claude-Session`) — lefthook rejects them.
- Read `.superpowers/sdd/ps-reference.md` §N (the section each task cites) before writing that task's code. Big tables → fetch from `ps-reference-verbatim.md` §V + the cited faad2/FFmpeg source, verify the size + anchor self-checks, do not hand-type.

---

## Spec-vs-code reconciliations (read before Task 6 and Task 9)

The design spec (`docs/superpowers/specs/2026-08-27-ps-decoder-design.md`) and `ps-reference.md` describe the seam against a **design sketch** (`X[38][64]`). The **merged SBR code** is the authority on the real types. These reconciliations are baked into the task interfaces below; they are the load-bearing corrections a fresh implementer must know.

- **R1 — The seam type is `[[Cpx; 64]; 32]`, not `X[38][64]`.** `sbr/qmf.rs` defines `pub(crate) struct Cpx { pub re: f32, pub im: f32 }`. SBR's `assemble` (and the `i_Temp` overlap patch in `sbr/mod.rs::run_channel_hf`) produce the current frame's assembled mono matrix as `x_out: [[Cpx; N_SYN]; TIME_SLOTS_RATE]` = **`[[Cpx; 64]; 32]`** (`[slot][band]`, 32 slots, 64 bands, **no** forward/history slots on the assembled matrix). The `[38]` shape appears in SBR only on the *adjusted-high-band* buffer `Y` (`MAX_Y_SLOTS = 38`) and on `x_low` (`MAX_SLOTS = 40`) — **neither is the assembled matrix**. So PS's seam is: consume `&[[Cpx; 64]; 32]` (the current mono frame), return two `&mut [[Cpx; 64]; 32]`. The **6 group-delay history slots** the 13-tap hybrid FIR needs are **PS's own responsibility** — `PsDecoder` retains the previous frame's trailing 6 assembled slots internally (`x_hist: [[Cpx; 64]; 6]`) and prepends them to form the working `[[Cpx; 64]; 38]` its hybrid analysis reads as `X[n+6]` (§5). The "38" is a PS-internal working shape, not the seam type.
- **R2 — The SBR trailing `bs_extension` loop must be rewritten to the conformant structure.** `sbr/bitstream.rs`'s current loop is a **Phase-2 placeholder** (`while … { id=read(2); len=read(8); skip(len·8) }`) that is *not* the ISO `sbr_extension_data` structure — it never reads the `bs_extended_data` gate or the `cnt` byte count, and it invents a per-sub-extension 8-bit length. It "works" only because Phase 2 never parses a real extension and pads to `end_bit`. PS's payload rides in the **real** structure (`ps-reference.md` §1): `bs_extended_data`(1), then `cnt = read(4) (+read(8) if 15)`, then `while nr_bits_left>7 { id=read(2); nr_bits_left-=2; nr_bits_left -= sbr_extension(id) }`. Task 9 replaces the placeholder with this and dispatches `id == EXTENSION_ID_PS` to `PsDecoder::parse_frame`, trusting its returned bit count.
- **R3 — PS param parse must run BEFORE the QMF-domain synthesis.** In the merged code, `sbr_extension_data` calls `super::drive` (which does parse **and** the full DSP incl. `qmf_synthesis`) and only *then* walks the trailing extension loop. That order is fatal for PS: `ps_data` (in the extension loop) carries **this frame's** parameters, which `apply` must consume before synthesizing. Task 9 fixes the ordering by **moving the trailing `bs_extension` loop parse into `drive`, placed after `parse_sbr_data` and before the per-channel synthesis** (faad2/FFmpeg's natural parse-everything-then-DSP order). `sbr_extension_data` keeps only the final pad-to-`end_bit`. The non-PS path stays bit-identical (the SBR conformance golden must remain green — it is a Task 9 gate).
- **R4 — Double synthesis uses SBR's two channel synthesis states; PS never calls `qmf_synthesis`.** `qmf::qmf_synthesis` is `pub(super)` within `sbr` and unreachable from `ps`. So `apply` returns the two matrices and the **SBR driver** synthesizes: left via `state.ch[0].qmf_syn`, right via `state.ch[1].qmf_syn`. For a mono HE-AACv2 core, SBR only ever populates `ch[0]`, so `ch[1].qmf_syn` (a persistent, frame-to-frame `QmfSynthesisState`) is free for PS's right channel. No new synthesis state is introduced.
- **R5 — `Cpx` sharing / module visibility.** PS reuses `crate::formats::aac::sbr::qmf::Cpx` (already `pub(crate)`) — it does **not** define a second complex type. The module path resolves from `formats::aac::ps` (`sbr` is `pub(crate)`, `qmf` is `pub(super)`/`pub(in formats::aac)`, `Cpx` is `pub(crate)`). PS defines its **own** dimension constants (`N_QMF=64`, `N_SLOTS=32`, `HISTORY=6`, `N_SLOTS_HIST=38`, `NR_BANDS_MAX=91`) rather than importing SBR's `pub(super)` ones; array types are structural so `[[Cpx; 64]; 32]` from `ps` matches `[[Cpx; N_SYN]; TIME_SLOTS_RATE]` from `sbr` at the call site. If the compiler rejects a path, promote `qmf`/`qmf_synthesis` to `pub(crate)` (noted in Task 9) — but do not duplicate `Cpx`.
- **R6 — Channel-count promotion.** `AacDecoder.channels` starts at `config.channels` (== 1 for a mono HE-AACv2 core). PS promotes it **1→2** the first time `apply` succeeds, mirroring SBR's `sbr_confirmed` sample-rate promotion. Plumbed via a new `SbrOutput.channels` field (set to 2 when PS ran) that `decode_rdb` reads for the interleave and reports up so `AacDecoder` sets `self.channels = 2` / a `ps_confirmed` flag; `stream_info().channels` then reports 2. `PCM_CAP` already = `2048 * 2 * 2` — a PS-active mono-core-to-stereo frame is exactly `2048 * 2` interleaved samples, no sizing change.

---

## Testing strategy (settled — no committed conformance golden)

The real HE-AACv2/PS conformance vector is **license-restricted** (Coding Technologies DecoderCheck package). It lives **gitignored** at `crates/soul-audio-core/testdata/ps-local.m4a` + `ps-local.ref.pcm` (ffmpeg's own stereo decode). The repo's `crates/soul-audio-core/testdata/.gitignore` already carries `/crates/soul-audio-core/testdata/ps-local.*` and both files are present in this working tree. Therefore:

- **COMMITTED regression coverage = per-unit DSP tests only.** Each unit task (Tasks 1–8) ships known-vector tests with **synthetic / hand-built inputs** verified against faad2/FFmpeg values: the modulated hybrid FIR against a QMF-domain impulse/tone (§5), the allpass impulse response (§6), the mixing matrix from a known IID/ICC (§7 sanity `ρ=1 ⇒ L=c2·s, R=c1·s`), Huffman decode of known codewords (V1.b cross-check), the `k_to_i` band maps against V3 anchors, dequant against V2 anchors. These run in CI and are the real regression net.
- **LOCAL-ONLY conformance + structural gates.** Both need a real PS bitstream, so **both** read the fixture via `std::fs::read` at **test runtime** and **return early (skip, not fail)** if it is absent — a no-op in CI/clean checkout, a real gate on a dev box that has the fixture. `include_bytes!` is forbidden here (it would fail to compile without the file). Conformance = **per-channel** segmental SNR vs ffmpeg (de-interleave L and R, one shared priming offset, assert **both** channels clear the calibrated bar — a one-channel bug must not average out). Structural = decode succeeds, output `channels == 2`, per-channel length matches the 2× reference within a frame, and **the two channels genuinely differ** (`RMS(L−R)/RMS(L)` above a small threshold — catches a mono-duplicated-to-stereo / PS-silently-disabled bug).
- **Robustness fuzz** = a **synthetic** committed seed exercising `ps_data`'s fields (built by hand so it is committable), plus the local fixture added as a fuzz seed **only when present** (skip-if-absent). Every PS field bounds-checked → `Err`/disable-PS, never panic/OOB.

---

## File structure

- `crates/soul-audio-core/testdata/ps-local.m4a`, `ps-local.ref.pcm` (EXISTING, **gitignored, never committed**) — license-restricted local PS fixture + ffmpeg's s16le stereo reference. Already in place in this working tree.
- `crates/soul-audio-core/testdata/.gitignore` (EXISTING) — already ignores `ps-local.*`; no change needed.
- `xtask/src/ps_tables.rs` (NEW) — host-side generator for the PS const tables (V1 Huffman trees, V2 dequant/angle + `pd_*_smooth`, V3 band maps, V4→modulated complex hybrid FIR, V5 allpass/phase, V6 constants); `run`/`check` entry points, mirroring `xtask/src/sbr_tables.rs`.
- `xtask/src/main.rs` (MODIFY) — register `gen-ps-tables` / `check-ps-tables`; add `ps_tables::check()?` to `verify`; extend `USAGE`.
- `crates/soul-audio-core/src/formats/aac/ps/mod.rs` (NEW) — `PsDecoder` (persistent state + per-frame scratch), `parse_frame`, `apply`, `ps_data_available`, the shared dimension constants + `Cpx` re-import; submodule decls.
- `crates/soul-audio-core/src/formats/aac/ps/tables.rs` (NEW, generated, checked in) — the PS const tables.
- `crates/soul-audio-core/src/formats/aac/ps/data.rs` (NEW) — `ps_data()` bitstream parse + return-count (§1).
- `crates/soul-audio-core/src/formats/aac/ps/frame.rs` (NEW) — frame_class/num_env/borders + band counts → `PsFrameLayout` (§2).
- `crates/soul-audio-core/src/formats/aac/ps/params.rs` (NEW) — Huffman + delta + clip/wrap + dequant + resolution up-map → `PsParams` (§3).
- `crates/soul-audio-core/src/formats/aac/ps/bands.rs` (NEW) — `k_to_i` band mapping + config constants (§4).
- `crates/soul-audio-core/src/formats/aac/ps/hybrid.rs` (NEW) — hybrid analysis/synthesis, modulated complex FIR (§5). **Hard unit #1.**
- `crates/soul-audio-core/src/formats/aac/ps/decorrelate.rs` (NEW) — per-band allpass + delay + transient attenuation (§6). **Hard unit #2.**
- `crates/soul-audio-core/src/formats/aac/ps/mix.rs` (NEW) — 2×2 mixing matrix + slot interpolation, Type A/B + IPD/OPD (§7). **Hard unit #3.**
- `crates/soul-audio-core/src/formats/aac/mod.rs` (MODIFY) — `pub(crate) mod ps;`; channel 1→2 promotion (`ps_confirmed`, use `sbr_out.channels` for the interleave).
- `crates/soul-audio-core/src/formats/aac/sbr/mod.rs` (MODIFY) — `SbrState.ps: ps::PsDecoder`; `drive` restructure (parse → extension loop → PS-aware synthesis); `SbrOutput.channels`.
- `crates/soul-audio-core/src/formats/aac/sbr/bitstream.rs` (MODIFY) — move the trailing `bs_extension` loop into `drive`, rewrite it to the conformant structure with `EXTENSION_ID_PS` dispatch (R2/R3).
- `crates/soul-audio-core/src/formats/aac/sbr/qmf.rs` (MODIFY, only if needed) — promote `qmf`/`qmf_synthesis` visibility per R5 if the path won't resolve (likely unnecessary).
- `crates/soul-audio-core/tests/decode.rs` (MODIFY) — local-only PS conformance (per-channel SNR) + structural (stereo, L≠R) tests, skip-if-absent.
- `crates/soul-audio-core/tests/robustness.rs` (MODIFY) — synthetic PS fuzz seed (+ local fixture seed when present).
- `crates/soul-audio-core/testdata/README.md` (MODIFY) — PS local-only fixture provenance + testing convention note.

---

## Task 0: Local-only PS test harness + skip-if-absent conformance/structural scaffold

**Files:** MODIFY `crates/soul-audio-core/tests/decode.rs` (skip-if-absent loader + two scaffold tests), `crates/soul-audio-core/testdata/README.md` (PS local-only convention). The fixture (`testdata/ps-local.m4a`, `ps-local.ref.pcm`) and its `.gitignore` entry are **already in place** — verify, do not re-source.

Read `ps-reference.md` §8 (the seam contract) + the "Testing strategy" section above. This task establishes the **license-restricted, local-only** convention as compiling, CI-safe scaffolding **before** any PS code exists: the tests must be a **no-op** in CI (fixture gitignored/absent) yet a real gate locally. Because the fixture is not committed, these tests **must not** use `include_bytes!` (compile-time embed of a missing file fails the build) — they read it with `std::fs::read` at runtime and return early if `Err`/absent.

**Interfaces — Produces (test-only helpers in `tests/decode.rs`):**
```rust
/// Load the gitignored local PS fixture at test runtime. `None` (→ the caller
/// skips) when either file is absent — the CI / clean-checkout case. NEVER
/// `include_bytes!` (the files are not committed).
fn ps_local_fixture() -> Option<(Vec<u8>, Vec<i32>)> {
    let dir = env!("CARGO_MANIFEST_DIR");
    let m4a = std::fs::read(format!("{dir}/testdata/ps-local.m4a")).ok()?;
    let refpcm = std::fs::read(format!("{dir}/testdata/ps-local.ref.pcm")).ok()?;
    Some((m4a, load_s16le(&refpcm)))   // load_s16le already exists in this file
}

/// De-interleave channel `ch` of `channels`-interleaved PCM into a mono vec.
fn deinterleave(pcm: &[i32], channels: usize, ch: usize) -> Vec<i32> {
    pcm.iter().skip(ch).step_by(channels).copied().collect()
}
```
**Consumes:** the existing `load_s16le`, `decode_all`, `best_align_segsnr`, `open`, `SliceSource` in `tests/decode.rs`.

- [ ] **Step 1: Verify the fixture + gitignore (no sourcing).** Confirm `git check-ignore crates/soul-audio-core/testdata/ps-local.m4a` prints the path (it is ignored) and that `crates/soul-audio-core/testdata/.gitignore` contains `/crates/soul-audio-core/testdata/ps-local.*`. Confirm both `ps-local.m4a` and `ps-local.ref.pcm` exist locally and that ffmpeg decodes the m4a to stereo (`ffprobe` reports 2 channels; the `.ref.pcm` is s16le stereo). If any is missing, STOP and ask the user — do not synthesize (no local encoder produces PS; see §5 of the design spec). Record the measured facts (channel count, ref length) for Task 10's calibration.
- [ ] **Step 2: Write the scaffold tests** in `tests/decode.rs` (they compile and PASS-as-skip in CI):
```rust
#[test]
fn ps_local_matches_ffmpeg_per_channel() {
    let Some((data, refr)) = ps_local_fixture() else {
        eprintln!("skip: ps-local.* fixture absent (license-restricted, local-only)");
        return;
    };
    let ours = decode_all(&data);          // interleaved i32, 2 ch (once PS lands)
    // One shared priming offset (property of the mono core + envelope timeline).
    let (off, _snr) = best_align_segsnr(&ours, &refr, 2);
    // Per-channel: de-interleave L and R, score each independently at `off`.
    // Task 10 fills PS_SNR_BAR from the measured value; Task 0 only asserts the
    // helper wiring runs (guarded so pre-PS it can't false-fail: skip if <2 ch).
    if ours.is_empty() { return; }
    let _ = (deinterleave(&ours, 2, 0), deinterleave(&refr, 2, 0), off);
    // NOTE (Task 10): assert per-channel segmental SNR > PS_SNR_BAR for BOTH L and R.
}

#[test]
fn ps_local_output_is_stereo_and_channels_differ() {
    let Some((data, refr)) = ps_local_fixture() else {
        eprintln!("skip: ps-local.* fixture absent");
        return;
    };
    let ours = decode_all(&data);
    // NOTE (Task 10): assert stream reports 2 channels, per-channel length ~= 2x
    // ref within a frame, and RMS(L-R)/RMS(L) exceeds a small threshold (the two
    // channels genuinely differ — not mono duplicated). Guarded no-op pre-PS:
    let _ = refr;
    if ours.is_empty() { return; }
}
```
- [ ] **Step 3: README provenance.** Add an `## AAC Parametric Stereo (HE-AACv2, PS) — local-only fixture` section to `testdata/README.md`: the fixture is **license-restricted** (Coding Technologies DecoderCheck), **gitignored, never committed**, used only for local dev verification (per-channel segmental SNR vs ffmpeg + a structural stereo gate); CI/clean checkouts skip these tests; committed PS coverage is the per-unit synthetic-input DSP tests. Note that no local encoder produces HE-AACv2/PS (the SBR fixtures are self-encoded, PS is not).
- [ ] **Step 4:** Run `cargo test -p soul-audio-core --test decode` → both scaffold tests PASS (locally: run + skip-guarded; CI: skip). `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all` clean, thumbv7em build, arch-tests clean.
- [ ] **Step 5: Commit** `test(ps): local-only skip-if-absent conformance + structural scaffold`.

> The scaffold intentionally does not assert SNR/stereo yet (there is no PS code) — it wires the loader, the skip semantics, and the per-channel/de-interleave helpers so Task 10 only fills in the calibrated asserts. Keeping it green from Task 0 proves the CI no-op path works before any PS lands.

---

## Task 1: `xtask gen-ps-tables` / `check-ps-tables` + `ps/tables.rs` + runtime self-check

**Files:** Create `xtask/src/ps_tables.rs`; MODIFY `xtask/src/main.rs`; Create `crates/soul-audio-core/src/formats/aac/ps/mod.rs` (skeleton: `pub(crate) mod tables;` + the dimension constants + `Cpx` re-import), `crates/soul-audio-core/src/formats/aac/ps/tables.rs` (generated, checked in); MODIFY `crates/soul-audio-core/src/formats/aac/mod.rs` (add `pub(crate) mod ps;`).

Read `ps-reference.md` §3/§5/§6/§7 + all of `ps-reference-verbatim.md` V1–V6. Exact precedent: `xtask/src/sbr_tables.rs` (`generate()`/`run()`/`check()`/`emit` helpers, `TABLES_PATH`, the `#[test]` host-side anchor re-check) + its `main.rs` wiring. This task emits **all** PS const tables so no downstream task needs `libm`. The core module starts `#![allow(dead_code)]` with a comment marking it removable once Tasks 2–8 consume the tables.

**Tables to generate** (host-side `f64` math where computed; write `const`/`static` arrays into `ps/tables.rs`). For each: **fetch/transcribe from `ps-reference-verbatim.md` §V + the cited faad2/FFmpeg source, verify size + anchor host-side in an `xtask` `#[test]`**, do not hand-type blindly.

- **Huffman trees (V1.a, faad2 binary-tree form)** — `F_HUFF_IID_DEF[28][2]`, `T_HUFF_IID_DEF[28][2]`, `F_HUFF_IID_FINE[60][2]`, `T_HUFF_IID_FINE[60][2]`, `F_HUFF_ICC[14][2]`, `T_HUFF_ICC[14][2]`, `F_HUFF_IPD[7][2]`, `T_HUFF_IPD[7][2]`, `F_HUFF_OPD[7][2]`, `T_HUFF_OPD[7][2]` as `[[i8; 2]; N]`. Emit verbatim (they are integers — transcribe, do not compute). Anchor: every table row 0 has a `-31` leaf; sizes 28/28/60/60/14/14/7/7/7/7. (The FFmpeg V1.b `{value,length}` form + `huff_offset={-30,-30,-14,-14,-7,-7,0,0,0,0}` is the cross-check used in Task 4's unit tests, not emitted.)
- **IID dequant (V2.a)** — `SF_IID_NORMAL[15]` (`no_iid_steps=7`), `SF_IID_FINE[31]` (`no_iid_steps=15`) as `[f32; N]`. Anchor: center entry (`[7]`/`[15]`) == `1.0`.
- **ICC dequant + mixing α (V2.b)** — `ICC_INVQ[8]` (ρ), `COS_ALPHAS[8]`, `SIN_ALPHAS[8]` as `[f32; 8]`. Anchor: `COS_ALPHAS[5]==SIN_ALPHAS[5]==0.70710678`; consistency `ρ = 2·cos_alphas²−1` reproduces `ICC_INVQ`.
- **IPD/OPD angle (V2.c)** — `IPDOPD_COS[8]`, `IPDOPD_SIN[8]` as `[f32; 8]` (`θ=index·π/4`; `±√½`, `0`, `±1`). Compute host-side from the closed form.
- **IPD/OPD complex smoothing (V2.c / §7, FFmpeg `aacps_tablegen.h`)** — `PD_RE_SMOOTH[8*8]`, `PD_IM_SMOOTH[8*8]` as `[f32; 64]` (index `hist*8 + cur`). **This is the `atan2`-free authoritative path** (Global Constraints). Compute host-side per FFmpeg's generator (the 3-frame `[¼,½,¼]` smoothing folded into the table); anchor against FFmpeg's emitted values.
- **Type-B / β / γ angle tables (V2.d)** — either transcribe `COS_BETAS_*`/`SIN_BETAS_*`/`COS_GAMMAS_*`/`SIN_GAMMAS_*`/`SINCOS_ALPHAS_B_*` (dims + anchors in V2.d) **or** decide to compute `H11..H22` from the §7 closed forms in Task 8 and emit only what that needs. **Recommendation:** emit the faad2 tables (they are an optimization and match the conformance oracle's math exactly); if you compute from closed forms instead, document that Task 8 depends on `SF_IID_*`/`COS_ALPHAS`/`SIN_ALPHAS` only. Whichever you pick, be consistent and note it in this task's commit.
- **Flattened↔param band maps (V3)** — `FF_K_TO_I_20[71]` (values 0..19), `FF_K_TO_I_34[91]` (values 0..33) as `[u8; N]`. Anchor: lengths 71/91, max 19/33. Also the resolution up-maps (10→20, 20→34) — transcribe the faad2/FFmpeg map arrays (used by Task 4's resolution expansion); anchor against V3 / FFmpeg `map_val_*`.
- **Hybrid FIR → modulated complex filters (V4, §5)** — transcribe the real 13-tap prototypes `G0_Q12[7]`, `G1_Q8[7]`, `G2_Q4[7]`, `G0_Q8[7]` (7 half-coeffs each, mirror to 13). Then **precompute the modulated complex per-subband filters** `filter[q][n] = proto[n]·exp(−i·2π·(q+½)·(n−6)/bands)`, `n=0..12`, for each split: `HYB_FILT_12: [[Cpx;13];12]` (band-0 34-cfg, bands=12), `HYB_FILT_8: [[Cpx;13];8]` (band-1 34-cfg), `HYB_FILT_4: [[Cpx;13];4]` (bands 2–4 34-cfg), `HYB_FILT_6: [[Cpx;13];6]` (band-0 20-cfg, **FFmpeg direct-6** form). Emit as `Cpx` from `sbr::qmf` (or a plain `[f32;…]` re/im pair). Anchor: each proto's center coeff == `1/bands`; full 13-tap sums to 1; a modulated filter's DC-ish check per the reference.
- **Allpass / decorrelation phase (V5, §6)** — `FILTER_A[3] = {0.65143905753106, 0.56471812200776, 0.48954165955695}`; `NUM_SAMPLE_DELAY_SER[3] = {3,4,5}`; `DELAY_D[64]` (14 for bands 0..34, 1 for 35..63). Fractional-phase tables `PHI_FRACT_QMF[64]`, `PHI_FRACT_SUBQMF20[12]`, `PHI_FRACT_SUBQMF34[32]`, `Q_FRACT_ALLPASS_QMF[64][3]`, `Q_FRACT_ALLPASS_SUBQMF20[12][3]`, `Q_FRACT_ALLPASS_SUBQMF34[32][3]` as `Cpx` — either transcribe from faad2 `ps_tables.h` (dims + first/last anchors in V5) **or** compute host-side from `F_CENTER_20[10]`/`F_CENTER_34[32]` (V5) per the exp formulas. Anchors: V5's first/last per array.
- **Config constants (V6)** — `NUM_ENV_TAB[2][4]={{0,1,2,4},{1,2,3,4}}`, `NR_IIDICC_PAR_TAB[6]={10,20,34,10,20,34}`, `NR_IIDOPD_PAR_TAB[6]={5,11,17,5,11,17}`, plus the scalars (`DECAY_SLOPE=0.05`, `ALPHA_DECAY=0.76592833836465`, `ALPHA_SMOOTH=0.25`, `GAMMA=1.5`, `NEGATE_IPD_MASK=0x1000`). (`NR_BANDS`/`NR_ALLPASS_BANDS`/`DECAY_CUTOFF`/`SHORT_DELAY_BAND`/`NR_PAR_BANDS`/`NR_IPDOPD_BANDS` live in `ps/bands.rs`, Task 5 — the boundary between "generated numeric tables" here and "config constants" there.)

- [ ] **Step 1: Failing test** — the runtime self-check in `ps/tables.rs` (tests may use `std` trig/`f64` to re-derive):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use core::f64::consts::PI;
    #[test]
    fn huffman_tree_sizes_and_root_leaf() {
        assert_eq!(F_HUFF_IID_DEF.len(), 28);
        assert_eq!(F_HUFF_IID_FINE.len(), 60);
        assert_eq!(F_HUFF_ICC.len(), 14);
        assert_eq!(F_HUFF_IPD.len(), 7);
        // Row 0 of every tree carries the most-probable symbol (leaf -31).
        for t in [&F_HUFF_IID_DEF[..], &T_HUFF_IID_DEF, &F_HUFF_ICC, &F_HUFF_IPD, &F_HUFF_OPD] {
            assert!(t[0][0] == -31 || t[0][1] == -31);
        }
    }
    #[test]
    fn iid_icc_dequant_anchors() {
        assert!((SF_IID_NORMAL[7] - 1.0).abs() < 1e-9);   // center == 1.0
        assert!((SF_IID_FINE[15] - 1.0).abs() < 1e-9);
        assert!((COS_ALPHAS[5] - 0.70710678).abs() < 1e-6);
        assert!((SIN_ALPHAS[5] - 0.70710678).abs() < 1e-6);
        for q in 0..8 {  // ρ = 2·cos²−1 reproduces icc_invq
            let rho = 2.0 * f64::from(COS_ALPHAS[q]).powi(2) - 1.0;
            assert!((rho - f64::from(ICC_INVQ[q])).abs() < 1e-3);
        }
    }
    #[test]
    fn ipdopd_angle_and_band_maps() {
        for q in 0..8 {
            let a = q as f64 * PI / 4.0;
            assert!((f64::from(IPDOPD_COS[q]) - a.cos()).abs() < 1e-6);
            assert!((f64::from(IPDOPD_SIN[q]) - a.sin()).abs() < 1e-6);
        }
        assert_eq!(FF_K_TO_I_20.len(), 71);
        assert_eq!(FF_K_TO_I_34.len(), 91);
        assert_eq!(*FF_K_TO_I_20.iter().max().unwrap(), 19);
        assert_eq!(*FF_K_TO_I_34.iter().max().unwrap(), 33);
    }
    #[test]
    fn hybrid_proto_centers_and_allpass_gains() {
        // proto center coeff == 1/bands (last of the 7 half-coeffs).
        assert!((f64::from(G0_Q12[6]) - 1.0/12.0).abs() < 1e-6);
        assert!((f64::from(G1_Q8[6]) - 1.0/8.0).abs() < 1e-6);
        assert!((f64::from(G2_Q4[6]) - 0.25).abs() < 1e-6);
        assert_eq!(HYB_FILT_12.len(), 12);
        assert_eq!(HYB_FILT_6.len(), 6);
        assert!((f64::from(FILTER_A[0]) - 0.65143905753106).abs() < 1e-9);
        assert_eq!(NUM_SAMPLE_DELAY_SER, [3, 4, 5]);
    }
}
```
- [ ] **Step 2:** Run `cargo test -p soul-audio-core --lib formats::aac::ps::tables` → FAIL (`ps` / tables undefined).
- [ ] **Step 3: Implement.**
  - `xtask/src/ps_tables.rs`: mirror `sbr_tables.rs` — `const TABLES_PATH = "crates/soul-audio-core/src/formats/aac/ps/tables.rs"`, a `generate() -> String` emitting `#![allow(dead_code)]` + a `// @generated` banner + each table via `emit`/`emit2d` (reuse the `lit`/`Write` helpers' shape), `pub fn run()`/`pub fn check()`. Transcribe the integer/verbatim tables (Huffman, `SF_IID_*`, `ICC_INVQ`, `COS/SIN_ALPHAS`, V3 maps, V4 protos, V5 phase tables) as source literals; compute `IPDOPD_COS/SIN`, `PD_*_SMOOTH`, and the modulated `HYB_FILT_*` (from the protos) host-side with `f64` trig. Add an `xtask` `#[test]` re-checking every anchor so a bad paste fails host-side too.
  - `xtask/src/main.rs`: `mod ps_tables;`, arms `"gen-ps-tables" => ps_tables::run()`, `"check-ps-tables" => ps_tables::check()`, add `ps_tables::check()?;` to `verify()` next to `sbr_tables::check()?;`, extend `USAGE`.
  - `ps/mod.rs`: `#![allow(dead_code)]` (temporary), `pub(crate) mod tables;`, `use crate::formats::aac::sbr::qmf::Cpx;`, and the dimension constants `pub(crate) const N_QMF: usize = 64; pub(crate) const N_SLOTS: usize = 32; pub(crate) const HISTORY: usize = 6; pub(crate) const N_SLOTS_HIST: usize = 38; pub(crate) const NR_BANDS_MAX: usize = 91; pub(crate) const MAX_PS_ENVELOPES: usize = 5; pub(crate) const EXTENSION_ID_PS: u32 = 2;`.
  - `formats/aac/mod.rs`: add `pub(crate) mod ps;` next to `pub(crate) mod sbr;`.
  - Run `cargo xtask gen-ps-tables` to write `ps/tables.rs`.
- [ ] **Step 4:** Run the self-check tests → PASS. Run `cargo xtask check-ps-tables` → OK (checked-in matches fresh generation).
- [ ] **Step 5:** thumbv7em build + `cargo clippy --workspace --all-targets -- -D warnings` + arch-tests clean.
- [ ] **Step 6: Commit** `feat(xtask): PS const tables (Huffman/dequant/band-maps/hybrid-FIR/allpass) + check gate`.

---

## Task 2: `ps/data.rs` — `ps_data()` bitstream parse + return-count

**Files:** Create `crates/soul-audio-core/src/formats/aac/ps/data.rs`; MODIFY `ps/mod.rs` (`pub(super) mod data;` + a skeleton `PsDecoder` holding the persistent header/param state).

Read `ps-reference.md` §1. This task parses the `ps_data()` payload (header gate + frame body + optional IPD/OPD extension) and returns the **exact bit count consumed** — the one number the SBR extension loop needs back (§1: "the SBR extension parser must trust `ps_data()`'s returned bit length"). Following the SBR-Task-2 precedent, the **frame body** (borders) and **parameter body** (Huffman/delta/dequant) are delegated to `frame::read_frame` (Task 3) and `params::read_params` (Task 4); for **this** task they are stubbed as "consume to `end_bit`", so the header parse, the `ps_data_available` gate, and the return-count are real and testable now. Tasks 3–4 replace the stubs.

**Interfaces — Produces:**
```rust
/// Persistent PS parse state (grown by later tasks with DSP scratch/state).
/// `Default` = "no header seen yet" (a frame before any header is a no-op).
pub(crate) struct PsDecoder {
    pub(crate) ps_data_available: bool, // a header has been read at least once
    // last accepted header (reused when enable_ps_header==0):
    pub(crate) enable_iid: bool,
    pub(crate) enable_icc: bool,
    pub(crate) enable_ext: bool,
    pub(crate) iid_mode: u8,            // 0..5  [NOPANIC]
    pub(crate) icc_mode: u8,            // 0..5  [NOPANIC]
    // current-frame layout + params (filled by Tasks 3/4):
    pub(crate) frame: crate::formats::aac::ps::frame::PsFrameLayout,
    pub(crate) params: crate::formats::aac::ps::params::PsParams,
    // persistent delta-time bases (Task 4):
    pub(crate) iid_index_prev: [i8; 34],
    pub(crate) icc_index_prev: [i8; 34],
    pub(crate) ipd_index_prev: [i8; 17],
    pub(crate) opd_index_prev: [i8; 17],
    // (Task 9 adds: x_hist, hybrid/decorrelate/mix state, per-frame scratch)
}

/// Bounded MSB-first read: `Some(v)` if `[.., end_bit)` allows it, else `None`
/// without advancing. The PS-path analogue of `sbr::bitstream::clamped_read`.
pub(crate) fn clamped_read(bits: &mut BitReader, end_bit: usize, n: u32) -> Option<u32>;

/// Parse `ps_data()` (§1): header gate, frame body, optional IPD/OPD extension.
/// Updates `ps`'s current- and next-frame state; returns the exact bit count
/// consumed. `end_bit` is the sub-extension budget end (never read past it).
/// [NOPANIC]: any read past `end_bit`, or an out-of-range `iid_mode`/`icc_mode`,
/// returns `Err(DecodeError::Malformed)` — the caller disables PS for the frame.
pub(crate) fn ps_data(
    ps: &mut PsDecoder,
    bits: &mut BitReader,
    end_bit: usize,
) -> Result<u32, DecodeError>;
```
**Consumes:** `crate::formats::aac::bits::BitReader` (shared with AAC-LC/SBR); `tables` (V6 mode ranges); Tasks 3/4 collaborators (stubbed here).

Layout (`ps-reference.md` §1, exact): `enable_ps_header`(1); if set → `enable_iid`(1), if `enable_iid` `iid_mode`(3) `[NOPANIC 0..5]`, `enable_icc`(1), if `enable_icc` `icc_mode`(3) `[NOPANIC 0..5]`, `enable_ext`(1), and set `ps_data_available=true`. If **no header ever seen** (`!ps_data_available`) → return the bits consumed so far (a no-op frame; mono passes through). Then the frame body: `frame_class`(1), `num_env_idx`(2) → delegate borders to `frame::read_frame` (Task 3; stub: no extra reads for fixed class). Then IID/ICC parameter bodies (delegate to `params::read_params`, Task 4; stub: skip to `end_bit`). Then if `enable_ext`: `ps_extension_size`(4, +8 if 15), the `ps_ext_id`(2) + `ps_extension()` sub-loop (IPD/OPD; delegate to Task 4), then fill bits. Return `bits.bit_pos() - entry_pos`.

- [ ] **Step 1: Failing tests** — header parse + gate + return-count (hand-built payloads):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    #[test]
    fn header_absent_before_any_header_is_noop() {
        // enable_ps_header=0 → no header yet → ps_data_available stays false,
        // consumes exactly 1 bit, returns Ok(1).
        let buf = [0u8; 2];               // first bit 0
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        let n = ps_data(&mut ps, &mut b, 16).unwrap();
        assert_eq!(n, 1);
        assert!(!ps.ps_data_available);
    }
    #[test]
    fn header_sets_modes_and_availability() {
        // enable_ps_header=1, enable_iid=1, iid_mode=010(2), enable_icc=1,
        // icc_mode=001(1), enable_ext=0, then frame_class=0, num_env_idx=00.
        // MSB-first: 1 1 010 1 001 0 | 0 00  = 1101 0100 1000 0(pad)
        let buf = [0b1101_0100, 0b1000_0000, 0, 0];
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        let _ = ps_data(&mut ps, &mut b, 32).unwrap();
        assert!(ps.ps_data_available);
        assert!(ps.enable_iid && ps.enable_icc && !ps.enable_ext);
        assert_eq!(ps.iid_mode, 2);
        assert_eq!(ps.icc_mode, 1);
    }
    #[test]
    fn rejects_out_of_range_mode() {
        // enable_ps_header=1, enable_iid=1, iid_mode=111(7) → invalid (>5) → Err.
        let buf = [0b1111_1000, 0, 0, 0];
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        assert!(ps_data(&mut ps, &mut b, 32).is_err());
    }
    #[test]
    fn overrun_past_end_bit_is_malformed() {
        let buf = [0b1000_0000u8];         // header flag=1 but no room for the rest
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        assert!(ps_data(&mut ps, &mut b, 1).is_err());
    }
}
```
- [ ] **Step 2:** Run `cargo test -p soul-audio-core --lib formats::aac::ps::data` → FAIL.
- [ ] **Step 3: Implement** `clamped_read` (mirror `sbr::bitstream::clamped_read`) and `ps_data` per §1. Validate `iid_mode`/`icc_mode` ∈ 0..=5 → else `Err`. Header-reuse: when `enable_ps_header==0` but `ps_data_available`, keep the stored `enable_*`/`*_mode`. Delegate frame/params to Tasks 3/4 (stub here: for the frame body read `frame_class`(1)+`num_env_idx`(2) and, for the parameter/ext bodies, `clamped_skip` to `end_bit`). Return the consumed-bit delta. Skeleton `PsDecoder::default()` = all-false/zero.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(ps): ps_data header parse + availability gate + return-count`.

> Interface note: Tasks 3–4 replace the frame/param stubs inside `ps_data`; `PsDecoder` grows field-by-field per task, always keeping `Default`. Task 9 wires `ps_data` behind `PsDecoder::parse_frame` and the SBR extension loop.

---

## Task 3: `ps/frame.rs` — parameter frame, envelopes, borders, band counts

**Files:** Create `crates/soul-audio-core/src/formats/aac/ps/frame.rs`; MODIFY `ps/mod.rs` (`pub(super) mod frame;`), `ps/data.rs` (call the real `read_frame`, remove the frame stub).

Read `ps-reference.md` §2. Turns `frame_class` + `num_env_idx` (read in `ps_data`) and the header's `iid_mode`/`icc_mode` into a `PsFrameLayout`: `num_env` (via `NUM_ENV_TAB`), the QMF-slot `border_position[]` (fixed even spacing, or explicit variable borders from the bitstream), and the per-type band counts + `is34`/`iid_quant`.

**Interfaces — Produces:**
```rust
#[derive(Clone, Copy, Default)]
pub(crate) struct PsFrameLayout {
    pub num_env: u8,               // 0..=5 (0 = fixed-class reuse)  [NOPANIC]
    pub border_position: [u8; 6],  // QMF slots; [0]=0, [num_env]=32   [NOPANIC monotone]
    pub nr_iid_par: u8,            // 10/20/34 (from NR_IIDICC_PAR_TAB[iid_mode])
    pub nr_icc_par: u8,            // 10/20/34
    pub nr_ipdopd_par: u8,         // 5/11/17 (from NR_IIDOPD_PAR_TAB[iid_mode])
    pub is34: bool,               // (nr_iid_par==34) || (nr_icc_par==34)
    pub iid_quant: bool,          // fine IID scale, iid_mode>=3
}

/// Build the frame layout (§2). `frame_class`/`num_env_idx` were read by
/// `ps_data`; borders are read from `bits` only for the variable class
/// (`frame_class==1`). Validates num_env range + border monotonicity [NOPANIC].
pub(crate) fn read_frame(
    bits: &mut BitReader, end_bit: usize,
    frame_class: u8, num_env_idx: u8,
    iid_mode: u8, icc_mode: u8,
) -> Result<PsFrameLayout, DecodeError>;
```
**Consumes:** `bits::BitReader`; `data::clamped_read`; `tables::{NUM_ENV_TAB, NR_IIDICC_PAR_TAB, NR_IIDOPD_PAR_TAB}` (V6).

Rules (§2): `num_env = NUM_ENV_TAB[frame_class][num_env_idx]` (range 0..4; clamp `≤ MAX_PS_ENVELOPES`). Fixed class (`frame_class==0`): `border_position[0]=0`, `border_position[e]=(e*32)/num_env`, `border_position[num_env]=32`. Variable class (`frame_class==1`): `border_position[0]=0`, `border_position[e]=read(5)+1` for `e=1..=num_env`; **require strictly increasing and `≤ 32`** (else `Err` — else §7 interpolation divides by a non-positive length). Band counts: `nr_iid_par = NR_IIDICC_PAR_TAB[iid_mode]`, `nr_icc_par = NR_IIDICC_PAR_TAB[icc_mode]`, `nr_ipdopd_par = NR_IIDOPD_PAR_TAB[iid_mode]`, `iid_quant = iid_mode >= 3`, `is34 = (nr_iid_par==34) || (nr_icc_par==34)`.

- [ ] **Step 1: Failing tests:**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    #[test]
    fn fixed_class_even_borders() {
        // frame_class=0, num_env_idx=3 → num_env=4 (NUM_ENV_TAB[0][3]).
        // iid_mode=1 (20-band, coarse), icc_mode=1 (20-band).
        let buf = [0u8; 4];
        let mut b = BitReader::new(&buf);
        let l = read_frame(&mut b, 32, 0, 3, 1, 1).unwrap();
        assert_eq!(l.num_env, 4);
        assert_eq!(&l.border_position[..5], &[0, 8, 16, 24, 32]);
        assert_eq!(l.nr_iid_par, 20);
        assert_eq!(l.nr_icc_par, 20);
        assert_eq!(l.nr_ipdopd_par, 11);
        assert!(!l.is34 && !l.iid_quant);
    }
    #[test]
    fn is34_and_fine_from_modes() {
        // iid_mode=5 → 34-band fine; icc_mode=2 → 34-band.
        let buf = [0u8; 4];
        let mut b = BitReader::new(&buf);
        let l = read_frame(&mut b, 32, 0, 1, 5, 2).unwrap();  // num_env_idx=1 → 1
        assert_eq!(l.num_env, 1);
        assert_eq!(l.nr_iid_par, 34);
        assert!(l.is34 && l.iid_quant);
        assert_eq!(l.nr_ipdopd_par, 17);
    }
    #[test]
    fn variable_borders_must_increase() {
        // frame_class=1, num_env_idx=1 → num_env=2 (NUM_ENV_TAB[1][1]).
        // borders read as read(5)+1: give 3 then 2 (decreasing) → Err.
        // bits: 00010 00001 = border[1]=3, border[2]=2.
        let buf = [0b0001_0000, 0b0100_0000, 0, 0];
        let mut b = BitReader::new(&buf);
        assert!(read_frame(&mut b, 32, 1, 1, 1, 1).is_err());
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** per §2; wire `ps_data` to call `read_frame` (dropping the Task-2 stub) and store into `ps.frame`.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(ps): parameter frame layout (num_env, borders, band counts, is34)`.

---

## Task 4: `ps/params.rs` — parameter Huffman + delta + clip/wrap + dequant

**Files:** Create `crates/soul-audio-core/src/formats/aac/ps/params.rs`; MODIFY `ps/mod.rs` (`pub(super) mod params;`), `ps/data.rs` (call the real IID/ICC + IPD/OPD param readers, remove the param/ext stubs).

Read `ps-reference.md` §3. Per parameter type, per envelope: read `dt`(1), Huffman-decode `nr_*_par` signed deltas (V1.a binary tree), accumulate (delta-freq or delta-time), clip/wrap `[NOPANIC]`, dequantize to linear via V2 tables, and **resolution-expand** to the working resolution (34 if `is34` else 20) so downstream indexing via `bands::k_to_i` is direct. Updates the persistent `*_index_prev[]` bases for next frame's delta-time env-0.

**Interfaces — Produces:**
```rust
#[derive(Clone, Copy, Default)]
pub(crate) struct PsParams {
    // per-envelope, per-WORKING-band DEQUANTIZED values (index 0..34 used;
    // 0..20 when !is34). IID → channel-level ratio c; ICC → (cos α, sin α):
    pub iid_c:   [[f32; 34]; 5],
    pub icc_cos: [[f32; 34]; 5],
    pub icc_sin: [[f32; 34]; 5],
    // IPD/OPD → (cos θ, sin θ) on the lowest nr_ipdopd_par working bands:
    pub ipd_cos: [[f32; 17]; 5],
    pub ipd_sin: [[f32; 17]; 5],
    pub opd_cos: [[f32; 17]; 5],
    pub opd_sin: [[f32; 17]; 5],
    pub enable_ipdopd: bool,   // set once the ext actually carried IPD/OPD symbols
}

/// Selects the Huffman table pair + clip/wrap policy + dequant table.
#[derive(Clone, Copy)]
pub(crate) enum ParamKind { IidCoarse, IidFine, Icc, Ipd, Opd }

/// Decode one parameter type across all envelopes: read `dt` per envelope,
/// Huffman-decode `nr_par` deltas, accumulate (freq/time vs `index_prev` for
/// env 0), clip/wrap [NOPANIC]. Fills `raw[env][band]` (post clip/wrap indices)
/// and updates `index_prev` to the last envelope's indices.
pub(crate) fn decode_param(
    bits: &mut BitReader, end_bit: usize,
    kind: ParamKind, num_env: u8, nr_par: u8,
    index_prev: &mut [i8],            // len nr_par; persistent delta-time base
    raw: &mut [[i8; 34]; 5],
) -> Result<(), DecodeError>;

/// Walk a V1.a binary tree: `i=0; while i>=0 { i = t[i][bit] } ; return i + 31`.
/// Bounded: a read past `end_bit` or an out-of-range node → Err (no infinite loop).
pub(crate) fn ps_huff_dec(bits: &mut BitReader, end_bit: usize, t: &[[i8; 2]]) -> Result<i32, DecodeError>;
```
**Consumes:** `bits::BitReader`; `data::clamped_read`; `tables` (V1 trees, V2 `SF_IID_*`/`ICC_INVQ`/`COS_ALPHAS`/`SIN_ALPHAS`/`IPDOPD_COS/SIN`, and the 10→20 / 20→34 resolution maps); `frame::PsFrameLayout`; the persistent `*_index_prev` arrays. `ps_data` orchestrates: IID then ICC in the main body; IPD then OPD inside the `enable_ext` sub-loop.

Accumulation (§3): `dt==0` (delta-freq) `par[b]=par[b-1]+delta[b]`, `par[-1]=0`; `dt==1` (delta-time) `par[e][b]=par_prev[b]+delta[b]` where `par_prev` = the previous **envelope**, or for `e==0` the previous frame's `index_prev[b]`. Clip/wrap `[NOPANIC]`: IID → `±7` (coarse) / `±15` (fine); ICC → `[0,7]`; IPD/OPD → **wrap mod 8** → `[0,7]`. Dequant: IID `c = SF_IID[no_iid_steps + q]`; ICC `(cos α, sin α) = (COS_ALPHAS[q], SIN_ALPHAS[q])`; IPD/OPD `(cos, sin) = (IPDOPD_COS[q], IPDOPD_SIN[q])`. Then resolution-expand the dequantized per-band values to the working resolution (34 if `is34` else 20) via the fixed 10→20 / 20→34 maps (verify against faad2/FFmpeg `map_val_*`).

- [ ] **Step 1: Failing tests** (Huffman cross-checked vs V1.b; delta + clip hand-computed):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    use crate::formats::aac::ps::tables::{F_HUFF_ICC, SF_IID_NORMAL, COS_ALPHAS};
    #[test]
    fn huff_root_leaf_decodes_zero_delta() {
        // Every tree's root has a -31 leaf → the shortest codeword decodes to
        // delta 0 (-31 + 31). Feed the bit that lands on that leaf.
        // F_HUFF_ICC row0 = {-31,1}: bit0=0 → leaf -31 → value 0.
        let buf = [0u8; 2];
        let mut b = BitReader::new(&buf);
        assert_eq!(ps_huff_dec(&mut b, 16, &F_HUFF_ICC).unwrap(), 0);
    }
    #[test]
    fn iid_delta_freq_accumulate_and_dequant() {
        // Single envelope, dt=0 (delta-freq), nr_par small; craft deltas so the
        // accumulated indices are known, then check dequant hits SF_IID anchors.
        // (Construct the exact codewords from V1.a F_HUFF_IID_DEF; verify the
        //  decoded index path against V1.b {value,length}+huff_offset.)
        let mut raw = [[0i8; 34]; 5];
        let mut prev = [0i8; 20];
        // ... hand-built bitstream that yields raw[0][0]=0, raw[0][1]=... ;
        // assert (SF_IID_NORMAL[7 + raw[0][0]] - 1.0).abs() < 1e-6  (index 0 -> 1.0)
        let _ = (&mut raw, &mut prev, &SF_IID_NORMAL, &COS_ALPHAS);
    }
    #[test]
    fn icc_clips_to_0_7() {
        // Accumulated ICC index below 0 or above 7 must clip; index used to read
        // COS_ALPHAS must always be in 0..8 (no OOB).
        // ... craft deltas overshooting; assert no panic and clipped result.
    }
    #[test]
    fn ipd_wraps_mod_8() {
        // IPD index wraps mod 8 (phase cyclic): delta pushing to 9 → 1.
    }
}
```
(Fill the `...` bodies by transcribing exact codewords from V1.a and hand-computing the accumulation; the V1.b `{value,length}` + `huff_offset` form is the ready-made cross-check for the decoded deltas.)
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `ps_huff_dec` (bounded tree walk, `+31` offset), `decode_param` (per-env dt read + delta accumulate + clip/wrap), the dequant helpers, and the resolution up-map. Wire `ps_data`: after `read_frame`, call the IID/ICC decoders; inside `enable_ext`, the IPD/OPD decoders (set `enable_ipdopd`). Store dequantized+expanded values into `ps.params`; update `ps.iid_index_prev`/`icc_index_prev`/`ipd_index_prev`/`opd_index_prev`. `[NOPANIC]`: `nr_*_par` sourced only from V6 (never bitstream length); every dequant index within its table bounds.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(ps): parameter Huffman + delta decode + clip/wrap + dequant`.

---

## Task 5: `ps/bands.rs` — 10↔20↔34 band mapping + config constants

**Files:** Create `crates/soul-audio-core/src/formats/aac/ps/bands.rs`; MODIFY `ps/mod.rs` (`pub(super) mod bands;`).

Read `ps-reference.md` §4. Maps a flattened processing band `k` (0..`NR_BANDS[is34]`) to its working-resolution parameter band via `k_to_i`, and exposes the FFmpeg-authoritative config constants the hybrid/decorrelate/mix units key off.

**Interfaces — Produces:**
```rust
pub(crate) const NR_BANDS: [usize; 2] = [71, 91];          // [is34] flattened band count
pub(crate) const NR_PAR_BANDS: [usize; 2] = [20, 34];      // working param resolution
pub(crate) const NR_IPDOPD_BANDS: [usize; 2] = [11, 17];
pub(crate) const NR_ALLPASS_BANDS: [usize; 2] = [30, 50];  // allpass applied below this
pub(crate) const DECAY_CUTOFF: [usize; 2] = [10, 32];      // decay slope starts above
pub(crate) const SHORT_DELAY_BAND: [usize; 2] = [42, 62];  // flattened ≡ QMF band 35

/// Flattened band `k` → working-resolution parameter band `i` (§4).
/// [NOPANIC]: caller guarantees `k < NR_BANDS[is34]`; result `< NR_PAR_BANDS[is34]`.
#[inline]
pub(crate) fn k_to_i(k: usize, is34: bool) -> usize;       // FF_K_TO_I_20 / FF_K_TO_I_34

/// Number of hybrid sub-bands the lowest QMF bands split into (§5), for the
/// hybrid unit to size its flattened↔QMF fold. 20-cfg: {6,2,2}; 34-cfg:
/// {12,8,4,4,4}. Exposed here so hybrid + decorrelate agree on the layout.
pub(crate) fn hybrid_split(is34: bool) -> &'static [usize];
```
**Consumes:** `tables::{FF_K_TO_I_20, FF_K_TO_I_34}` (V3).

- [ ] **Step 1: Failing tests** — V3 anchors + bounds:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn k_to_i_anchors_and_bounds() {
        // V3 anchors: ff_k_to_i_20 starts 1,0,0,1,2,3,... and every value < 20.
        assert_eq!(k_to_i(0, false), 1);
        assert_eq!(k_to_i(1, false), 0);
        assert_eq!(k_to_i(2, false), 0);
        for k in 0..NR_BANDS[0] { assert!(k_to_i(k, false) < NR_PAR_BANDS[0]); }
        for k in 0..NR_BANDS[1] { assert!(k_to_i(k, true) < NR_PAR_BANDS[1]); }
        // ff_k_to_i_34 starts 0,1,2,3,4,5,6,6,7,2,1,0,... (the low-band fold-back).
        assert_eq!(k_to_i(0, true), 0);
        assert_eq!(k_to_i(9, true), 2);
        assert_eq!(k_to_i(11, true), 0);
    }
    #[test]
    fn config_constants_match_ffmpeg() {
        assert_eq!(NR_ALLPASS_BANDS, [30, 50]);
        assert_eq!(DECAY_CUTOFF, [10, 32]);
        assert_eq!(SHORT_DELAY_BAND, [42, 62]);
        assert_eq!(hybrid_split(false), &[6, 2, 2]);
        assert_eq!(hybrid_split(true), &[12, 8, 4, 4, 4]);
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `k_to_i` (index the V3 table by `is34`), the constant arrays, `hybrid_split`. All pure, no state.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(ps): flattened↔parameter band mapping + FFmpeg-authoritative config constants`.

---

## Task 6: `ps/hybrid.rs` — hybrid analysis/synthesis, modulated complex FIR (**HARD #1, verify-first**)

**Files:** Create `crates/soul-audio-core/src/formats/aac/ps/hybrid.rs`; MODIFY `ps/mod.rs` (`pub(super) mod hybrid;`).

Read `ps-reference.md` §5. **Hardest unit #1 of 3, verify-first** (like the SBR QMF): the modulated-prototype FIR and the 6-slot group-delay alignment are easy to get subtly wrong, so validate against a known QMF-domain impulse/tone → expected sub-subband split **before** decorrelation/mixing depend on it. **Analysis** FIR-splits the lowest QMF bands of the group-delay-aligned mono matrix into hybrid sub-bands (20-cfg: bands 0/1/2 → 6/2/2; 34-cfg: bands 0..4 → 12/8/4/4/4) using the V4 modulated complex filters (Task 1), and delays the pass-through QMF bands by the same 6 slots. **Synthesis** sums each split band's sub-subbands back to plain QMF. Use **FFmpeg's direct-6** form for the 20-cfg band-0 split (Global Constraints).

**R1 reminder:** the seam hands PS the current mono `[[Cpx; 64]; 32]`; PS forms the working `[[Cpx; 64]; 38]` by prepending its retained 6-slot history (`x_hist`, owned by `PsDecoder`, Task 9). `hybrid_analysis` reads that `[38][64]`: split-band FIR output slot `n` reads input slots `n..n+12` (center `n+6`) via the persistent 12-sample per-band FIR memory; pass-through band `b` at output slot `n` = `x38[n][b]` (the 6-slot delay).

**Interfaces — Produces:**
```rust
use crate::formats::aac::ps::{N_QMF, N_SLOTS, N_SLOTS_HIST, NR_BANDS_MAX};
use crate::formats::aac::sbr::qmf::Cpx;

/// Persistent per-band 13-tap FIR memory (12 past samples × up to 5 split bands),
/// carried frame-to-frame. faad2 `hyb->buffer`.
#[derive(Clone, Copy, Default)]
pub(crate) struct HybridHistory { pub buf: [[Cpx; 12]; 5] }

/// Hybrid analysis (§5): flattened mono hybrid-domain signal from the
/// group-delay-aligned QMF matrix. `s[slot][band]`, band 0..NR_BANDS[is34].
pub(crate) fn hybrid_analysis(
    x38: &[[Cpx; N_QMF]; N_SLOTS_HIST],   // [38][64] = 6 history ++ 32 current
    is34: bool,
    hist: &mut HybridHistory,
    s: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS],
);

/// Hybrid synthesis (§5): sum each split band's sub-bands back to plain 64-band
/// QMF; pass-through bands copy through. Runs once per output channel.
pub(crate) fn hybrid_synthesis(
    x_hyb: &[[Cpx; NR_BANDS_MAX]; N_SLOTS],  // mixed channel, hybrid domain
    is34: bool,
    out: &mut [[Cpx; N_QMF]; N_SLOTS],       // [32][64]
);
```
**Consumes:** `tables` (V4 modulated complex filters `HYB_FILT_12/8/4/6`); `bands::hybrid_split` (the split layout); `sbr::qmf::Cpx`.

Flattened layout (§5): the first `Σ hybrid_split` bands are the hybrid sub-bands of the low QMF bands (in the fixed FFmpeg sub-band order), followed by the pass-through QMF bands (`3..64` for 20-cfg → 61 bands → 71 total; `5..64` for 34-cfg → 59 bands → 91 total). `hybrid_synthesis` inverts: sum sub-band group → its QMF band; copy pass-through.

- [ ] **Step 1: Failing tests** — the verify-first known-vector gate (tests may use `std`):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn mag2(c: &Cpx) -> f32 { c.re*c.re + c.im*c.im }
    #[test]
    fn analysis_zero_in_zero_out() {
        let x = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        let mut hist = HybridHistory::default();
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        hybrid_analysis(&x, true, &mut hist, &mut s);
        for slot in &s { for c in slot { assert_eq!(*c, Cpx::default()); } }
    }
    #[test]
    fn passthrough_band_delayed_by_six() {
        // A unit impulse in a HIGH (pass-through) QMF band at input slot 6+n0
        // must appear in the corresponding flattened pass-through band at output
        // slot n0 (the 6-slot group-delay alignment). Pick a band well above the
        // split region so no FIR runs on it.
        let mut x = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        let qmf_band = 40usize; let n0 = 3usize;
        x[6 + n0][qmf_band] = Cpx { re: 1.0, im: 0.0 };
        let mut hist = HybridHistory::default();
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        hybrid_analysis(&x, true, &mut hist, &mut s);
        // flattened index of qmf_band = 32 (Σ hybrid) + (qmf_band - 5).
        let flat = 32 + (qmf_band - 5);
        assert!((s[n0][flat].re - 1.0).abs() < 1e-5);
        assert_eq!(s[(n0 + 1) % N_SLOTS][flat].re == 0.0 || n0 + 1 < N_SLOTS, true);
    }
    #[test]
    fn split_band_tone_localizes_to_expected_subband() {
        // Drive a low QMF band (that gets split) with a subband-rotating phasor
        // whose intra-band frequency centres on hybrid sub-band q; that sub-band
        // must dominate the split output. Verify q against the modulated-proto
        // formula filter[q][n]=proto[n]·exp(-i·2π·(q+½)·(n-6)/bands) BEFORE
        // trusting downstream (this is the reference-vector gate).
        // ... build the phasor for band 0 (bands=12, 34-cfg), assert argmax sub-band.
    }
    #[test]
    fn analysis_then_synthesis_roundtrips_passthrough() {
        // A pass-through band round-trips analysis→synthesis to the same QMF band
        // (subband-rotating stimulus), pinning the fold/sum inverse.
        // ... 
    }
}
```
(Fill the `...` bodies from §5's modulated-prototype formula + `bands::hybrid_split`; the split-localization test is the verify-first gate — do not proceed to Task 7 until it passes.)
- [ ] **Step 2:** Run `cargo test -p soul-audio-core --lib formats::aac::ps::hybrid` → FAIL.
- [ ] **Step 3: Implement** `hybrid_analysis` (per split band: 13-tap complex-FIR MAC over the 12-sample history + current inputs, using `HYB_FILT_*`; per pass-through band: copy `x38[n]`; update `hist` with the frame's trailing 12 samples per split band) and `hybrid_synthesis` (sum each sub-band group; copy pass-through). No `libm`, no `unsafe`; every index compile-time-bounded. Keep the FFmpeg direct-6 20-cfg ordering; do not mix band orders (§5 disagreement note).
- [ ] **Step 4:** Run → PASS. **Verify-first gate:** the split-localization + passthrough-delay tests pin the FIR + alignment before Tasks 7–9 build on them.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(ps): hybrid filterbank analysis/synthesis (modulated complex FIR, verify-first)`.

---

## Task 7: `ps/decorrelate.rs` — per-band allpass + delay + transient attenuation (**HARD #2, verify-first**)

**Files:** Create `crates/soul-audio-core/src/formats/aac/ps/decorrelate.rs`; MODIFY `ps/mod.rs` (`pub(super) mod decorrelate;`).

Read `ps-reference.md` §6. **Hardest unit #2 of 3, verify-first:** validate a known allpass impulse response against the reference formula before mixing trusts `d`. For each flattened band below `NR_ALLPASS_BANDS[is34]`: a 3-link serial allpass reverberator (base delay `D=2` + fractional phase `Phi_Fract`; per-link integer delays `d(m)={3,4,5}` + fractional phase `Q_Fract_allpass`; feedback `a(m)·g(k)` with the `DECAY_CUTOFF`/`DECAY_SLOPE` decay above the cutoff). At/above that band: a pure delay (14 samples for flattened `< SHORT_DELAY_BAND`, else 1). A per-parameter-band transient (peak-decay) attenuation scales `d` down during transients (`[NOPANIC]`-guarded `G_trans` divide).

**Interfaces — Produces:**
```rust
use crate::formats::aac::ps::{N_SLOTS, NR_BANDS_MAX};
use crate::formats::aac::sbr::qmf::Cpx;

/// Persistent decorrelation state (rings + transient), carried frame-to-frame.
/// Sized to the 34-cfg worst case. `ps-reference.md` §6/§8 buffer inventory.
#[derive(Clone, Copy)]
pub(crate) struct DecorrState {
    pub delay_qmf:        [[Cpx; 64]; 14],     // base delay, QMF bands
    pub delay_subqmf:     [[Cpx; 32]; 2],      // base delay, hybrid sub-bands
    pub delay_qmf_ser:    [[[Cpx; 64]; 5]; 3], // 3 links × ≤5 samples × bands
    pub delay_subqmf_ser: [[[Cpx; 32]; 5]; 3],
    pub ring: [usize; /* per-ring write indices */ ],
    pub peak_decay: [f32; 34],                 // P_PeakDecayNrg
    pub p_prev: [f32; 34],                     // P_prev
    pub diff_prev: [f32; 34],                  // P_SmoothPeakDecayDiffNrg_prev
}
impl Default for DecorrState { /* all zeroed */ }

/// Decorrelate (§6): d[] = allpass/delay + transient reverb of s[] (the mono
/// hybrid-domain signal). Both `s` (direct) and `d` (decorrelated) feed mixing.
/// Never panics (transient divide guarded → 1.0); no Result needed.
pub(crate) fn decorrelate(
    s: &[[Cpx; NR_BANDS_MAX]; N_SLOTS],
    is34: bool,
    st: &mut DecorrState,
    d: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS],
);
```
**Consumes:** `tables` (V5 `PHI_FRACT_*`, `Q_FRACT_ALLPASS_*`, `FILTER_A`, `NUM_SAMPLE_DELAY_SER`, `DELAY_D`); `bands::{NR_ALLPASS_BANDS, DECAY_CUTOFF, SHORT_DELAY_BAND, k_to_i}`; `sbr::qmf::Cpx`.

Per-band structure (§6): `d = ( z^-D·Phi_Fract[k] · Π_m (Q_Fract[k][m]·z^-d(m) − a(m)·g(k)) / (1 − a(m)·g(k)·Q_Fract[k][m]·z^-d(m)) )·s`. Decay `g(k)=1` for `k ≤ DECAY_CUTOFF`, else `clip(1 − DECAY_SLOPE·(k−DECAY_CUTOFF),0,1)`, `DECAY_SLOPE=0.05`. Transient per param band `bk`, per slot: `P=Σ|s|²`; `P_peak=max(P_peak·ALPHA_DECAY,P)`; `P_diff=P_diff_prev+ALPHA_SMOOTH·(P_peak−P−P_diff_prev)`; `nrg=P_prev+ALPHA_SMOOTH·(P−P_prev)`; `G_trans=(GAMMA·P_diff ≤ nrg)?1.0:nrg/(GAMMA·P_diff)`; `d *= G_trans`. `[NOPANIC]` guard `P_diff==0 → G_trans=1.0`. Hybrid sub-bands use the `SubQmf` phase tables + `delay_subqmf*`; QMF bands use the `Qmf` tables + `delay_qmf*`.

- [ ] **Step 1: Failing tests** — allpass impulse + delay + transient (verify-first):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pure_delay_band_delays_impulse() {
        // A band >= NR_ALLPASS_BANDS but < SHORT_DELAY_BAND: unit impulse in s at
        // slot 0 → d has it delayed by 14 slots (ring-buffered; frame 1 sees the
        // tail). Drive two frames and check the impulse re-emerges 14 later.
        // ... 
    }
    #[test]
    fn allpass_impulse_matches_reference() {
        // A low band (< NR_ALLPASS_BANDS): unit impulse in s → the first few d
        // samples match the 3-link allpass difference equation evaluated by hand
        // from V5 (Phi_Fract, Q_Fract, filter_a). Allpass ⇒ energy preserved over
        // the tail (|d|≈|s| in aggregate). This is the verify-first gate.
        // ... 
    }
    #[test]
    fn transient_attenuates_and_never_divides_by_zero() {
        // A sudden energy spike in s → G_trans < 1 on the following slots (reverb
        // tail suppressed); a silent band (P_diff==0) → G_trans==1, no NaN/panic.
        // ... 
    }
}
```
(Fill the `...` bodies from §6's difference equation + V5 tables; the allpass-impulse test is the verify-first gate — do not proceed to Task 8 until it passes.)
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** the per-band allpass links + pure-delay branch (ring buffers indexed mod their delay length), the decay slope, and the transient peak-decay attenuation with the guarded divide. Sized to 34-cfg worst case, fixed by-value. No `libm`/`unsafe`.
- [ ] **Step 4:** Run → PASS. **Verify-first gate** before Task 8.
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(ps): decorrelation (serial allpass + pure delay + transient attenuation)`.

---

## Task 8: `ps/mix.rs` — stereo reconstruction / mixing (**HARD #3, verify-first**)

**Files:** Create `crates/soul-audio-core/src/formats/aac/ps/mix.rs`; MODIFY `ps/mod.rs` (`pub(super) mod mix;`).

Read `ps-reference.md` §7. **Hardest unit #3 of 3, verify-first:** validate a hand-computed small mixing case (IID-only, ICC `ρ=1` → the reference's own identity `L=c2·s, R=c1·s`) before the seam trusts it — and heed the faad2-vs-ISO `H12`/`H21` **naming transposition** warning (§7): the `s,d` formulas below are stated unambiguously to avoid it. Per parameter band & envelope: build the 2×2 mixing matrix (Type A `icc_mode<3`; Type B `icc_mode≥3`), optionally rotate complex with IPD/OPD (via `PD_*_SMOOTH`), linearly interpolate across the envelope's QMF slots, and form `L=H11·s+H12·d`, `R=H21·s+H22·d` (+ imaginary cross-terms when IPD/OPD active).

**Interfaces — Produces:**
```rust
use crate::formats::aac::ps::{N_SLOTS, NR_BANDS_MAX};
use crate::formats::aac::ps::{frame::PsFrameLayout, params::PsParams};
use crate::formats::aac::sbr::qmf::Cpx;

/// Persistent mixing state: per-band interpolation anchors + IPD/OPD smoothing
/// rings, carried frame-to-frame. `ps-reference.md` §7/§8.
#[derive(Clone, Copy, Default)]
pub(crate) struct MixState {
    pub h11_prev: [f32; 50], pub h12_prev: [f32; 50],
    pub h21_prev: [f32; 50], pub h22_prev: [f32; 50],
    // complex forms when IPD/OPD active (re/im per element):
    pub h_im_prev: [[f32; 4]; 50],
    pub ipd_prev: [[f32; 2]; 20], pub opd_prev: [[f32; 2]; 20],
}

/// Stereo mixing (§7): fill `xl`/`xr` (hybrid domain) from `s` (direct) and `d`
/// (decorrelated) using per-band/envelope interpolated 2×2 matrices. Updates the
/// interpolation anchors + IPD/OPD rings.
pub(crate) fn stereo_mix(
    s: &[[Cpx; NR_BANDS_MAX]; N_SLOTS],
    d: &[[Cpx; NR_BANDS_MAX]; N_SLOTS],
    params: &PsParams,
    layout: &PsFrameLayout,
    icc_mode: u8,
    st: &mut MixState,
    xl: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS],
    xr: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS],
);
```
**Consumes:** `tables` (V2.a/b/d angle tables + `PD_RE_SMOOTH`/`PD_IM_SMOOTH`); `bands::k_to_i` (flattened band → param band); `params::PsParams`; `frame::PsFrameLayout`; `sbr::qmf::Cpx`.

Type A (`icc_mode<3`, §7): from IID `c=SF_IID[·]`, `c1=√2/√(1+c²)`, `c2=c·c1`; from ICC `α=½·acos(ρ)` (via `COS_ALPHAS`/`SIN_ALPHAS`), `β=α·(c1−c2)/√2`; `H11=c2·cos(α+β)`, `H12=c2·sin(α+β)`, `H21=c1·cos(β−α)`, `H22=c1·sin(β−α)`. Sanity: `ρ=1 ⇒ α=β=0 ⇒ L=c2·s, R=c1·s`. Type B (`icc_mode≥3`): `H11=√2·cosψ·cosγ`, `H12=√2·sinψ·cosγ`, `H21=−√2·cosψ·sinγ`, `H22=√2·sinψ·sinγ` (ψ from `SINCOS_ALPHAS_B[iid]`, γ from IID,ICC). IPD/OPD (`enable_ipdopd` and `bk < nr_ipdopd_par`): 3-frame `[¼,½,¼]`-smoothed via `ipd_prev`/`opd_prev`, then multiply the real `H1x`/`H2x` into re+im using `PD_RE_SMOOTH`/`PD_IM_SMOOTH` (index `hist·8+cur`) — **no `atan2`**; `NEGATE_IPD_MASK` groups negate IPD. Interpolation per band over slots `[border[e],border[e+1])`: `L_env=border[e+1]−border[e]` (`>0` [NOPANIC §2]); `dH=(H_now−H_prev)/L_env`; step `H` per slot and apply to every sub-band of the band; carry `H_prev=H_now`.

- [ ] **Step 1: Failing tests** — the verify-first identity + Type B + IPD/OPD:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn iid_only_icc_rho1_is_pure_pan() {
        // ICC ρ=1 (icc index 0 → cos_alpha=1, sin_alpha=0) ⇒ α=β=0 ⇒
        // L = c2·s, R = c1·s (pure IID panning of the correlated mono).
        // Single-envelope, IID index gives a known c; d arbitrary (drops out).
        // Assert xl ≈ c2·s and xr ≈ c1·s on a chosen band, within 1e-4.
        // ...
    }
    #[test]
    fn matrix_interpolates_across_slots() {
        // Two envelopes with different H → the matrix ramps linearly between
        // border slots (check a mid slot equals H_prev + k·dH).
        // ...
    }
    #[test]
    fn ipdopd_makes_matrix_complex() {
        // With enable_ipdopd and a nonzero IPD/OPD index, xl/xr carry the
        // expected imaginary cross-terms (via PD_*_SMOOTH), matched vs FFmpeg.
        // ...
    }
}
```
(Fill the `...` bodies from §7's closed forms + V2 tables; the `ρ=1` identity is the verify-first gate.)
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** Type A + Type B matrix construction, the IPD/OPD complex rotation (`PD_*_SMOOTH`, 3-frame smoothing rings), the per-band per-slot interpolation, and `L=H11·s+H12·d`/`R=H21·s+H22·d` (+ imaginary cross-terms). Index params by `k_to_i(k,is34)`. No `libm` (all angles from tables). No `unsafe`.
- [ ] **Step 4:** Run → PASS. **Verify-first gate.**
- [ ] **Step 5:** thumbv7em + clippy + arch-tests clean.
- [ ] **Step 6: Commit** `feat(ps): stereo mixing (Type A/B matrices + IPD/OPD + slot interpolation)`.

---

## Task 9: `ps/mod.rs` `PsDecoder::{parse_frame, apply}` + SBR seam wiring + channel promotion

**Files:** MODIFY `crates/soul-audio-core/src/formats/aac/ps/mod.rs` (finish `PsDecoder`: DSP scratch + state, `parse_frame`, `apply`, `ps_data_available`; drop the temporary `#![allow(dead_code)]`); MODIFY `crates/soul-audio-core/src/formats/aac/sbr/bitstream.rs` (move the trailing `bs_extension` loop into `drive`, rewrite it conformantly with PS dispatch — R2/R3); MODIFY `crates/soul-audio-core/src/formats/aac/sbr/mod.rs` (`SbrState.ps` field; split `drive` into parse → extension-loop → PS-aware synthesis; `SbrOutput.channels`); MODIFY `crates/soul-audio-core/src/formats/aac/mod.rs` (channel 1→2 promotion via `sbr_out.channels`); MODIFY `crates/soul-audio-core/src/formats/aac/sbr/qmf.rs` only if a visibility promotion is needed (R5).

Read `ps-reference.md` §8 (assembly + seam) + the **Spec-vs-code reconciliations** section of this plan (R1–R6) — they are the whole of this task. This closes the loop: drive units 5–7 (hybrid analysis → decorrelate → mix → hybrid synthesis) behind `apply`, wire `parse_frame` into the (rewritten, reordered) SBR extension path, and promote the channel count. This is the heaviest task (the PS analogue of SBR Task 11); keep the non-PS SBR path **bit-identical** (the SBR conformance golden is a gate here).

**Interfaces — Produces (`ps/mod.rs`, finishing `PsDecoder`):**
```rust
use crate::formats::aac::sbr::qmf::Cpx;
use crate::formats::aac::bits::BitReader;
use crate::decoder::DecodeError;
// PsDecoder now also owns (added to the Task-2 skeleton):
//   x_hist: [[Cpx; N_QMF]; HISTORY],                 // R1: previous frame's last 6 assembled slots
//   hyb: hybrid::HybridHistory,
//   dec: decorrelate::DecorrState,
//   mix: mix::MixState,
//   // per-frame scratch (reused, not reallocated):
//   x38: [[Cpx; N_QMF]; N_SLOTS_HIST],               // history ++ current
//   s:   [[Cpx; NR_BANDS_MAX]; N_SLOTS],             // direct hybrid-domain
//   d:   [[Cpx; NR_BANDS_MAX]; N_SLOTS],             // decorrelated hybrid-domain
//   xl:  [[Cpx; NR_BANDS_MAX]; N_SLOTS],             // mixed left (hybrid domain)
//   xr:  [[Cpx; NR_BANDS_MAX]; N_SLOTS],             // mixed right (hybrid domain)

impl PsDecoder {
    /// Parse-time seam (from the SBR extension loop, R2/R3). Delegates to
    /// `data::ps_data`; updates current/next-frame parameter state; returns the
    /// exact bit count consumed. On malformed input returns `Err` → the caller
    /// disables PS for this frame (mono passthrough).
    pub(crate) fn parse_frame(&mut self, bits: &mut BitReader, end_bit: usize)
        -> Result<u32, DecodeError> { data::ps_data(self, bits, end_bit) }

    /// True once a PS header has been seen (`ps_data_available`).
    pub(crate) fn is_active(&self) -> bool { self.ps_data_available }

    /// QMF-domain seam (R1/R4): consume the current assembled MONO matrix,
    /// produce two matrices for SBR to synthesize twice. Uses this frame's
    /// parameters (or the last header's when `enable_ps_header==0`). Internally
    /// prepends the retained 6-slot history to form the [38][64] working matrix,
    /// runs §8 order (hybrid analysis → decorrelate → mix → hybrid synthesis),
    /// then updates the history. `Err` → caller falls back to mono passthrough.
    pub(crate) fn apply(
        &mut self,
        x_mono: &[[Cpx; N_QMF]; N_SLOTS],
        out_left:  &mut [[Cpx; N_QMF]; N_SLOTS],
        out_right: &mut [[Cpx; N_QMF]; N_SLOTS],
    ) -> Result<(), DecodeError>;
}
```
`apply` body (§8): build `x38 = x_hist ++ x_mono`; `hybrid::hybrid_analysis(&x38, is34, &mut hyb, &mut s)`; `decorrelate::decorrelate(&s, is34, &mut dec, &mut d)`; `mix::stereo_mix(&s, &d, &params, &frame, icc_mode, &mut mix, &mut xl, &mut xr)`; `hybrid::hybrid_synthesis(&xl, is34, out_left)`; `hybrid::hybrid_synthesis(&xr, is34, out_right)`; then `x_hist = x_mono[N_SLOTS-HISTORY..]` (last 6 slots) for next frame. All buffers are reused `PsDecoder` fields (no alloc).

**SBR-side wiring changes:**
- `sbr/mod.rs` `SbrState`: add `ps: crate::formats::aac::ps::PsDecoder` (always present; `Default` = inactive). Add `pub(super) channels: usize` to `SbrOutput` (default = the input `n_ch`).
- `sbr/mod.rs` `drive` (**restructure per R3**): keep the top `parse_sbr_data` + dequant (`parsed`) as-is. **Then**, before the per-channel DSP loop, call the (moved) extension-loop parser (below), which may set `state.ps` active. **Then** run the per-channel DSP:
  - PS path — only when `state.ps.is_active()` **and** `n_ch == 1` (mono core): run ch[0] analysis→HF→assemble to the mono `x_out` exactly as today (via `run_channel_hf`/`assemble_upsample` producing `x_out`), then instead of one synthesis do:
    ```rust
    let mut xl = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
    let mut xr = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
    if state.ps.apply(&x_out, &mut xl, &mut xr).is_ok() {
        let (c0, c1) = state.ch.split_at_mut(1);       // ch[0] left, ch[1] right
        qmf_synthesis(&xl, &mut c0[0].qmf_syn, &mut out.pcm[0]);
        qmf_synthesis(&xr, &mut c1[0].qmf_syn, &mut out.pcm[1]);
        out.channels = 2;                              // R6 promotion
    } else {
        qmf_synthesis(&x_out, &mut state.ch[0].qmf_syn, &mut out.pcm[0]); // PS failed → mono
        out.channels = 1;
    }
    ```
  - non-PS path — the existing per-channel loop, unchanged (`out.channels = n_ch`).
  - `out.written = 2048;` as today.
- `sbr/bitstream.rs` (**R2/R3**): remove the trailing `bs_extension` loop from `sbr_extension_data` (keep only the final pad-to-`end_bit`), and add a `pub(super) fn parse_extensions(payload, state) -> ()` that `drive` calls after `parse_sbr_data`. `parse_extensions` implements the **conformant** structure (`ps-reference.md` §1): `bs_extended_data = clamped_read(1)`; if 1, `cnt = read(4); if cnt==15 { cnt += read(8) }`; `nr_bits_left = 8*cnt` (clamped to the payload); `while nr_bits_left > 7 { id = read(2); nr_bits_left -= 2; match id { EXTENSION_ID_PS => { let ext_end = min(end_bit, bit_pos + nr_bits_left); match state.ps.parse_frame(bits, ext_end) { Ok(used) => nr_bits_left -= used, Err(_) => break } }, _ => break } }`. Every read via `clamped_read`; any overrun → stop (never panic). Keep the existing `EXTENSION_ID_PS` const (drop its `#[allow(dead_code)]`).
- `formats/aac/mod.rs` `decode_rdb` (**R6**): after the SBR call returns `applied`, use `let out_ch = if applied { sbr_out.channels } else { n };` for the interleave (`pcm[i*out_ch + ch]`) and return the produced channel count so the caller can promote. In `AacDecoder`: add `ps_confirmed: bool` (or reuse the returned count); the first frame where `out_ch == 2 > self.channels`, set `self.channels = 2`. `stream_info().channels` then reports 2. `PCM_CAP` unchanged (`2048*2*2` already covers `2048*2` interleaved).

- [ ] **Step 1: Failing test** — an `apply` seam smoke test + the SBR golden as a guard:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::sbr::qmf::Cpx;
    #[test]
    fn apply_produces_distinct_stereo_from_active_ps() {
        // A PsDecoder made active with a hand-set header + a nonzero IID on some
        // band, fed a nonzero mono matrix, must fill out_left != out_right
        // (PS created a stereo image), and must not panic.
        let mut ps = PsDecoder::default();
        // ... set ps active with a small layout/params (iid_c != 1 on a band),
        //     is34=false; fill x_mono with a nonzero low band.
        let x = /* nonzero mono [[Cpx;64];32] */;
        let mut l = [[Cpx::default(); N_QMF]; N_SLOTS];
        let mut r = [[Cpx::default(); N_QMF]; N_SLOTS];
        assert!(ps.apply(&x, &mut l, &mut r).is_ok());
        let differ = l.iter().zip(r.iter()).any(|(a,b)| a.iter().zip(b).any(|(p,q)| p != q));
        assert!(differ, "PS must produce a genuine stereo image (L != R)");
    }
    #[test]
    fn inactive_ps_is_untouched() {
        // Default PsDecoder is inactive → is_active() false (drive keeps mono).
        assert!(!PsDecoder::default().is_active());
    }
}
```
(Plus: after wiring, the **existing** `open_decodes_he_aac_sbr_matching_ffmpeg` SBR golden in `tests/decode.rs` must still pass unchanged — the non-PS path is bit-identical. Run it as a Step-4 gate.)
- [ ] **Step 2:** Run → FAIL (`apply` unimplemented).
- [ ] **Step 3: Implement** the `apply` pipeline, the `SbrState.ps`/`SbrOutput.channels` fields, the `drive` restructure (parse → `parse_extensions` → PS-aware synthesis), the conformant `parse_extensions` loop with PS dispatch, and the `decode_rdb` channel promotion. Resolve any `qmf`/`qmf_synthesis` visibility (R5) by promotion if needed. Keep the LC-only and non-PS-SBR paths byte-identical.
- [ ] **Step 4:** Run → PASS. **Gate:** the SBR conformance golden (`open_decodes_he_aac_sbr_matching_ffmpeg`, both `sbr.m4a`/`sbr.aac`) and `sbr_output_is_2x_and_non_silent` must still pass (non-PS unchanged). Full `cargo test -p soul-audio-core`.
- [ ] **Step 5:** thumbv7em + `cargo clippy --workspace --all-targets -- -D warnings` + arch-tests clean. Re-measure decode stack headroom (the `AnyDecoder`/`decode_all` big-stack thread) — PS adds ~100 KB (design §3.3); confirm the 16 MiB test-thread stack still suffices and note it.
- [ ] **Step 6: Commit** `feat(ps): PsDecoder apply/parse_frame + SBR seam wiring + channel 1→2 promotion`.

> The stale SBR test `bs_extension_loop_length_skips_bogus_sub_extension` (bitstream.rs) exercises the old placeholder framing. Its final-position assertion still holds via the pad-to-`end_bit`, but its comment/intent is now obsolete — update the comment (or replace it with a conformant-loop + PS-dispatch test using an active `SbrState`).

---

## Task 10: Local-only conformance + structural gates, committed robustness fuzz, docs

**Files:** MODIFY `crates/soul-audio-core/tests/decode.rs` (finish the Task-0 scaffold asserts), `crates/soul-audio-core/tests/robustness.rs` (committed synthetic PS seed + local fixture seed when present), `crates/soul-audio-core/testdata/README.md` (finalize the measured numbers).

Read `ps-reference.md` §5 (design testing plan) + §8 `[NOPANIC]` checklist + the **Testing strategy** section of this plan. Now that PS decodes, fill the Task-0 scaffold with the calibrated per-channel SNR + structural asserts (local-only, skip-if-absent), add the committed robustness coverage, and record the measured numbers.

- [ ] **Step 1: Finish the conformance test** (`ps_local_matches_ffmpeg_per_channel`, replacing the Task-0 `NOTE`): decode the fixture, find one shared priming offset via `best_align_segsnr(&ours, &refr, 2)`, then de-interleave L and R and score **each channel independently** at that offset; assert **both** clear `PS_SNR_BAR` (a `const` calibrated from the measured value with margin, documented in a doc-comment — same discipline as the AAC-LC 55.3→45 dB and SBR ~104→90 dB gates). Also assert `dec.stream_info().channels == 2` after decoding (R6 promotion). Skip-if-absent stays.
```rust
#[test]
fn ps_local_matches_ffmpeg_per_channel() {
    let Some((data, refr)) = ps_local_fixture() else {
        eprintln!("skip: ps-local.* fixture absent (license-restricted, local-only)"); return;
    };
    let mut dec = open(SliceSource::new(&data)).expect("open HE-AACv2/PS");
    let ours = decode_all(&data);
    assert_eq!(dec_channels_after_decode(&mut dec), 2); // stream_info reports 2 (R6)
    let (off, _) = best_align_segsnr(&ours, &refr, 2);
    for ch in 0..2 {
        let o = deinterleave(&ours, 2, ch);
        let r = deinterleave(&refr, 2, ch);
        let (_, snr) = best_align_segsnr(&interleave1(&o), &interleave1(&r), 1); // score at shared off
        assert!(snr > PS_SNR_BAR, "PS ch{ch} SNR vs ffmpeg too low: {snr:.1} dB (off {off})");
    }
}
```
(Adjust the per-channel scoring to reuse the shared `off` — e.g. slice `o[off..]` against `r` and compute segmental SNR directly; the helper shape is illustrative. `PS_SNR_BAR`: set below the measured value; SBR is already near-bit-exact vs ffmpeg here, PS adds table-driven f32 DSP, so expect a high bar — measure, then subtract margin.)
- [ ] **Step 2: Finish the structural test** (`ps_local_output_is_stereo_and_channels_differ`): assert output `channels == 2`; per-channel length within a frame (`4096`) of the 2× reference; non-silent; within i16 range; **and the two channels genuinely differ** — `RMS(L−R)/RMS(L)` above a small threshold (e.g. `> 0.01`), catching a mono-duplicated / PS-silently-disabled bug that every other check would pass. Skip-if-absent stays.
- [ ] **Step 3: Committed robustness fuzz** in `tests/robustness.rs`:
  - Add a **committed synthetic** seed — a hand-built minimal HE-AACv2-shaped ADTS/mp4 buffer whose fill element carries an `EXTENSION_ID_PS` sub-extension with a valid tiny `ps_data` (or reuse the smallest committed SBR seed and document that PS-field fuzzing is driven by the bit-flips over it). Purpose: the sweep's bit-flips/truncations over the PS payload exercise every §8 `[NOPANIC]` field (`iid_mode`/`icc_mode`/`frame_class`/`num_env_idx`/`border_position`/Huffman deltas/`ps_extension_size`) → `Err`/disable-PS, never panic/OOB. If a fully synthetic PS bitstream is impractical to hand-author, add a **helper that injects a synthetic PS sub-extension into the committed `sbr.aac`** at test build time (pure Rust, in-test) and fuzz that — keep it committed and deterministic.
  - Add the **local fixture** as a seed **only when present**: `if let Ok(b) = std::fs::read(ps_local_path()) { seeds.push(("ps-local", b)); }` (never `include_bytes!`). Confirm `decoders_never_panic_or_hang_on_malformed_input` passes.
- [ ] **Step 4: Docs.** Finalize `testdata/README.md`: the measured per-channel SNR + chosen `PS_SNR_BAR`, the structural `RMS(L−R)/RMS(L)` observed value, and restate that PS conformance is **local-only / skip-if-absent** (license-restricted, never committed) while committed coverage is the per-unit synthetic-input DSP tests + the robustness sweep.
- [ ] **Step 5:** Run `cargo test -p soul-audio-core` (locally: PS gates run; CI: skip), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, thumbv7em build (`--no-default-features`), `cargo test -p architecture-tests`, and `cargo xtask verify` — all clean. Drop the temporary `#![allow(dead_code)]` from `ps/mod.rs` / `ps/tables.rs` (every table now consumed).
- [ ] **Step 6: Commit** `test(ps): local-only conformance + structural gates + robustness fuzz + docs`.

---

## Self-review notes (for the executor)

- **Spec §4 unit → task coverage.** Reference §1 → Task 2 (`ps/data.rs`); §2 → Task 3 (`ps/frame.rs`); §3 → Task 4 (`ps/params.rs`); §4 → Task 5 (`ps/bands.rs`); §5 → Task 6 (`ps/hybrid.rs`, hard #1); §6 → Task 7 (`ps/decorrelate.rs`, hard #2); §7 → Task 8 (`ps/mix.rs`, hard #3); §8 → Task 9 (`ps/mod.rs` assembly + seam). Const tables (§3.1 `[NOLIBM]` inventory) → Task 1. Testing (§5) → Task 0 scaffold + Task 10 gates + the per-unit tests in Tasks 1–8. Every numbered reference section has a home; the 3 hard DSP units (§5/§6/§7) are each verify-first with a known-vector gate before the next depends on them.
- **Type consistency (Produces in Task N = Consumes in Task M).** `Cpx` = `crate::formats::aac::sbr::qmf::Cpx` (real, `pub(crate)`), used identically in Tasks 6–9 and at the SBR call site. Dimension constants (`N_QMF=64`, `N_SLOTS=32`, `N_SLOTS_HIST=38`, `HISTORY=6`, `NR_BANDS_MAX=91`) defined once in `ps/mod.rs` (Task 1), consumed by Tasks 6–9. `PsFrameLayout` (Task 3) → consumed by Task 4 (`is34`/`iid_quant`/band counts), Task 8 (`border_position`/band counts), Task 9. `PsParams` (Task 4) → Task 8, Task 9. `HybridHistory`/`DecorrState`/`MixState` (Tasks 6/7/8) → owned by `PsDecoder` (Task 9). The seam signatures match the **real** SBR code: `apply(x_mono: &[[Cpx;64];32], out_left/right: &mut [[Cpx;64];32])` is exactly `[[Cpx; N_SYN]; TIME_SLOTS_RATE]` — the shape `sbr/mod.rs::drive` holds as `x_out` right before `qmf_synthesis`. `parse_frame(bits: &mut BitReader, end_bit: usize) -> Result<u32, DecodeError>` returns the bit count the rewritten `sbr/bitstream.rs` extension loop consumes.
- **Exact SBR seam interface (the real types).** Parse-time: `sbr/bitstream.rs::parse_extensions` (new; runs inside `sbr/mod.rs::drive` after `parse_sbr_data`, before synthesis) dispatches `bs_extension_id == EXTENSION_ID_PS (2)` to `state.ps.parse_frame(&mut BitReader, end_bit) -> Result<u32, DecodeError>`. QMF-domain: `sbr/mod.rs::drive`, when `state.ps.is_active() && n_ch == 1`, calls `state.ps.apply(&x_out, &mut xl, &mut xr) -> Result<(), DecodeError>` on the assembled mono `x_out: [[Cpx; 64]; 32]`, then `qmf_synthesis(&xl, &mut state.ch[0].qmf_syn, &mut out.pcm[0])` and `qmf_synthesis(&xr, &mut state.ch[1].qmf_syn, &mut out.pcm[1])` (SBR's own two persistent `QmfSynthesisState`s; `ch[1]` is free for a mono core). `SbrOutput.channels` (new) = 2 → `decode_rdb` interleaves 2 channels and promotes `AacDecoder.channels` 1→2.
- **Spec-vs-code reconciliations made (R1–R6, in the dedicated section).** R1: the seam type is the real `[[Cpx;64];32]`, **not** `X[38][64]`; the 6-slot group-delay history is PS-internal (`x_hist`), because SBR's `assemble` produces only the current 32 slots. R2: the SBR trailing `bs_extension` loop is a Phase-2 **placeholder** (invented 8-bit per-sub-extension length) and must be rewritten to the conformant `bs_extended_data`/`cnt`/`id`+`sbr_extension()` structure. R3: PS param parse must precede the QMF-domain synthesis, so the extension-loop parse moves **into** `drive` (after `parse_sbr_data`, before synthesis) — the current code parses it after `drive` has already synthesized. R4: `qmf_synthesis` is `pub(super)` in `sbr`, unreachable from `ps`; `apply` returns matrices and the SBR driver synthesizes twice using `ch[0]`/`ch[1]` synthesis states. R5: reuse `Cpx` (already `pub(crate)`); PS owns its dimension constants; promote `qmf`/`qmf_synthesis` visibility only if a path won't resolve. R6: `AacDecoder.channels` promotes 1→2 on first successful `apply`, plumbed via `SbrOutput.channels`.
- **Placeholder scan.** Every task has concrete Create/Modify paths, real Rust interface signatures referencing the real `Cpx`/seam types, and runnable TDD steps with expected FAIL/PASS. The DSP-heavy known-vector test bodies (Tasks 6–8) are sketched with explicit `...`-to-fill instructions pointing at the exact reference formula (§5/§6/§7) + the V-table anchors — deliberately not hand-fabricated numeric expectations (they must be computed from the reference, not guessed), matching how the SBR plan left QMF/HF-gen unit-vector bodies to be filled from the reference. Big tables are "fetch from `ps-reference-verbatim.md` §V + faad2/FFmpeg, verify size+anchor", never inlined.
- **Testing decision baked in (user).** No committed conformance golden: the license-restricted `ps-local.*` fixture is gitignored and read via `std::fs::read` at runtime with skip-if-absent (Task 0 scaffold, Task 10 asserts) — a CI no-op, a local gate. Committed = per-unit synthetic-input DSP tests (Tasks 1–8) + the robustness sweep's synthetic PS seed (Task 10). The structural gate also needs a PS bitstream, so it too is local-only.
- **Tasks still large for one review gate.** **Task 9** (apply pipeline + the `drive` restructure + conformant extension loop + channel promotion, spanning `ps/mod.rs` + three `sbr/*` files + `formats/aac/mod.rs`) is the heaviest — the PS analogue of SBR Task 11 — and its correctness only shows end-to-end at the local conformance gate; keep the non-PS path bit-identical and use the SBR golden as its guard. **Tasks 6–8** (the three hard DSP units) are each verify-first and independently gated. If a reviewer wants finer gates, Task 9 splits at (a) `apply` + `ps/mod.rs` DSP orchestration vs (b) the `sbr/*` + `decode_rdb` wiring.
- **Firmware invariants.** Every task's commit passes the thumbv7em `--no-default-features` build + `cargo test -p architecture-tests`; every PS bitstream field is `[NOPANIC]`-checked → `Err`/disable-PS (never panic/OOB); no `libm`/no new dependency (all trig/allpass/phase/dequant are `xtask`-generated `const` tables with a `check-ps-tables` gate in `cargo xtask verify`); commit messages carry no AI trailers.

