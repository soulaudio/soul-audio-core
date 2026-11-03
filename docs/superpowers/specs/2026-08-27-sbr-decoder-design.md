# SBR decoder (Phase 2 of HE-AACv2) — design

**Date:** 2026-08-27
**Status:** Approved (design review)
**Depends on:** the AAC-LC decoder (`docs/superpowers/specs/2026-08-26-aac-lc-decoder-design.md`,
`crates/soul-audio-core/src/formats/aac/{mod,syntax,filterbank,tables,bits,huffman}.rs`),
the `Decoder`/`AnyDecoder` seam, and the `xtask` const-table generator pattern
(`xtask/src/filterbank_tables.rs`, `resample_tables.rs`).
**Reference:** `.superpowers/sdd/sbr-reference.md` (10 numbered sections, cited
as `§N` below) + `.superpowers/sdd/sbr-reference-verbatim.md` (the QMF
prototype filter, noise table, and Huffman trees — referenced, not inlined).

## 0. Phasing

Per the AAC-LC spec's §0, HE-AACv2 is three sequential phases:

1. **AAC-LC** (done) — the core codec.
2. **SBR** (this spec) — Spectral Band Replication → HE-AAC; wraps the LC
   output with a QMF filterbank + high-frequency reconstruction.
3. **PS** (later) — Parametric Stereo → HE-AACv2; operates in SBR's QMF
   domain. SBR must expose a seam PS can slot into (§2 below); it does not
   implement PS.

PS depends on SBR and is out of scope here.

## 1. Goal

Extend the existing `no_std`, no-alloc AAC-LC decoder in `soul-audio-core`
with **SBR** so it decodes HE-AAC (object type 5, and implicit backward-
compatible signalling) end to end: LC core → SBR high-band reconstruction →
PCM at **2× the core sample rate**. Both **explicit** (AudioObjectType 5, ASC
hierarchical signalling) and **implicit** (SBR discovered from the
`fill_element` at decode time, common in ADTS) signalling are supported. Mono
and stereo, including the coupled-channel (`bs_coupling`) stereo mode.

**Dual-rate (2×) SBR is the only mode implemented in v1.** Downsampled/
single-rate (1×) SBR (`§7.3`) is explicitly **out of scope** — see §5/§6.

**Complex QMF** (the high-quality analysis/synthesis filterbank in `§7`, not
the low-power/real-valued variant) is used throughout, because Phase 3 (PS)
requires the complex subband representation to do its decorrelation and
rotation math. This is a structural decision, not an optimization deferred —
implementing the real/low-power QMF now would mean re-deriving the complex
path for PS later.

## 2. Seam & containers

### 2.1 Signalling — where SBR is discovered

- **Explicit (AOT 5):** `AudioSpecificConfig.audioObjectType == 5`, or a
  `syncExtensionType == 0x2B7` trailer after the core `GASpecificConfig` with
  `extensionAudioObjectType == 5` and `sbrPresentFlag == 1` (`§1`). The ASC's
  `extensionSamplingFrequencyIndex` gives the *output* (2×) rate up front.
- **Implicit (ADTS-common):** no ASC signal; the decoder discovers SBR the
  first time it sees `EXT_SBR_DATA`/`EXT_SBR_DATA_CRC` in a `fill_element`
  (`§1`). Output rate becomes 2× the ADTS core rate from that point on.
- Either way, **SBR presence is a per-stream runtime fact, not a static type**:
  the decoder starts in LC-only (upsample-passthrough, `kx=32, M=0`) mode and
  promotes to full SBR reconstruction once a `sbr_header()` (`§2`) has been
  seen. `AacDecoder` therefore always reports the *2× rate* in `stream_info()`
  once SBR is confirmed present (explicit signal, or after the first
  `EXT_SBR_DATA` fill element) — before that, LC rate. A stream that never
  carries `EXT_SBR_DATA` (plain AAC-LC) is decoded exactly as Phase 1 already
  does; this is the backward-compatibility path and requires no separate code
  path — SBR is additive to the existing `decode_rdb`.

### 2.2 Where SBR plugs into the existing pipeline

The AAC-LC `decode_rdb` (`formats/aac/mod.rs`) already parses the
`raw_data_block`'s element loop down to `ID_FIL`, where it currently calls
`decode_fil` to **skip** the fill element's bytes unconditionally
(`syntax.rs::decode_fil`). SBR changes that one seam:

