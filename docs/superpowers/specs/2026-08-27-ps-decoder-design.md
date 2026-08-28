# AAC Parametric Stereo (PS) decoder (Phase 3 of HE-AACv2) — design

**Date:** 2026-08-27
**Status:** Approved (design review)
**Depends on:** the SBR decoder
(`docs/superpowers/specs/2026-08-27-sbr-decoder-design.md`,
`formats/aac/sbr/{qmf,assemble,bitstream,...}.rs`), which itself depends on
AAC-LC (`2026-08-26-aac-lc-decoder-design.md`), the `Decoder`/`AnyDecoder`
seam, and the `xtask` const-table generator pattern
(`xtask/src/filterbank_tables.rs`, `resample_tables.rs`).
**Reference:** `.superpowers/sdd/ps-reference.md` (8 numbered sections, cited
as `§N` below) + `.superpowers/sdd/ps-reference-verbatim.md` (Huffman trees,
hybrid-FIR prototypes, allpass/phase tables — referenced as `V1`..`V6`, not
inlined).

## 0. Phasing

Per the AAC-LC spec's §0 and the SBR spec's §0, HE-AACv2 is three sequential
phases:

1. **AAC-LC** (done) — the core codec.
2. **SBR** (drafted, `2026-08-27-sbr-decoder-design.md`) — wraps LC with a
   QMF filterbank + high-frequency reconstruction; exposes a PS seam (its
   §2.4) that Phase 2 itself leaves unused (mono→mono, stereo→stereo, no
   decorrelation).
3. **PS** (this spec) — Parametric Stereo; parametrically upmixes a **mono**
   SBR signal to stereo inside SBR's complex QMF domain. Completes HE-AACv2.
   Nothing follows PS in the phase list.

PS depends on SBR's merged Phase-2 work (specifically `formats/aac/sbr/qmf.rs`
and `formats/aac/sbr/assemble.rs`) and is out of scope for anything upstream.

## 1. Goal

**Full PS**, not a subset: IID + ICC + IPD/OPD, both the 20-band and 34-band
parameter configs (including the fine-IID modes, `iid_mode` 0..5). IPD/OPD is
**included** for fidelity, not deferred to a later slice — per `ps-reference.md`
§7 it is a bounded, table-driven addition (no new algorithmic class), so
carving it out would buy little and would leave PS's stereo image measurably
worse on content that uses it.

`no_std` (outside `test`), no-alloc, `#![forbid(unsafe_code)]` — the crate-wide
invariants already in force (`crates/soul-audio-core/src/lib.rs`). No `libm`:
every trig/allpass/phase value PS needs is a precomputed `const` table (§3). No
new dependency: PS reuses the crate's own bit reader, its own small `Complex`
representation (reconciled with SBR's, §2), and nothing else.

**What "HE-AACv2" means architecturally, precisely stated because it drives
the whole seam design:** an HE-AACv2 stream carries a **mono** AAC-LC core
(a single SCE, never a CPE) plus SBR plus a PS extension. SBR reconstructs the
high band of that mono signal in the QMF domain; PS is the **only** stage in
the entire pipeline that turns 1 channel into 2 — it does so by synthesizing a
stereo image from IID/ICC/IPD/OPD side information, not by decoding two
independently-coded channels. A stream with an SBR-extended **stereo** CPE
core (explicit two-channel HE-AAC, no PS) is SBR's domain (its §2.4/§10
`bs_coupling` note) and never reaches PS — `PsDecoder` is simply not invoked
for it.

## 2. Seam — what PS consumes from / returns to SBR

This section is the one the SBR spec's `§2.4`/`§4` must stay reconciled with;
two concrete gaps are flagged at the end.

**Location (function boundary):** `formats/aac/sbr/assemble.rs`. The SBR
spec's own unit description for that file names this exactly: *"the function
boundary between 'produce the assembled mono matrix' and 'synthesize' is
exactly where Phase 3 will later intercept."* In non-PS operation,
`assemble.rs` builds the assembled mono matrix (SBR reference §10 step 5:
splice `X_low[0..kx-1]` unchanged low band with `Y[kx..kx+M-1]` adjusted high
band) and calls `sbr/qmf.rs::qmf_synthesis` once. When PS is active,
`assemble.rs` instead hands that **same assembled mono matrix** to PS, gets
back **two** matrices, and calls `qmf_synthesis` **twice** — exactly the "Phase
3 will call it twice" behavior the SBR spec already anticipates.