- `decode_fil` becomes SBR-aware: it inspects `extension_type` (`§1`) and,
  for `EXT_SBR_DATA`/`EXT_SBR_DATA_CRC`, hands the remaining bits to the new
  `sbr::sbr_extension_data()` instead of blindly skipping them. Any other
  `extension_type` (including plain `EXT_FILL`) is still skipped exactly as
  today.
- The LC core decode (syntax → dequant → stereo → TNS → IMDCT) is
  **unchanged** and still runs first, per channel, producing the existing
  1024-sample/channel time-domain output. SBR then runs as an **additional
  stage after IMDCT**: QMF-analyze that LC PCM, reconstruct the high band,
  QMF-synthesize to 2048 samples/channel. This matches `§10`'s "LC core must
  decode first" note and keeps the LC path itself untouched (no risk to the
  already-shipped, tested Phase 1 code).
- `id_aac` (SCE vs CPE) for the fill element's SBR payload is "whichever
  element preceded this fill element" (`§1`) — `decode_rdb`'s element loop
  already knows this (it just decoded that SCE/CPE), so it threads it through.

### 2.3 `AacDecoder` changes

- New per-instance `sbr: Option<SbrState>` (or an always-present `SbrState`
  that starts in "not seen a header yet" mode — TBD at plan time; either way,
  no heap, fixed size). Promoted to active on the first valid `sbr_header()`.
- `stream_info().sample_rate` reflects the SBR-confirmed rate (§2.1). Channel
  count is unchanged (SBR does not add channels; PS will, in Phase 3, but
  that is invisible to Phase 2).
- Output buffer sizing: `decode_rdb` currently emits at most `1024 * 2`
  interleaved samples per LC frame; with SBR active this becomes
  `2048 * 2` (`PCM_CAP` and any fixed-size scratch grow accordingly — see the
  RAM budget in §3.1).
- `AnyDecoder::Aac` is unaffected structurally (still one variant) but grows
  in byte size because `AacDecoder` now carries SBR's per-channel state
  in-line (no allocation). See Notes/risks (§6) re: stack-size impact,
  mirroring the existing multichannel-FLAC precedent.

### 2.4 The PS seam (critical — Phase 3 depends on this)

SBR's QMF stage is architected so that Parametric Stereo (Phase 3) can be
inserted at **one** point — the boundary between "produce the fully-assembled
mono subband matrix" and "synthesize it" — without restructuring SBR. The seam
is the **assembled, full-band** matrix (low band from analysis + high band from
HF generation/adjustment), NOT the raw analysis output: the core LC signal is
band-limited, so QMF analysis only ever populates the low bands, and the high
bands do not exist as real content until HF generation (§8–9) synthesizes them.
PS needs the full band range, so it must consume the matrix **after** the
low/high splice (§10 step 5):

```
core LC PCM (mono, 1024 samples)
        │
        ▼
  QMF ANALYSIS (complex, 32-band, §7.1)  ──►  X_low (low bands only)
        │
        ▼
  HF GENERATION + HF ADJUSTMENT (§8–9)   ──►  X_high (reconstructed high band)
        │
        ▼
  ASSEMBLE (§10 step 5): splice X_low + X_high
        │
        ▼
  ╔════════════════════════════════════════════════════╗
  ║  mono complex 64-band QMF subband matrix            ║  ← THE PS SEAM
  ║  X[38][64] complex: 32 time slots/frame + 6 slots   ║
  ║  of history (the shape the PS reference specifies)  ║
  ╚════════════════════════════════════════════════════╝
        │                                    │
        │ (Phase 2: mono, straight through)  │ (Phase 3: PS decorrelates +
        ▼                                    │  upmixes here, one → two)
  QMF SYNTHESIS ×1 (this channel, §7.2)      ▼
        │                          two L/R 64-band × 32-slot
        │                          complex matrices
        │                                    │
        ▼                                    ▼
  2× LC-rate PCM                    QMF SYNTHESIS ×2 (L, R)
                                     (Phase 3 wires this)
                                             │
                                             ▼
                                     2× LC-rate stereo PCM
```

For an explicit **stereo** (CPE) HE-AAC stream (no PS), each channel runs its
own independent analysis → HF-gen → HF-adj → assemble → synthesis chain (the
chains never cross; `§10`'s `bs_coupling` governs only their *envelope/noise
data*, not the QMF matrices). HE-AACv2 is always a **mono** core, so the PS
fork only ever applies to the single-channel path.

Concretely, **Phase 2 exposes** (as a real Rust interface inside
`formats/aac/sbr/`, not a trait object — a function/struct boundary, not
dynamic dispatch), with the seam mediated by `sbr/assemble.rs`:

- `qmf_analysis(core_pcm: &[f32; 1024], state: &mut QmfAnalysisState)` — fills
  `X_low` (low bands). HF generation/adjustment (§8–9) then fill `X_high`.
- `sbr/assemble.rs` splices them into the full mono `X[38][64]` matrix (§10
  step 5). **This assembled matrix is the seam.** In Phase 2, `assemble.rs`
  passes it straight to `qmf_synthesis` once. In Phase 3, `assemble.rs` hands
  it to `ps::apply(&x) -> (X_left, X_right)` and calls `qmf_synthesis` **twice**.
- `qmf_synthesis(x: &X[..][64] complex, state: &mut QmfSynthesisState) -> [f32; 2048]` —
  takes **one** assembled complex subband matrix and produces one channel's
  time-domain output.

The `X[38][64]` buffer is always built in full even for a mono stream (the
6-slot history is what PS's 13-tap hybrid FIR needs — §2 of the PS spec), so
Phase 3 has a uniform interface to hook regardless of channel count. **Phase 2
itself implements no PS decorrelation** — mono stays mono. The concrete complex
element type (`Complex { re: f32, im: f32 }`, likely `formats/aac/filterbank.rs`'s
`Cpx` promoted to `pub(crate)`) is defined by `sbr/qmf.rs` and reused by PS; the
SBR and PS plans agree on it when `qmf.rs` lands.

**Parse-side hook — the `bs_extension_id` loop:** the SBR side-data itself
(`sbr_data`) is followed, inside `sbr_extension_data()`, by a length-prefixed
`bs_extension` region carrying zero or more `bs_extension_id`-tagged
sub-extensions (§1 / SBR reference §1). **Phase 2 walks this loop** (it does not
treat the region as opaque padding): each sub-extension is length-skipped in
Phase 2, but the loop structure is parsed so Phase 3 can dispatch its
`EXTENSION_ID_PS == 2` sub-extension to `ps::parse_frame(...)` here, trusting
the PS parser's returned bit count for the loop's own position bookkeeping.
`sbr/bitstream.rs` (§4) owns this loop.

## 3. `no_std` / no-alloc / firmware constraints

### 3.1 No new dependency, no `libm`, no `unsafe`

Same crate-level invariants as LC (`#![no_std]` outside `test`,
`#![forbid(unsafe_code)]`, the `architecture-tests` allowlist gating
dependencies to `embedded-io`-only). SBR adds **more** transcendental math
than LC did, all of which must be precomputed:

| Transcendental | Where (reference §) | Table strategy |
|---|---|---|
| QMF analysis modulation `cos/sin(π(k+½)(2n−95)/64)` | `§7.1` | const table, `xtask gen-sbr-qmf-tables` |
| QMF synthesis modulation `cos/sin(π(k+½)(2n−63)/128)` | `§7.2` | const table, same generator |
| `log2`/power (`q^i` geometric spacing) for the master frequency table | `§3.3` | const `log2Table`/`logTable` + Taylor exp, faad2-style, or an equivalent closed-form table — same generator |
| `√2`, `√` in gain/noise/sine level calc | `§6.4`, `§9.3` | `√2` is a literal constant; general `√x` needs a no-libm sqrt (Newton-Raphson seeded by a bit-hack, matching the pattern any other no-libm sqrt in this crate would use) — a **function**, not a table (domain is data-dependent), documented and unit-tested against known values |
| Noise sequence table `V[512]` | `§9.4`, verbatim §2 | const table (already just numeric constants — no math to precompute, straight transcription) |

All of these follow the **exact precedent** `filterbank_tables.rs` /
`resample_tables.rs` set: a host-side `xtask` subcommand
(`cargo xtask gen-sbr-tables`, generating
`formats/aac/sbr/qmf_tables.rs` + `formats/aac/sbr/freq_tables_math.rs` or
similar) computes the tables with `f64`/host trig, writes them as `const`
arrays, and a `check-sbr-tables` gate (wired into `xtask verify`, like
`check-filterbank-tables`) fails CI if the checked-in tables don't match a
fresh regeneration. A runtime self-check test (mirroring
`fft_matches_naive_dft`/`windows_satisfy_princen_bradley`) re-derives a
closed-form property of the QMF tables (e.g. unit-modulus of the modulation
exponential, or a small-N direct-sum cross-check) so the tables can't rot
silently. Given `§7`/`§8`'s explicit warning that these exact offsets
(`2n−95`, `2n−63`) are easy to get subtly wrong, the QMF tables get a
**reference-vector validation** before anything downstream trusts them (see
§4's unit list — QMF is a Task-0-adjacent, verify-first unit, same as the
LC IMDCT was).

### 3.2 Fixed by-value buffers — per-channel RAM budget

No heap; every SBR buffer is a fixed-size array sized to the reference's
worst case (`§0`). This is materially larger than LC's per-channel state.
Approximate per-channel budget, from `§0`'s buffer list (`f32`, 4 bytes,
unless noted):