**Type:** `X[38][64]` complex — the type SBR's own `qmf.rs` defines and
threads through its whole pipeline (analysis output → HF-gen → HF-adjust →
assemble). 64 QMF bands; **38 = 32 new time slots (this frame) + 6 slots of
previous-frame history**. Per `ps-reference.md` §5, the history occupies the
**low** indices — `X[0..6]` = the previous frame's trailing 6 slots, `X[6..38]`
= this frame's 32 new slots (faad2 reads the input as `X[n+6][band]`, i.e. slot
`n`'s "current" sample sits 6 past the frame start) — needed because PS's own
13-tap hybrid FIR (§5 below) has a 6-slot group delay and must stay
time-aligned across the split/pass-through bands. **This is why the seam
matrix is `[38]`, not `[32]`:** it is not a PS-specific requirement bolted onto
SBR — it is the exact shape the SBR spec already committed to for its own
internal buffer, which happens to be sufficient for PS's group-delay need too.
PS carries its **own** additional history internally (§4, hybrid + decorrelation
rings) on top of this; the 6-slot history on `X` itself is SBR's to provide.

**PS's job:** consume one mono `X[38][64]`, return **two** matrices
`X_left`, `X_right`, shaped to match whatever `sbr/qmf.rs::qmf_synthesis`
takes as its `x` parameter (the SBR spec writes this as `X[..][64] complex` —
current-frame-only, no forward history needed on the synthesis side, unlike
PS's own analysis-side need above). Concretely: 64 QMF bands × 32 time slots
each, complex. `assemble.rs` then calls `qmf_synthesis(&X_left, ...)` and
`qmf_synthesis(&X_right, ...)` for the two output channels.

**Trigger — when PS runs:** only when `ps_data_available` is true, i.e. a PS
header has been successfully parsed at least once (`ps-reference.md` §1/§8).
Concretely this is discovered inside `sbr_extension_data()`'s trailing
extension region (§1) the first time an `EXTENSION_ID_PS` (`bs_extension_id
== 2`) sub-extension's `ps_data()` sets `enable_ps_header`. Before that,
`assemble.rs` calls `qmf_synthesis` once (mono) exactly as SBR Phase 2 does
today — the PS-active path is additive, not a fork of the mono path.

**Channel-count promotion:** mirrors SBR's own sample-rate promotion pattern
(SBR spec §2.1 — `stream_info().sample_rate` promotes 1×→2× the first time SBR
is confirmed present). Symmetrically, `AacDecoder::stream_info().channels`
promotes **1→2** the first time a PS header decodes successfully; before that,
1 (mono, SBR-only, matching SBR's own explicit statement that "SBR does not
add channels; PS will, in Phase 3, but that is invisible to Phase 2").

**No `PCM_CAP` growth needed:** the SBR spec's §2.3 already grows the
interleaved output buffer to `2048 * 2` samples/frame to cover *its own*
stereo (CPE) case. A PS-active mono-core-to-stereo output is exactly
`2048 * 2` interleaved samples too — the same buffer, no further sizing
change.

**Two concrete reconciliation gaps for the SBR spec (not just naming):**

1. **The `bs_extension_id` loop is currently undocumented on the SBR side.**
   The SBR spec's §1 (bitstream plumbing) parses `sbr_extension_data()`'s
   trailing region as opaque *"`bs_extension` bits + byte-alignment padding"*
   to skip. `ps-reference.md` §1 spells out what that region actually is: a
   1-bit gate (`bs_extended_data`) then a length-prefixed loop of
   `bs_extension_id`-tagged sub-extensions (`EXTENSION_ID_PS == 2` among
   them), and PS's entire payload rides inside it. This is a real gap, not a
   naming mismatch: whichever SBR unit currently treats that region as
   discardable padding (§1 says "the reader must track bits consumed vs
   `count·8`", which is the right mechanism, just not yet wired to dispatch
   on `bs_extension_id`) needs a small amendment to walk that loop and, for
   `id == 2`, hand off to PS's `ps_data` parser (§4 unit 1 below), trusting
   its returned bit count for the loop's own bookkeeping — exactly as
   `ps-reference.md` §1 specifies ("the SBR extension parser must trust
   `ps_data()`'s returned bit length").
2. **The §2.4 diagram's fork point is drawn one stage too early.** Its top box
   ("mono complex 64-band QMF subband matrix `X[38][64]`") sits directly under
   `QMF ANALYSIS`, before HF generation/adjustment, and the diagram draws the
   Phase-3 branch forking off that box. Physically this can't be where PS
   forks: the core LC signal is band-limited, so QMF analysis only ever
   populates the **low** bands — the high bands don't exist as real content
   until HF generation (§8–9 of the SBR reference) synthesizes them. PS needs
   all 64 (or the relevant sub-)bands, so it must consume the **assembled**
   matrix, after the low/high splice — which is exactly what the SBR spec's
   own `assemble.rs` unit description says (quoted above) and what
   `ps-reference.md` §8 and the SBR reference's own §10 step 5 both describe.
   Recommend routing the diagram's Phase-3 arrow out of the "assembled X (§10
   step 5)" box instead of the raw analysis-output box, so the diagram matches
   its own prose.

## 3. `no_std` / no-alloc / firmware constraints

### 3.1 No new dependency, no `libm`, no `unsafe`

Same crate-level invariants as LC/SBR. `[NOLIBM]` inventory (from
`ps-reference.md`'s own closing table), all `const`, all `xtask`-generated
into `formats/aac/ps/tables.rs` by a new `cargo xtask gen-ps-tables` /
`check-ps-tables` pair (exact precedent: `filterbank_tables.rs`/
`resample_tables.rs`, wired into `xtask verify` the same way):

| Category | Tables | Verbatim ref |
|---|---|---|
| Huffman codebooks | `f_huff_*`/`t_huff_*` binary-tree form (faad2 layout; use this, not FFmpeg's `{value,length}` form, per §3's own recommendation) | V1.a |
| IID/ICC dequant + mixing angles | `sf_iid_normal`/`sf_iid_fine`, `icc_invq`, `cos_alphas`/`sin_alphas`, `cos_gammas`/`sin_gammas`, `sincos_alphas_B` | V2.a/b/d |
| IPD/OPD phase | `ipdopd_cos`/`ipdopd_sin` **and** the precomputed complex smoothing tables `pd_re_smooth`/`pd_im_smooth[8×8]` (FFmpeg's `atan2`-free form — see below) | V2.c |
| Flattened↔parameter band maps | `ff_k_to_i_20[71]`, `ff_k_to_i_34[91]` (or the `group_border`/`map_group2bk` alternative) | V3 |
| Hybrid FIR prototypes | `g0_Q12`, `g1_Q8`, `g2_Q4`, `g0_Q8` (13-tap real prototypes) → PS precomputes the **modulated complex** per-subband filter coefficients from these at table-gen time | V4 |
| Allpass/decorrelation phase | `Phi_Fract_*` (hybrid + QMF variants), `Q_Fract_allpass[k][m]`, `filter_a = {0.6514.., 0.5647.., 0.4895..}` | V5 |
| Band-count / config constants | `num_env_tab`, `nr_iidicc_par_tab`, `nr_iidopd_par_tab`, `NR_BANDS`, `NR_ALLPASS_BANDS`, `DECAY_CUTOFF`, `SHORT_DELAY_BAND` | V6 |

**IPD/OPD specifically adopts FFmpeg's `pd_re_smooth`/`pd_im_smooth` complex
tables, not faad2's runtime `atan2` path** — this is explicit key decision #3
from the brief and `ps-reference.md` §7/§8 says so directly ("FFmpeg avoids
`atan2` by keeping everything as complex `(re,im)`... **do this**, not faad2's
`atan2` path").

**Settled source disagreement (key decision #5):** `ps-reference.md`'s own
"reconciled differences" note #5 flags that faad2's `nr_allpass_bands`/
`decay_cutoff` scalar values, as summarized from its source, looked
inconsistent with FFmpeg's explicit compile-time constants. This spec adopts
**FFmpeg's `NR_ALLPASS_BANDS={30,50}`, `DECAY_CUTOFF={10,32}`,
`SHORT_DELAY_BAND={42,62}` as authoritative** and defers re-verification of
faad2's private band-space numbers entirely — correctness is checked at the
conformance gate (§5), not by chasing the faad2 discrepancy further. Flagged
here so the plan doesn't silently pick a different source.

**Cosmetic disagreement, pick one:** `ps-reference.md` §5 notes faad2 splits
the 20-config's band 0 into 8 sub-bands then merges down to 6
(`channel_filter8` + `X[3]+=X[4]; X[2]+=X[5]`), while FFmpeg produces 6
directly (`hybrid6_cx`) — both conformant, same result. This spec adopts
**FFmpeg's direct-6 form**, consistent with adopting FFmpeg's constants
elsewhere (decision #5) and avoiding an unnecessary intermediate merge step.

### 3.2 Firmware no-panic invariant

Every PS bitstream field is bounds-checked → `Err(DecodeError::Malformed)`,
never a panic or OOB index — reuse `ps-reference.md` §8's `[NOPANIC]`
checklist table directly (it is already a complete, per-field enumeration:
`bs_extension_id`, the extension byte count vs. `nr_bits_left`,
`enable_ps_header`/header-seen gating, `iid_mode`/`icc_mode` range,
`nr_ipdopd_par` derivation, `frame_class`, `num_env`, `border_position[]`
monotonicity, Huffman leaf-walk termination, the post-accumulate IID/ICC/
IPD/OPD clip/wrap, `nr_*_par` sourced only from the V6 tables (never
bitstream length), `k_to_i[k]` bounds, and the transient `G_trans` divide
guard). Mirrors SBR's own graceful-degradation stance (its §3.3): a malformed
PS header or derived-table failure disables PS for that frame (mono
passthrough via `assemble.rs`'s existing single-`qmf_synthesis` path) rather
than propagating a hard decode error — this is exactly `ps_data_available`'s
own semantics in `ps-reference.md` §1 ("a frame with `enable_ps_header==0`
before any header is a no-op"), just extended to "any subsequent parse
failure also reverts to no-op" rather than being fatal to the whole stream.

### 3.3 Per-frame RAM budget

One `PsDecoder` instance total (not per-channel — there is exactly one PS
stage per stream, since the underlying core is always mono). Sized to the
34-band/91-flattened-band worst case unconditionally (fixed by-value, no
per-config resizing).

**Persistent state** (carried frame-to-frame, `ps-reference.md` §8's
inventory):

| Buffer | Shape | Approx. size |
|---|---|---|
| Header + frame history (`iid_mode`, `icc_mode`, `num_env_old`, `is34bands_old`, …) | few `u8` | <10 B |
| `iid_index_prev`/`icc_index_prev` | `i8[34]` × 2 | 68 B |
| `ipd_index_prev`/`opd_index_prev` | `i8[17]` × 2 | 34 B |
| Hybrid analysis history (`hyb->buffer`) | 12 samples × 5 bands, complex `f32` | 480 B |
| Decorrelation `delay_Qmf` | `14×64` complex `f32` | 7.0 KB |
| Decorrelation `delay_SubQmf` | `2×32` complex `f32` | 0.5 KB |
| Decorrelation `delay_Qmf_ser` | `3×5×64` complex `f32` | 7.5 KB |
| Decorrelation `delay_SubQmf_ser` | `3×5×32` complex `f32` | 3.75 KB |
| Transient state (`P_PeakDecayNrg`/`P_prev`/`P_SmoothPeakDecayDiffNrg_prev`) | `f32[34]` × 3 | 0.4 KB |
| Mixing interpolation anchors (`h11..h22_prev`) | `f32[50]` × 4 | 0.8 KB |
| IPD/OPD smoothing rings (`ipd_prev`/`opd_prev`) | `f32[20][2]` × 2 | 0.3 KB |
| **Persistent subtotal** | | **≈ 21 KB** |

**Per-frame scratch** (reused struct fields, not reallocated; sized to
`NR_BANDS=91`, the 34-config worst case):

| Buffer | Shape | Approx. size |
|---|---|---|
| Direct hybrid-domain signal `s` | `32×91` complex `f32` | 22.75 KB |
| Decorrelated hybrid-domain signal `d` | `32×91` complex `f32` | 22.75 KB |
| Output `X_left`, `X_right` | `32×64` complex `f32` × 2 | 32.0 KB |
| **Scratch subtotal** | | **≈ 77.5 KB** |

**Total ≈ 100 KB**, single instance. Two notes:

- This is materially **smaller** than SBR's own ~120 KB/**channel** budget
  (SBR spec §3.2), because PS replaces what would otherwise be a *second*
  independent SBR channel chain (stereo, non-PS: ~240 KB) with *one* SBR
  chain (~120 KB, mono) + this ~100 KB of PS state — comparable order of
  magnitude to explicit-stereo SBR, not a new RAM tier on top of it.
- `s`/`d` as full `32`-slot buffers is the straightforward design, not the
  most memory-efficient one — decorrelation is inherently a per-slot
  recursion driven by the ring buffers already listed above, so a
  slot-by-slot streaming implementation could shrink `s`/`d` well below
  32 full slots. Flagged as a plan-level optimization, not decided here (the
  fixed-worst-case-buffer approach is simpler to reason about for a first
  correct implementation and matches how AAC-LC/SBR are built today).
- Stacks onto the existing `AnyDecoder`-size risk both the AAC-LC and SBR
  specs already flag (see Notes/risks, §7).

## 4. Focused units (`formats/aac/ps/`)

New submodule tree, wired from `formats/aac/sbr/bitstream.rs` (parse-time
entry, unit 1) and `formats/aac/sbr/assemble.rs` (QMF-domain entry, unit 8) —
not from `formats/aac/mod.rs` directly, mirroring how SBR itself wires into LC
through a narrow splice point (`syntax.rs::decode_fil`) rather than a broad
top-level change. All `no_std`, no-alloc, `#![forbid(unsafe_code)]`.

1. **`ps/data.rs`** — `ps_data()` bitstream parse (§1). *Input:* `BitReader`
   positioned at the `EXTENSION_ID_PS` sub-extension payload (inside
   `sbr_extension_data()`'s trailing `bs_extension_id` loop — §2's
   reconciliation note 1) + the remaining-bits budget for that sub-extension.
   *Output:* updates `PsDecoder`'s current-frame header/frame state
   (delegating the frame/band and parameter bodies to units 2–3 below);
   returns the exact bit count consumed, which the SBR extension loop must
   trust for its own position bookkeeping (§1's explicit requirement).
   *Depends on:* `bits::BitReader` (shared with AAC-LC/SBR), `ps/frame.rs`,
   `ps/params.rs`.
2. **`ps/frame.rs`** — parameter frame & bands (§2). `frame_class` +
   `num_env_idx` → `num_env` (`num_env_tab`); fixed or variable
   `border_position[]` (with the §2 monotonicity/range `[NOPANIC]`);
   `iid_mode`/`icc_mode` → `nr_iid_par`/`nr_icc_par`/`nr_ipdopd_par` band
   counts, `iid_quant` (coarse/fine), `is34` (V6 lookups). *Input:*
   `BitReader` (variable-borders path only), `frame_class`, `num_env_idx`,
   `iid_mode`, `icc_mode`. *Output:* a `PsFrameLayout` (num_env, borders, band
   counts, `is34`, `iid_quant`). *Depends on:* `bits`, `tables.rs` (V6).
3. **`ps/params.rs`** — parameter Huffman decode + delta + dequant (§3). Per
   envelope, per parameter type (IID/ICC/IPD/OPD): read `dt`, faad2
   binary-tree Huffman-decode `nr_*_par` signed deltas (V1.a), delta-freq
   (`par[b]=par[b-1]+delta[b]`) or delta-time (vs. the same band in the
   previous envelope, or `*_index_prev` for envelope 0) accumulation, clip
   (IID `±7`/`±15`, ICC `[0,7]`) or wrap-mod-8 (IPD/OPD) `[NOPANIC]`,
   dequantize to linear gain/coherence-angle/phase via V2 tables. *Input:*
   `BitReader`, `PsFrameLayout`, the persistent `*_index_prev[]` arrays.
   *Output:* per-envelope dequantized `iid[5][34]` (gain `c`), `icc[5][34]`
   (`cos_alpha`/`sin_alpha`), `ipd[5][17]`/`opd[5][17]` (`cos`/`sin` angle);
   updates `*_index_prev[]` for next frame's delta-time base. *Depends on:*
   `bits`, `tables.rs` (V1 Huffman trees + V2 dequant tables).
4. **`ps/bands.rs`** — band mapping, 10↔20↔34 → QMF+hybrid (§4). Flattened
   processing-band ↔ parameter-band lookup (`k_to_i_20[71]`, `k_to_i_34[91]`,
   V3) with `[NOPANIC]` bounds (`k < NR_BANDS[is34]`, result `< nr_*_par`);
   exposes the derived config constants (`NR_BANDS`, `NR_ALLPASS_BANDS`,
   `DECAY_CUTOFF`, `SHORT_DELAY_BAND`, V6, per the settled FFmpeg-authoritative
   decision in §3.1). *Input:* flattened band index `k`, `is34`. *Output:*
   parameter-band index `i`. *Depends on:* `tables.rs` (V3, V6).
5. **`ps/hybrid.rs`** + hybrid FIR tables (§5) — **hardest unit, #1 of 3.**
   Analysis: FIR-splits the lowest 3 (20-cfg) or 5 (34-cfg) QMF bands of the
   mono seam matrix into 10/32 hybrid sub-subbands with the 13-tap
   modulated-prototype complex filters (V4), 6-slot group delay handled by
   reading `X[n+6][band]` and delaying pass-through bands by the same 6 slots
   (this is the seam's `[38]` history, §2). Synthesis: sums each split band's
   sub-subbands back to plain QMF after mixing (§7) has run. *Input
   (analysis):* the seam's `X[38][64]` + a persistent 12-sample×5-band
   history ring. *Output (analysis):* flattened hybrid-domain mono signal
   `s[32][NR_BANDS]` (`NR_BANDS` = 71 or 91) + updated history ring. *Input
   (synthesis):* mixed hybrid-domain `X_left`/`X_right[32][NR_BANDS]` (from
   `ps/mix.rs`). *Output (synthesis):* `X_left`/`X_right[32][64]` — the
   seam's return shape. *Depends on:* `tables.rs` (V4 modulated complex FIR,
   precomputed at table-gen time from the real prototypes), `ps/bands.rs`.
6. **`ps/decorrelate.rs`** + allpass/decay tables (§6) — **hardest unit, #2 of
   3.** Per flattened band below `NR_ALLPASS_BANDS={30,50}`: a 3-link serial
   allpass reverberator (base delay `D=2` + fractional phase `Phi_Fract`,
   per-link delays `d(m)={3,4,5}` + fractional phase `Q_Fract_allpass`,
   feedback `a(m)·g(k)` with the `DECAY_CUTOFF`/`DECAY_SLOPE` decay past the
   cutoff band); at/above that band, a pure delay (14 samples up to
   `SHORT_DELAY_BAND={42,62}`, then 1 sample). Per-parameter-band transient
   (peak-decay) attenuation scales the result (`[NOPANIC]`-guarded
   `G_trans` divide). *Input:* `s[32][NR_BANDS]` (from `ps/hybrid.rs`
   analysis) + the persistent ring buffers and transient state (§3.3's
   inventory). *Output:* decorrelated `d[32][NR_BANDS]`, alongside the
   unmodified `s`; updated rings/transient state. *Depends on:* `tables.rs`
   (V5 `Phi_Fract`/`Q_Fract_allpass`/`filter_a`), `ps/bands.rs` (for the
   band-count constants).
7. **`ps/mix.rs`** — stereo reconstruction/mixing (§7) — **hardest unit, #3 of
   3.** Per parameter band & envelope: build the 2×2 mixing matrix
   (`H11`/`H12`/`H21`/`H22`) — Type A (`icc_mode<3`, direct IID/ICC closed
   form) or Type B (`icc_mode≥3`, "enhanced" ICC via `γ`); when
   `enable_ipdopd` and the band is `< nr_ipdopd_par`, rotate the matrix
   complex using FFmpeg's precomputed `pd_re_smooth`/`pd_im_smooth[8×8]`
   tables (3-frame `[¼,½,¼]`-smoothed IPD/OPD, §3.1's `[NOLIBM]` decision);
   linearly interpolate the matrix across the envelope's QMF slots
   (`dH = (H_now − H_prev) / L_env`); apply per slot & sub-band:
   `L = H11·s + H12·d`, `R = H21·s + H22·d` (plus imaginary cross-terms when
   IPD/OPD is active). *Input:* `s`, `d[32][NR_BANDS]` (from `ps/hybrid.rs`/
   `ps/decorrelate.rs`), the dequantized `iid`/`icc`/`ipd`/`opd` arrays (from
   `ps/params.rs`), `border_position[]` (from `ps/frame.rs`), the persistent
   interpolation anchors (`h11..h22_prev[50]`) and IPD/OPD smoothing rings
   (`ipd_prev`/`opd_prev[20][2]`). *Output:* mixed hybrid-domain
   `X_left`/`X_right[32][NR_BANDS]` (fed to `ps/hybrid.rs` synthesis); updates
   the interpolation anchors and smoothing rings. *Depends on:* `tables.rs`
   (V2.a/b/d angle tables, the IPD/OPD complex-phase tables).
8. **`ps/mod.rs`** — assembly + the SBR seam + wiring into the SBR/AAC
   pipeline (§8). `PsDecoder` owns the persistent state (§3.3) and the
   per-frame scratch buffers as struct fields (fixed by-value, matching the
   `AacDecoder`/`Filterbank` convention — no heap, no per-call allocation).
   Exposes **two** entry points at the **two different points in a frame**
   where PS actually runs (`ps_data()` parses early, alongside SBR's own
   bitstream parse; the QMF-domain work runs late, at `assemble.rs` — these
   are not the same call site, which `ps-reference.md`'s single §8
   pseudocode block elides):
   - `parse_frame(bits, nr_bits_left) -> Result<u32, DecodeError>` — called
     from `sbr/bitstream.rs`'s extension-id loop (§2's reconciliation note 1)
     when `bs_extension_id == EXTENSION_ID_PS`; drives units 1–3, updates
     current- and next-frame parameter state; returns bits consumed.
   - `apply(x: &X38, out_left: &mut X32, out_right: &mut X32) ->
     Result<(), DecodeError>` — called from `sbr/assemble.rs` after it builds
     the assembled mono matrix (§2); drives units 5–7 in the §8 order
     (hybrid analysis → decorrelate → mix → hybrid synthesis) using whichever
     frame's parameters are current (this frame's, or last frame's if
     `enable_ps_header==0` — "reuse last header", §1). Only called at all
     when `ps_data_available` is true; otherwise `assemble.rs` keeps calling
     `qmf_synthesis` once, unchanged from SBR Phase 2.
   *Depends on:* units 1–7 above, plus the seam types `sbr/qmf.rs` defines.
9. **Conformance golden** (test unit, not a source module) — extends
   `crates/soul-audio-core/tests/decode.rs` with the PS-specific goldens (§5
   below) and a `testdata/ps.m4a`/`ps.aac` (+ ffmpeg `.ref.pcm`) fixture pair,
   joining the AAC-LC/SBR golden-vector pattern (`testdata/README.md`).

**Coverage check:** `ps-reference.md` §1→unit 1, §2→unit 2, §3→unit 3, §4→unit
4, §5→unit 5, §6→unit 6, §7→unit 7, §8→unit 8 (+ unit 9 for the conformance
gate §8 also implies). Every numbered section has a home.

## 5. Testing — lossy, and no local HE-AACv2 encoder exists

**Constraint (verified in this environment):** `ffmpeg -encoders` here lists
only the native `aac` and `aac_mf` (MediaFoundation) encoders — both **AAC-LC
only**, no `libfdk_aac`. There is no way to *produce* an HE-AACv2 (SBR+PS)
bitstream locally. The native **decoder** (`aac`) does decode HE-AACv2
natively, including PS — decode and encode are independent code paths, same
situation the SBR spec already documented for SBR alone.

- **Conformance fixture (Task-0-level risk, sourced not synthesized):** a
  committed real **HE-AACv2 (PS)** bitstream, sourced from the FFmpeg samples
  corpus or the ISO conformance suite (both carry PS streams for exactly this
  reason). Sourcing + verifying it — does it actually carry an
  `EXTENSION_ID_PS` sub-extension (not just SBR), does it decode cleanly with
  the local ffmpeg, is its license clear to commit — is Task 0 of the
  implementation plan, called out **as a risk** exactly as the SBR spec calls
  out its own fixture-sourcing Task 0. Prefer a small stereo clip (the format
  necessarily produces stereo output). `testdata/README.md` provenance entry
  and `testdata/ps.m4a`/`ps.aac` naming, mirroring the `sbr.m4a`/`sbr.aac`
  convention SBR's own testing section establishes.
- **Oracle: ffmpeg's own decode of that fixture to s16le stereo PCM.** Same
  `aac.m4a.ref.pcm`-style golden convention already established for AAC-LC
  and SBR.
- **Metric: segmental SNR with a tolerance, priming-aligned, per-channel.**
  Reuses `tests/decode.rs::best_align_segsnr`'s alignment search (the priming
  offset is a property of the shared mono core + envelope timeline, so one
  offset applies to both channels), but — unlike the AAC-LC/SBR goldens,
  which score the interleaved stream as one signal — **de-interleaves L and R
  and scores each channel's segmental SNR independently against ffmpeg's own
  L/R**, asserting **both** clear the (measured-then-calibrated) bar. This is
  the brief's explicit requirement: PS is where the stereo image is created,
  so a bug confined to one channel (e.g. a transposed `H12`/`H21`, per the
  reference's faad2-vs-ISO naming-transposition warning in §7) must not be
  maskable by a combined-channel metric that happens to average out. The
  threshold itself is **calibrated from the measured value**, not guessed —
  same discipline as every prior lossy golden in this codebase (AAC-LC: 55.3
  dB measured → 45 dB gate; SBR: TBD at its own implementation time).
- **Structural sanity gate (independent of ffmpeg):** output `channels == 2`;
  correct length (`2048` samples/channel per SBR frame, same rule SBR
  established); and — the PS-specific check — **the two channels genuinely
  differ**. A mono-duplicated-to-stereo bug (e.g. PS silently disabled,
  `assemble.rs` falling back to calling `qmf_synthesis` once and copying the
  result to both channels) would otherwise pass every other check. Assert
  something like `RMS(L − R) / RMS(L)` exceeds a small threshold, or that the
  per-sample-equal fraction is far below 100% — catches exactly this failure
  mode, which a same-length/non-silence check alone would miss.
- **No-panic fuzz:** the sourced PS fixture joins the standing robustness
  sweep (`tests/robustness.rs`) as a new seed, specifically exercising
  `ps_data()`'s fields (mutate `iid_mode`/`icc_mode`/`frame_class`/
  `num_env_idx`/`border_position`/the Huffman-coded delta bytes/
  `ps_extension_size`) per §3.2's `[NOPANIC]` checklist — every field bounds-
  checked to `Err`, never panic/OOB, same pattern as the existing AAC/ADTS/
  `mp4a`/SBR seeds.
- **Unit tests per module:** known-vector tests for the hybrid FIR (a known
  QMF-domain impulse/tone in → expected sub-subband split, verified against
  the reference's modulated-prototype formula before it's trusted downstream
  — this and decorrelation are the "verify-first" units, matching how the
  LC IMDCT and SBR's QMF were each validated against a reference vector
  before the rest of the pipeline depended on them), a known allpass
  impulse response for `ps/decorrelate.rs`, Huffman decode of known
  codewords (V1.b's `{value,length}` form is a ready-made cross-check table),
  the `k_to_i` band maps against V3's anchors, and a hand-computed small
  mixing-matrix case (IID-only, ICC=1 i.e. `ρ=1` — the reference's own sanity
  identity `L=c2·s, R=c1·s`, §7) for `ps/mix.rs`.

## 6. Scope

**In (Phase 3):** full PS — IID + ICC + IPD/OPD; both 20-band and 34-band
parameter configs including fine-IID modes (`iid_mode` 0..5); Type A and Type
B stereo mixing; the hybrid filterbank for both configs (6-way/10-sub-band
20-cfg split, 32-sub-band 34-cfg split); decorrelation (allpass + pure delay +
transient attenuation); wiring into `sbr/bitstream.rs` (parse) and
`sbr/assemble.rs` (QMF-domain apply); `AacDecoder` channel-count promotion
1→2; firmware no-panic invariant; PS const tables via `xtask`.

**Out:** downsampled/1× SBR (already out of SBR's own Phase-2 scope, and
PS's design here assumes the dual-rate complex QMF domain SBR commits to);
SBR-extended stereo (CPE) cores without PS (SBR's domain, not PS's — no code
path in `ps/` is ever invoked for them); seeking (inherits the `Unsupported`
stance from LC/SBR); >2-channel layouts (ISO PS is inherently mono→stereo;
there is no multichannel PS to support); SBR's own bitstream CRC enforcement
(inherited, unaffected by PS).

## 7. Notes / risks

- **One plan, like SBR.** This spec deliberately stays a single plan, matching
  the SBR spec's own default and its reasoning: PS's 8 sections have the same
  strict, mostly-linear data dependency shape (bitstream/params → bands →
  hybrid analysis → decorrelation → mixing → hybrid synthesis → seam wiring)
  that made one ordered plan work well for SBR. Unlike SBR, there isn't an
  obvious self-contained sub-unit to split out preemptively (SBR flagged QMF
  as a candidate because it's independently verifiable from const tables
  alone; PS's three hardest units — hybrid filterbank, decorrelation, mixing —
  are each independently verifiable too, but also each other's direct
  consumer in sequence, so splitting any one out wouldn't shrink the other
  two's dependency chain). Not deciding this unilaterally either way beyond
  "no evidence to deviate from the SBR precedent" — flag as open if the user
  disagrees once the plan is scoped.
- **Hardest units, in order: hybrid filterbank (`ps/hybrid.rs`),
  decorrelation (`ps/decorrelate.rs`), stereo mixing (`ps/mix.rs`).** All
  three are DSP-heavy, table-driven, and only observable end-to-end (through
  the ffmpeg conformance gate) or via careful known-vector unit tests — same
  "verify-first" discipline the LC IMDCT and SBR's QMF each required.
- **Two SBR-spec reconciliation gaps, restated from §2:** (1) SBR's §1
  currently treats `sbr_extension_data()`'s trailing region as discardable
  padding; it's actually the `bs_extension_id` loop PS's payload rides in,
  and the loop-walking + `id==2` dispatch needs adding to whichever SBR unit
  owns that parse. (2) SBR's §2.4 diagram forks the Phase-3 branch one stage
  too early (off raw QMF-analysis output instead of the post-HF-gen
  assembled matrix); its own `assemble.rs` prose already has this right, only
  the diagram needs the arrow moved. Neither blocks writing this spec, but
  both should be fixed before/while the SBR plan is executed, since PS's plan
  will otherwise be built against a slightly different picture than SBR's.
- **`Complex` type sharing (open wiring detail, not decided here):** neither
  this spec nor the SBR spec pins a concrete Rust type/field-name for the
  `X[38][64]` complex matrix — the SBR spec deliberately leaves it a "function/
  struct boundary, not dynamic dispatch" without naming the struct. PS should
  reuse whatever `sbr/qmf.rs` defines (e.g. a `Complex { re: f32, im: f32 }`
  akin to the module-private `Cpx` already in `formats/aac/filterbank.rs`,
  promoted to `pub(crate)` and shared) rather than defining a second,
  incompatible complex type. Flag for the SBR/PS plans to agree on explicitly
  when `qmf.rs` lands.
- **RAM/stack risk, stacking on the existing precedent:** PS's own ~100 KB
  (§3.3) adds to SBR's ~120 KB/channel and LC's own state inside the single
  `AacDecoder` — all embedded by-value in `AnyDecoder` (no heap). This is the
  same shape of problem the multichannel-FLAC stack overflow already
  surfaced and SBR's spec already re-flagged: expect the same mitigation
  (decode on a big-stack thread on desktop; static/BSS placement on firmware)
  to extend here too, not a new mechanism — but the plan should re-measure
  actual stack headroom once PS lands, since `AnyDecoder`'s total size will
  have grown across three phases by then.
- **Fixture-sourcing risk (§5) — REAL for PS, unlike SBR.** Verified in this
  environment: `aac_mf` encodes HE-AAC (SBR) but **not** HE-AACv2 (PS) — it
  stays discrete-stereo HE-AAC even at 10 kb/s. So, unlike SBR (whose fixture
  is self-encodable), the **PS fixture must be sourced** (a real HE-AACv2/PS
  bitstream carrying an `EXTENSION_ID_PS` sub-extension). A first attempt to
  pull one from `samples.ffmpeg.org` failed (host unreachable/slow from this
  sandbox). Phase-3 Task 0 must therefore resolve fixture sourcing as a genuine
  **blocking first step** — retry the FFmpeg samples mirror(s), the ISO/3GPP
  conformance corpus, or ask the user to supply a small PS clip. ffmpeg's
  native decoder *does* decode PS, so once a fixture exists the oracle path is
  the same as SBR's.
- **Placeholder/consistency self-check:** every reference section (§1–§8)
  maps to a named unit (§4's coverage-check paragraph); the seam is specified
  as one concrete data shape (`X[38][64]` complex in, two `64×32` complex
  matrices out) and one concrete function boundary
  (`sbr/assemble.rs`, between "assemble" and "synthesize") — not a restated
  "PS runs somewhere in SBR's QMF domain." The two SBR-spec gaps above are
  named as gaps to close, not glossed over as already-consistent.