| Buffer | Shape | Approx. size |
|---|---|---|
| Analysis QMF delay `x` | 320 reals | 1.3 KB |
| Synthesis QMF delay `v` | 1280 reals | 5.1 KB |
| `X_low` | `32×40×2` complex | 10.2 KB |
| `X_high` | `64×40×2` complex | 20.5 KB |
| `Y` (adjusted, double-buffered) | `2×38×64×2` | 38.9 KB |
| Assembled `X` | `2×38×64` | 19.4 KB |
| Frequency tables | `f_master[64]` etc., `u8` | <1 KB |
| Envelope/noise factors + `_prev` | `6×48` ×2, `6×5` ×2, `+64+64` | ~3 KB |
| Smoothing rings `g_temp`/`q_temp` | `42×48` ×2 | 16.1 KB |
| Gain/level scratch (`gain`, `q_m`, `s_m`, `e_origmapped`, `q_mapped`, `s_mapped`, `e_curr`) | `5×48` ×7 | 6.7 KB |

**Total: on the order of 120 KB/channel** (dominated by `Y`, `X_high`,
`X_low`, and the smoothing rings) — call this out explicitly as **larger than
LC's per-channel budget** (LC was "tens of KB/channel" per its own spec's
Notes) but still **bounded and fully known at compile time**. Stereo doubles
it. This is the primary driver of the AnyDecoder-size risk in §6 — the exact
byte totals get pinned down at plan/implementation time (some buffers such as
`X_high`/`Y` may be shareable/reusable scratch rather than fully persistent
per-channel state; that reuse analysis is plan-level work, not this spec's).

### 3.3 Firmware no-panic invariant

Every SBR bitstream field is bounds-checked → `Err(DecodeError::Malformed)`
(or `Unsupported` where the reference says "reject/revert to upsample-only"),
**never** a panic or out-of-bounds index — identical to the invariant the LC
decoder and the fill-element byte-span clamp already uphold. This spans:
`num_env`/`L_E`/`L_Q`, all band counts (`N_master`, `N_high`, `N_low`, `N_Q`,
`M`, limiter bands), every frequency-table index (`k0`, `k2`, `kx`, patch
indices), Huffman-decoded envelope/noise values (range-checked, `§6.2`/`§6.3`
call out the exact `≤127`/`≤30` bounds), noise/sine flags, `bs_xover_band`,
and the SBR extension byte-span itself (`§1`'s "clamp all reads to
`[payload_start, payload_start + count·8)`" — on overrun, the whole frame's
SBR is discarded and the decoder falls back to upsample-only for that frame,
matching faad2/FFmpeg's `sbr_turnoff()` behavior). A header/derived-table
validation failure (`§2`/`§3`'s `Reset` rules) reverts to the last known-good
header rather than propagating a hard decode error, mirroring the reference's
own graceful-degradation stance — full decode failure is reserved for
structurally unrecoverable cases (e.g. the LC core itself failing, which is
already handled).

## 4. Focused units (`formats/aac/sbr/`)

Mirrors the LC spec's per-unit decomposition. Each becomes a plan task with
its own test; all `no_std`, no-alloc, `#![forbid(unsafe_code)]`. New
submodule tree under `formats/aac/sbr/`, wired from `formats/aac/mod.rs`
alongside `bits`/`huffman`/`tables`/`syntax`/`dsp`/`filterbank`.

- **`sbr/mod.rs`** — the module's own top-level orchestration
  (`sbr_extension_data` entry point + per-channel state struct + the
  step-by-step drive matching `§10`'s assembly list). *Input:* the
  fill-element's bit span + `id_aac` + the LC-decoded core PCM for this
  frame. *Output:* 2× PCM for the channel(s), or a pass-through signal
  ("SBR not active this frame") that tells `decode_rdb` to fall back to
  plain LC-rate output. *Depends on:* all units below.
- **`sbr/bitstream.rs`** — `sbr_extension_data()`/fill-element dispatch,
  `sbr_data(id_aac)` framing, the CRC field (validate-or-skip), the byte-span
  clamp (`§1`), and the trailing **`bs_extension_id` loop**. *Input:*
  `BitReader` positioned at the fill element payload + `count`. *Output:*
  dispatch into `sbr_header`/`sbr_grid`/etc. below, with bit-position
  bookkeeping enforcing the clamp; and, after `sbr_data`, walk the
  length-prefixed `bs_extension` loop of `bs_extension_id`-tagged
  sub-extensions — in Phase 2 each is length-skipped, but the loop is parsed
  (not treated as opaque padding) so **Phase 3 PS dispatches its
  `EXTENSION_ID_PS == 2` sub-extension to `ps::parse_frame` from exactly this
  loop** (§2.4), trusting the returned bit count. *Depends on:* `bits::BitReader`
  (existing).
- **`sbr/header.rs`** — `sbr_header()` parse (`§2`) + the `Reset` /
  header-change comparison logic (diff against the previous header's
  `bs_start_freq`/`bs_stop_freq`/`bs_freq_scale`/`bs_alter_scale`/
  `bs_xover_band`/`bs_noise_bands`). *Input:* `BitReader`. *Output:* a
  `SbrHeader` struct + a `reset: bool`/`limiter_only: bool` flag. *Depends
  on:* `bits`.
- **`sbr/freq_tables.rs`** — the frequency-band math (`§3`): `k0`/`k2`
  (start/stop channel), `f_master` (linear or warped-log path, needs
  `log2`/power tables from §3.1), the derived `f_table_res[HI/LO]`, `kx`,
  `M`, `N_high`/`N_low`, `f_table_noise`, `f_table_lim`. *Input:* `SbrHeader`
  fields + `sr_index` (output rate) + the HF-gen patch borders (for the
  limiter table, so this has a callback/two-pass relationship with
  `hfgen.rs`'s patch construction — noted as a dependency-ordering detail for
  the plan). *Output:* the fixed `u8` frequency-table arrays from `§0`.
  *Depends on:* `sbr/header.rs`, the log2/power const tables (§3.1), and
  (for the limiter table) `sbr/hfgen.rs`'s patch borders. **Verify-first
  unit** (like the LC IMDCT): validated against `§3.5`'s two worked examples
  before anything downstream uses it — a table bug here silently corrupts
  every later stage.
- **`sbr/grid.rs`** — `sbr_grid()` (`§4`): the four frame-class variants
  (FIXFIX/FIXVAR/VARFIX/VARVAR), envelope/noise time borders (`t_E`, `t_Q`),
  `bs_freq_res[env]`, `bs_pointer`, `l_A`. *Input:* `BitReader` (per
  channel, or copied for coupled ch1 — `§5`). *Output:* `SbrGrid` (borders +
  resolution flags + derived `L_E`/`L_Q`/`l_A`). *Depends on:* `bits`.
- **`sbr/dtdf_invf.rs`** — `sbr_dtdf()` + `invf_mode()` (`§5`): the
  delta-coding direction flags and per-noise-band inverse-filter mode, with
  the CPE coupling copy (ch0→ch1) for `invf_mode`. *Input:* `BitReader`,
  `SbrGrid.L_E/L_Q`, `N_Q` (from `freq_tables`). *Output:* `bs_df_env[5]`,
  `bs_df_noise[2]`, `bs_invf_mode[5]`. *Depends on:* `bits`, `freq_tables`
  (for `N_Q`), `grid` (for `L_E`/`L_Q`).
- **`sbr/envelope.rs`** — envelope + noise Huffman decode and dequant
  (`§6`): Huffman table selection (amp_res/coupling/delta-time-vs-freq),
  `sbr_envelope`/`sbr_noise` read, `extract_envelope_data`/
  `extract_noise_floor_data` delta resolution (including the HI↔LO
  resolution remap), and `sbr_dequant` (linear energy, both non-coupled and
  coupled/pan forms). *Input:* `BitReader`, `SbrGrid`, `freq_tables`'
  `n[res]`/`N_Q`, previous frame's `E_prev`/`Q_prev`. *Output:*
  `env_facs`/`noise_facs` (linear, `f32`) ready for HF adjustment; updates
  `E_prev`/`Q_prev` for the next frame. *Depends on:* `bits`, `huffman`-style
  Huffman decode (new SBR-specific trees, verbatim §3 — likely a small
  `sbr/huffman.rs` sharing the existing `huffman.rs` tree-walk pattern rather
  than a new mechanism), `grid`, `freq_tables`.
- **`sbr/qmf.rs`** — QMF analysis + synthesis (`§7`) — **the heavy DSP unit,
  the SBR analogue of the LC IMDCT.** Complex-exponential modulated 32-band
  analysis / 64-band synthesis, built on the shared 640-tap prototype window
  (verbatim §1) and the const modulation tables from §3.1. *Input:* analysis
  — core LC PCM (`f32[1024]`) + persistent delay line `x[320]`; synthesis —
  an assembled complex subband matrix + persistent delay line `v[1280]`.
  *Output:* analysis — the `X[38][64]` complex matrix (**the PS seam
  boundary, §2.4**); synthesis — `f32[2048]` time-domain PCM (mono; called
  once per output channel). *Depends on:* the QMF const tables (§3.1),
  nothing else in `sbr/`. **Verify-first unit**: validated against a
  reference vector (a known input → known subband/time-domain output, per
  `§7`'s explicit warning about the `(2n−95)`/`(2n−63)` offsets) before
  wiring into the full pipeline — same discipline as the LC IMDCT twiddle
  verification.
- **`sbr/hfgen.rs`** — HF generation (`§8`): per-low-band 2nd-order LPC
  (`calc_prediction_coef`, complex autocorrelation + stability fallback),
  chirp factor smoothing (`calc_chirp_factors`, using `bw_prev` state),
  patch construction (`patch_construction`, the low→high band transposition
  map), and the actual per-band HF generation MAC (`X_low → X_high`).
  *Input:* `X_low` (from `qmf.rs` analysis, with `tHFGen=8`-slot lookback),
  `bs_invf_mode` (from `dtdf_invf.rs`), `freq_tables`' `f_master`/`kx`/`M`/
  `table_map_k_to_g`. *Output:* `X_high[kx..kx+M-1]` complex, plus the patch
  borders `freq_tables.rs` needs for the limiter table (see the
  dependency-ordering note there), plus persisted `bwArray_prev`/LPC state
  across frames. *Depends on:* `qmf.rs` (reads `X_low`), `dtdf_invf.rs`,
  `freq_tables.rs` (reads `f_master`/`kx`/`M`, writes back patch borders).
- **`sbr/hfadj.rs`** — HF adjustment (`§9`): mapping envelope/noise energy
  onto QMF bins (`sbr_mapping`), current-envelope energy estimate
  (`sbr_env_estimate`), gain/limiter/noise/sine-level calculation
  (`sbr_gain_calc`, using the one `√` transcendental from §3.1), and final
  assembly with smoothing + noise + sine addition (`sbr_hf_assemble`,
  reading the noise table `V[512]` and cycling `indexNoise`/`indexSine`
  state across frames). *Input:* `X_high` (from `hfgen.rs`), `env_facs`/
  `noise_facs` (from `envelope.rs`), `freq_tables`' limiter table, `SbrGrid`.
  *Output:* `Y[..][kx..kx+M-1]` — the adjusted high band for this channel,
  plus persisted smoothing rings / noise-index / sine-phase state. *Depends
  on:* `hfgen.rs`, `envelope.rs`, `freq_tables.rs`, `grid.rs`, the `√`
  function and `V[512]` table (§3.1).
- **`sbr/assemble.rs`** — assembly + output + the PS seam (`§10`): splices
  `X_low[0..kx-1]` (unchanged low band, delayed by `tHFAdj`) with
  `Y[kx..kx+M-1]` (adjusted high band) into the full complex subband matrix,
  honoring the frame-boundary slot bookkeeping (`i_Temp`), then calls QMF
  synthesis. This is also where the **coupled-channel (`bs_coupling`) pan
  un-mapping to L/R energies** (`§10`'s stereo note, feeding back into
  `envelope.rs`'s dequant) and the **PS seam hand-off point** (§2.4) live —
  the function boundary between "produce the assembled mono matrix" and
  "synthesize" is exactly where Phase 3 will later intercept. *Input:*
  `X_low`, `Y`, per-channel state. *Output:* the assembled `X[38][64]`
  matrix → `qmf.rs` synthesis → `f32[2048]` per channel. *Depends on:*
  `qmf.rs` (calls synthesis), `hfadj.rs` (reads `Y`).
- **Wiring into `AacDecoder`/the fill-element path** (`formats/aac/mod.rs`,
  `formats/aac/syntax.rs::decode_fil`) — not a new module, but a focused
  change: `decode_fil` dispatches `EXT_SBR_DATA`/`_CRC` to `sbr::mod`, the
  LC PCM → SBR hand-off point in `decode_rdb` (§2.2), `stream_info()`'s
  rate promotion (§2.1), and the growth of `PCM_CAP`/scratch buffers to the
  2× frame size (§2.3).

This covers all 10 reference sections: §0→ RAM budget baked into every unit's
sizing; §1→ `sbr/bitstream.rs`; §2→ `sbr/header.rs`; §3→ `sbr/freq_tables.rs`;
§4→ `sbr/grid.rs`; §5→ `sbr/dtdf_invf.rs`; §6→ `sbr/envelope.rs`; §7→
`sbr/qmf.rs`; §8→ `sbr/hfgen.rs`; §9→ `sbr/hfadj.rs`; §10→ `sbr/assemble.rs`
+ the `AacDecoder` wiring.

## 5. Testing — lossy, and no local HE-AAC encoder exists

**Constraint (RESOLVED — verified in this environment):** the native `aac`
encoder is AAC-LC-only and there is no `libfdk_aac`, BUT the Windows
**MediaFoundation encoder `aac_mf` DOES encode HE-AAC (SBR)** — verified:
`ffmpeg -i tone.wav -c:a aac_mf -b:a 24k out.m4a` produces a stream ffprobe
reports as `profile=HE-AAC`, and ffmpeg's native decoder round-trips it to
full-rate PCM. So SBR fixtures are **self-encodable**, exactly like AAC-LC's
were (just via `aac_mf` instead of the native `aac` encoder) — the earlier
"must source a real HE-AAC bitstream" concern does **not** apply to Phase 2.
(Note: `aac_mf` will **not** emit HE-AAC**v2** / PS even at very low bitrates —
that is Phase 3's problem, not this one.)

- **Conformance fixture (self-encoded, low risk):** Task 0 generates a
  **broadband** source (NOT the low-frequency 440/659 Hz tone — SBR reconstructs
  the *high* band, so the fixture source must carry real high-frequency energy
  above the SBR crossover for the test to be sensitive to SBR bugs; e.g. shaped
  noise or a wide chirp spanning to ~16 kHz), encodes it with
  `ffmpeg -c:a aac_mf -b:a 24k` to `testdata/sbr.m4a` (and `-f adts` to
  `testdata/sbr.aac`), and verifies ffprobe reports `HE-AAC` + the file carries
  `EXT_SBR_DATA`. Small/short to keep the committed binary small; provenance in
  `testdata/README.md`. Same `testdata/` convention as the AAC-LC fixtures.
- **Oracle: ffmpeg's own decode of that fixture to s16le PCM.** ffmpeg
  decodes HE-AAC (SBR) natively via its built-in `aac` decoder even though it
  cannot *encode* it — decode and encode are independent code paths. This
  reuses the exact `aac.m4a.ref.pcm`-style golden convention already
  established for AAC-LC.
- **Metric: segmental SNR with a tolerance, priming-aligned** — identical
  methodology to the AAC-LC golden (`tests/decode.rs::best_align_segsnr`):
  best-alignment search over a small frame-offset window, `SNR = 10·log10(Σ
  ref² / Σ (ref−ours)²)`. The **threshold is calibrated from the measured
  value** once the fixture is sourced and a first decode exists — not
  guessed up front, exactly as the LC spec's bar was calibrated (55.3 dB
  measured → 45 dB gate, ~10 dB margin). SBR reconstruction is inherently
  approximate (LPC-predicted high band + noise/sine synthesis, not a
  deterministic inverse), so expect this bar to sit **lower** than the pure
  LC bar — document the measured number and the chosen margin in the test,
  same as today.
- **No clean tone-FFT sanity test is feasible** — unlike LC, there is no way
  to encode a known tone to HE-AAC locally to backstop the ffmpeg comparison.
  The conformance-vs-ffmpeg gate is therefore **primary**, not secondary.
- **Structural sanity gate (the backstop instead):** independent of ffmpeg,
  assert on the *shape* of the output — output length is exactly `2×` the
  core frame's sample count per channel; every channel is non-silent
  (nonzero RMS over the decoded clip); and a coarse not-garbage check (no
  runaway DC offset / no clipped-every-sample output / finite values only —
  reuses whatever the existing FFT `analyze()` helper already exposes for
  gross-error detection, applied to level/non-silence rather than pitch
  since pitch content isn't controlled here).
- **No-panic fuzz:** the sourced HE-AAC fixture joins the standing robustness
  fuzz sweep (`tests/robustness.rs`) as a new seed, exercising the SBR
  bitstream fields specifically (mutate `sbr_header`/`sbr_grid`/envelope
  Huffman bytes) — every field bounds-checked to `Err`, never panic/OOB
  (§3.3's invariant), same pattern as the existing AAC/ADTS/`mp4a` seeds.
- **Unit tests per module** (mirroring the LC spec's §4): known-vector tests
  for the QMF analysis/synthesis (reference-vector validated, §4's
  "verify-first" note), the frequency-table worked examples from `§3.5`
  (both the linear and warped-log paths — these are given numerically in the
  reference specifically so they can be unit-tested directly), SBR Huffman
  decode of known codewords, the LPC/chirp math, and the gain/limiter
  calculation against a hand-computed small case.

## 6. Scope

**In (Phase 2):** dual-rate (2×) SBR reconstruction; complex QMF analysis +
synthesis; mono and stereo including `bs_coupling`; both explicit (AOT 5) and
implicit SBR signalling; the full `§1`–`§10` bitstream + DSP pipeline; the PS
seam (§2.4) exposed but unused (mono→mono, stereo→stereo, no decorrelation);
firmware no-panic invariant; SBR const tables via `xtask`.

**Out:** downsampled/single-rate (1×) SBR (`§7.3`) — note below; Parametric
Stereo (Phase 3); PCE-defined >2-channel layouts (inherits the LC limitation);
seeking (inherits the LC `Unsupported` stance); SBR bitstream CRC
*enforcement* (the field is read per `§1` but validation vs. skip-on-mismatch
is a plan-level judgment call, matching faad2/FFmpeg's own "decoders may
skip" latitude).

## 7. Notes / risks

- **Should SBR be split into sub-phases?** This spec deliberately keeps SBR
  as **one plan**, per the task brief's default. It is a large unit relative
  to LC — 10 reference sections vs LC's more modest set — but the sections
  have a strict, mostly-linear data dependency (bitstream → frequency tables
  → grid → envelope/noise → QMF analysis → HF gen → HF adjust → assemble/
  synthesize), which is exactly the shape a single ordered plan handles well
  (each task both implements and unlocks the next, as LC's IMDCT-then-wiring
  sequence did). The one candidate for a **hard split** would be QMF (§7) as
  its own preliminary plan/PR — it is self-contained (only needs the const
  tables), independently verifiable against a reference vector, and both HF
  generation and assembly depend on it — but the brief's default is one plan
  and nothing here strictly requires splitting it out. **Flagging this as an
  open question for the user**, not deciding it unilaterally.
- **RAM/stack risk (carries forward the AnyDecoder precedent):** SBR's
  ~120 KB/channel budget (§3.2) will make `AacDecoder` — and therefore the
  `AnyDecoder` enum's overall stack footprint — significantly larger than
  today, plausibly overtaking the current largest variant (`Flac`, per the
  existing `#[allow(clippy::large_enum_variant)]` note). This is the same
  shape of problem the project already solved once (multichannel FLAC's
  ~147 KiB overflowing the default stack, worked around by running decode on
  a big-stack thread in the CLI) — expect the same mitigation to extend to
  SBR-active AAC decode, not a new mechanism. Called out here so the plan
  budgets for re-measuring stack headroom during implementation, not
  rediscovering the issue.
- **Fixture is self-encoded (§5, RESOLVED):** `aac_mf` encodes HE-AAC locally
  (verified), so Task 0 self-produces the fixture from a broadband source — no
  sourcing/licensing risk. The only care needed is picking a source with real
  high-frequency energy (so SBR actually reconstructs something the test can
  measure) rather than the low-frequency LC tone.
- **QMF + frequency-band tables are the hardest units**, directly analogous
  to the LC IMDCT: both get a reference-vector/worked-example validation
  pass *before* the rest of the pipeline depends on them (§4 marks both as
  "verify-first").
- **Placeholder/consistency self-check:** every reference section (§0–§10)
  maps to a named unit or an explicit cross-cutting concern (§4's closing
  paragraph); the PS seam is specified as a concrete data shape
  (`X[38][64]` complex in, two L/R `64×32` matrices out) and a concrete
  function-boundary location (`sbr/qmf.rs` analysis output / `sbr/qmf.rs`
  synthesis input, mediated by `sbr/assemble.rs`), not a vague "SBR will
  support PS later" — Phase 3's spec can cite this section directly.
