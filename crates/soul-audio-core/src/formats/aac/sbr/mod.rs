//! SBR (Spectral Band Replication, HE-AAC Phase 2) decoder. `tables` holds
//! the const transcendental tables (`cargo xtask gen-sbr-tables`); `bitstream`
//! owns the fill-element envelope (CRC field, `bs_header_flag` framing, the
//! byte-span clamp, the trailing `bs_extension` loop). This module's [`run_frame`]
//! is the §10 per-channel drive that ties every prior `sbr::*` unit together:
//! QMF analysis of the AAC-LC core → HF generation → HF adjustment →
//! [`assemble`](assemble::assemble) splice → QMF synthesis, emitting **2048
//! samples/channel** (2× the core rate) per frame.

pub(crate) mod tables;

pub(super) mod assemble;
pub(super) mod bitstream;
pub(super) mod dtdf_invf;
pub(super) mod envelope;
pub(super) mod freq_tables;
pub(super) mod grid;
pub(super) mod header;
pub(super) mod hfadj;
pub(super) mod hfgen;
pub(super) mod huffman;
pub(super) mod qmf;

use assemble::assemble;
use bitstream::{IdAac, SbrPayload, clamped_read};
use dtdf_invf::{read_dtdf, read_invf};
use envelope::{
    EnvLinear, EnvPrev, EnvQuant, dequant_coupled, dequant_sce, read_env_noise, read_envelope,
    read_noise,
};
use freq_tables::{FreqTables, derive, derive_limiter};
use grid::{SbrGrid, sbr_grid};
use header::SbrHeader;
use hfadj::{HfAdjState, hf_adjustment};
use hfgen::{
    ChirpState, Patches, calc_chirp_factors, calc_prediction_coef, hf_generation,
    patch_construction,
};
use qmf::{
    Cpx, MAX_SLOTS, MAX_Y_SLOTS, N_ANA, N_SYN, QmfAnalysisState, QmfSynthesisState, T_HFADJ,
    T_HFGEN, TIME_SLOTS_RATE, qmf_analysis, qmf_synthesis,
};

use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// Frequency + patch tables, rebuilt once per accepted header/reset (§3, §8.3).
/// `Copy` so the driver can lift it out of `SbrState` and hand `ft`/`patches`
/// to the per-channel DSP without colliding with the `&mut` borrow of the
/// channel state.
#[derive(Clone, Copy)]
struct Tables {
    ft: FreqTables,
    patches: Patches,
}

/// One SBR channel's cross-frame DSP state (`sbr-reference.md` §10 tail).
/// A CPE keeps two; an SCE uses `ch[0]`.
#[derive(Clone)]
struct SbrChannel {
    /// Analysis QMF input delay line (§7.4).
    qmf_ana: QmfAnalysisState,
    /// Synthesis QMF delay line (§7.4).
    qmf_syn: QmfSynthesisState,
    /// Low-band QMF buffer `[slot][band]`: this frame's 32 fresh analysis
    /// slots live at physical `[T_HFGEN, T_HFGEN+32) = [8, 40)`; slots `[0, 8)`
    /// carry the previous frame's last 8 analysis slots (the LPC lookback, §8).
    x_low: [[Cpx; N_ANA]; MAX_SLOTS],
    /// Chirp (inverse-filter bandwidth) smoothing state (§8.2).
    chirp: ChirpState,
    /// Envelope/noise delta-time base carried from the previous frame (§6.2).
    env_prev: EnvPrev,
    /// HF-adjustment smoothing rings + noise/sine phase (§9.4).
    hf_adj: HfAdjState,
    /// Previous frame's full 38-slot adjusted high band `Y` — its overlap
    /// slots `[32, 38)` become the first `i_Temp` output slots of THIS frame
    /// (§10 `i_Temp`/`Ypos` ping-pong, FFmpeg `sbr_x_gen`).
    y_prev: [[Cpx; N_SYN]; MAX_Y_SLOTS],
}

impl Default for SbrChannel {
    fn default() -> Self {
        Self {
            qmf_ana: QmfAnalysisState::default(),
            qmf_syn: QmfSynthesisState::default(),
            x_low: [[Cpx::default(); N_ANA]; MAX_SLOTS],
            chirp: ChirpState::default(),
            env_prev: EnvPrev::default(),
            hf_adj: HfAdjState::default(),
            y_prev: [[Cpx::default(); N_SYN]; MAX_Y_SLOTS],
        }
    }
}

/// Per-stream SBR decode state, carried frame-to-frame across calls to
/// [`bitstream::sbr_extension_data`]. `kx`/`m` are the frequency-table anchors
/// (`sbr-reference.md` §3). `header` is the last accepted [`SbrHeader`] (`None`
/// until `bs_header_flag` first fires — §2); `active` promotes to `true` the
/// first time one is accepted (from then on the stream emits 2×-rate output).
/// `tables` holds the derived frequency/patch tables (rebuilt on
/// header-reset); `ch` holds each channel's cross-frame DSP state.
/// [`Default`] is "not active" — pure upsample-only.
pub(super) struct SbrState {
    pub(super) kx: u8,
    pub(super) m: u8,
    header: Option<SbrHeader>,
    pub(super) active: bool,
    tables: Option<Tables>,
    ch: [SbrChannel; 2],
    /// Parametric Stereo decoder (HE-AACv2, Phase 3). Always present; a
    /// `Default` `PsDecoder` is inactive (no header seen), so an SBR-only or
    /// LC-only stream never touches it. Fed by the SBR extension loop
    /// ([`bitstream::parse_extensions`]) behind `EXTENSION_ID_PS`; consumed by
    /// [`drive`] to upmix a mono core to stereo in the QMF domain (§8).
    pub(super) ps: crate::formats::aac::ps::PsDecoder,
}

impl Default for SbrState {
    fn default() -> Self {
        Self {
            kx: 32,
            m: 0,
            header: None,
            active: false,
            tables: None,
            ch: [SbrChannel::default(), SbrChannel::default()],
            ps: crate::formats::aac::ps::PsDecoder::default(),
        }
    }
}

impl SbrState {
    /// (Re)build the frequency + patch + limiter tables (§3/§8.3) from the
    /// current header at `output_rate` (= 2×core). On any table-derivation
    /// failure, `tables` is left `None` (→ upsample-only). Called at header
    /// accept / reset by [`bitstream::sbr_extension_data`].
    pub(super) fn rebuild_tables(&mut self, output_rate: u32) {
        self.tables = None;
        let Some(hdr) = self.header else {
            return;
        };
        let Ok(mut ft) = derive(&hdr, output_rate) else {
            return;
        };
        let Ok(patches) = patch_construction(&ft, output_rate) else {
            return;
        };
        let (borders, len) = patches.borders(ft.kx);
        if derive_limiter(&mut ft, &borders[..len]).is_err() {
            return;
        }
        self.kx = ft.kx;
        self.m = ft.m;
        self.tables = Some(Tables { ft, patches });
    }

    /// Re-initialise every per-channel DSP buffer on a header reset (or the
    /// first SBR frame): QMF delay lines, `x_low`/`y_prev` history, chirp,
    /// `EnvPrev`, and the HF-adjust rings/`index_noise`/`index_sine`/
    /// `t_env_num_env_old`/`s_index_carry` all start zeroed, and `hf_adj.reset`
    /// is armed so the first frame's smoothing ring is primed from its own
    /// `gain[0]`/`q_m[0]` (§9.4 reset re-prime) — requirement #2 of the task.
    pub(super) fn reset_channels(&mut self) {
        for c in &mut self.ch {
            *c = SbrChannel::default();
            c.hf_adj.reset = true;
        }
    }
}

/// Entry point for the AAC-LC pipeline (`formats::aac::decode_rdb`): decode the
/// SBR fill-element payload located at `[start_bit, end_bit)` in the
/// `raw_data_block` bytes `rdb`, using the already-reconstructed LC core PCM.
/// Constructs the `SbrPayload`/`IdAac` (both `pub(super)`-within-`sbr`, so the
/// parent module can't build them directly) and runs
/// [`bitstream::sbr_extension_data`]. Returns `Ok(true)` when 2048
/// samples/channel were written to `out` (SBR active), `Ok(false)` for the
/// pure-LC pass-through.
#[allow(clippy::too_many_arguments)]
pub(super) fn decode_fill_sbr(
    rdb: &[u8],
    start_bit: usize,
    end_bit: usize,
    crc: bool,
    id_cpe: bool,
    state: &mut SbrState,
    core_pcm: &[[f32; 1024]; 2],
    n_ch: usize,
    output_rate: u32,
    out: &mut SbrOutput,
) -> Result<bool, DecodeError> {
    let mut payload = SbrPayload {
        crc,
        bits: BitReader::new_at(rdb, start_bit),
        end_bit,
    };
    let id_aac = if id_cpe { IdAac::Cpe } else { IdAac::Sce };
    bitstream::sbr_extension_data(
        &mut payload,
        id_aac,
        state,
        core_pcm,
        n_ch,
        output_rate,
        out,
    )
}

/// 2×-rate PCM scratch a successful SBR frame writes into: 2 channels of 2048
/// samples (`sbr-reference.md` §0). `written` is the per-channel sample count
/// produced (2048 when SBR ran, 0 on upsample fallback that produced nothing).
pub(super) struct SbrOutput {
    pub(super) pcm: [[f32; 2048]; 2],
    pub(super) written: usize,
    /// Channels actually produced this frame: the input `n_ch` for the plain
    /// SBR path, or `2` when Parametric Stereo upmixed a mono core (R6). Set
    /// by [`drive`] every active frame; `decode_rdb` reads it for the
    /// interleave stride and the `AacDecoder` 1->2 channel promotion.
    pub(super) channels: usize,
}

impl Default for SbrOutput {
    fn default() -> Self {
        Self {
            pcm: [[0.0; 2048]; 2],
            written: 0,
            // Overwritten by `drive` before it is read on any active frame;
            // 2 is the safe fallback (matches the PCM buffer's channel width).
            channels: 2,
        }
    }
}

// ---------------------------------------------------------------------------
// sbr_data parse (§4-§6): grid / dtdf / invf / envelope / noise / add_harmonic
// per channel, honouring SCE vs CPE and CPE coupling.
// ---------------------------------------------------------------------------

/// One channel's parsed SBR side data for the current frame.
#[derive(Clone, Copy)]
struct ChParse {
    grid: SbrGrid,
    invf: [u8; 5],
    env_q: EnvQuant,
    add_harmonic: [u8; 48],
}

impl Default for ChParse {
    fn default() -> Self {
        Self {
            grid: SbrGrid::default(),
            invf: [0; 5],
            env_q: EnvQuant::default(),
            add_harmonic: [0; 48],
        }
    }
}

/// `bs_add_harmonic` (`sinusoidal_coding`, ISO Table 4.60): a 1-bit flag and,
/// if set, one bit per HI-RES band (`n[1]` = `n_high`). Returns the per-band
/// flags as hfadj's `[u8; 48]`.
fn read_add_harmonic(
    bits: &mut crate::formats::aac::bits::BitReader,
    end_bit: usize,
    n_high: usize,
) -> Result<[u8; 48], DecodeError> {
    let mut add = [0u8; 48];
    let flag = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)?;
    if flag == 1 {
        for slot in add.iter_mut().take(n_high.min(48)) {
            *slot = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)? as u8;
        }
    }
    Ok(add)
}

/// Parse `sbr_data(id_aac)` (§4-§6) into per-channel [`ChParse`], updating each
/// channel's cross-frame `EnvPrev`. Returns `(bs_coupling, [ch0, ch1])`.
/// Mirrors FFmpeg `read_sbr_single_channel_element` / `read_sbr_channel_pair_element`
/// bit-for-bit (including the non-coupled CPE env0,env1,noise0,noise1 order).
fn parse_sbr_data(
    payload: &mut SbrPayload,
    id_aac: &IdAac,
    ch_state: &mut [SbrChannel; 2],
    ft: &FreqTables,
    amp_res: u8,
    n_ch: usize,
) -> Result<(bool, [ChParse; 2]), DecodeError> {
    let end = payload.end_bit;
    let bits = &mut payload.bits;
    let n_q = ft.n_q;
    let n_high = ft.n_high as usize;

    let mut cp = [ChParse::default(), ChParse::default()];

    match id_aac {
        IdAac::Sce => {
            // bs_data_extra: 1 bit, + 4 reserved if set (SCE).
            if clamped_read(bits, end, 1).ok_or(DecodeError::Malformed)? == 1 {
                clamped_read(bits, end, 4).ok_or(DecodeError::Malformed)?;
            }
            let grid = sbr_grid(bits, end)?;
            let (df_env, df_noise) = read_dtdf(bits, end, grid.num_env, grid.num_noise)?;
            let invf = read_invf(bits, end, n_q)?;
            let env_q = read_env_noise(
                bits,
                end,
                &grid,
                &df_env,
                &df_noise,
                ft,
                amp_res,
                false,
                0,
                &mut ch_state[0].env_prev,
            )?;
            let add_harmonic = read_add_harmonic(bits, end, n_high)?;
            cp[0] = ChParse {
                grid,
                invf,
                env_q,
                add_harmonic,
            };
            Ok((false, cp))
        }
        IdAac::Cpe => {
            let _ = n_ch;
            // bs_data_extra: 1 bit, + 8 reserved if set (CPE).
            if clamped_read(bits, end, 1).ok_or(DecodeError::Malformed)? == 1 {
                clamped_read(bits, end, 8).ok_or(DecodeError::Malformed)?;
            }
            let coupling = clamped_read(bits, end, 1).ok_or(DecodeError::Malformed)? == 1;
            if coupling {
                let grid = sbr_grid(bits, end)?;
                let grid1 = grid; // ch1 copies ch0's grid
                let (df_env0, df_noise0) = read_dtdf(bits, end, grid.num_env, grid.num_noise)?;
                let (df_env1, df_noise1) = read_dtdf(bits, end, grid.num_env, grid.num_noise)?;
                let invf = read_invf(bits, end, n_q)?; // ch1 copies ch0's invf
                let env_q0 = read_env_noise(
                    bits,
                    end,
                    &grid,
                    &df_env0,
                    &df_noise0,
                    ft,
                    amp_res,
                    true,
                    0,
                    &mut ch_state[0].env_prev,
                )?;
                let env_q1 = read_env_noise(
                    bits,
                    end,
                    &grid1,
                    &df_env1,
                    &df_noise1,
                    ft,
                    amp_res,
                    true,
                    1,
                    &mut ch_state[1].env_prev,
                )?;
                cp[0] = ChParse {
                    grid,
                    invf,
                    env_q: env_q0,
                    add_harmonic: [0; 48],
                };
                cp[1] = ChParse {
                    grid: grid1,
                    invf,
                    env_q: env_q1,
                    add_harmonic: [0; 48],
                };
            } else {
                let grid0 = sbr_grid(bits, end)?;
                let grid1 = sbr_grid(bits, end)?;
                let (df_env0, df_noise0) = read_dtdf(bits, end, grid0.num_env, grid0.num_noise)?;
                let (df_env1, df_noise1) = read_dtdf(bits, end, grid1.num_env, grid1.num_noise)?;
                let invf0 = read_invf(bits, end, n_q)?;
                let invf1 = read_invf(bits, end, n_q)?;
                // Non-coupled CPE order: env0, env1, THEN noise0, noise1.
                let mut env_q0 = read_envelope(
                    bits,
                    end,
                    &grid0,
                    &df_env0,
                    ft,
                    amp_res,
                    false,
                    0,
                    &mut ch_state[0].env_prev,
                )?;
                let mut env_q1 = read_envelope(
                    bits,
                    end,
                    &grid1,
                    &df_env1,
                    ft,
                    amp_res,
                    false,
                    1,
                    &mut ch_state[1].env_prev,
                )?;
                read_noise(
                    bits,
                    end,
                    &grid0,
                    &df_noise0,
                    ft,
                    false,
                    0,
                    &mut env_q0,
                    &mut ch_state[0].env_prev,
                )?;
                read_noise(
                    bits,
                    end,
                    &grid1,
                    &df_noise1,
                    ft,
                    false,
                    1,
                    &mut env_q1,
                    &mut ch_state[1].env_prev,
                )?;
                cp[0] = ChParse {
                    grid: grid0,
                    invf: invf0,
                    env_q: env_q0,
                    add_harmonic: [0; 48],
                };
                cp[1] = ChParse {
                    grid: grid1,
                    invf: invf1,
                    env_q: env_q1,
                    add_harmonic: [0; 48],
                };
            }
            // add_harmonic per channel, after all env/noise.
            cp[0].add_harmonic = read_add_harmonic(bits, end, n_high)?;
            cp[1].add_harmonic = read_add_harmonic(bits, end, n_high)?;
            Ok((coupling, cp))
        }
    }
}

// ---------------------------------------------------------------------------
// §10 per-channel drive.
// ---------------------------------------------------------------------------

/// Upsample-only splice (`kx = 32`, `M = 0`): the 32 analysis bands pass
/// straight into synthesis bands `0..32` (delayed by `T_HFADJ`), the high 32
/// bands stay zero. Used when SBR isn't (yet) reconstructing a high band —
/// FFmpeg `sbr_turnoff`'s pure 2× upsample.
fn assemble_upsample(
    x_low: &[[Cpx; N_ANA]; MAX_SLOTS],
    x_out: &mut [[Cpx; N_SYN]; TIME_SLOTS_RATE],
) {
    for (s, out_slot) in x_out.iter_mut().enumerate() {
        *out_slot = [Cpx::default(); N_SYN];
        if let Some(low) = x_low.get(s + T_HFADJ) {
            out_slot[..N_ANA].copy_from_slice(low);
        }
    }
}

/// Entry point from [`bitstream::sbr_extension_data`]: run one SBR frame's §10
/// drive over `n_ch` channels, writing 2048 samples/channel into `out`.
/// Returns `true` when it produced 2×-rate output — always the case once SBR is
/// active, whether via full HF reconstruction or the upsample-only fallback (a
/// bad frame never shortens the output). Private to the `sbr` module — the
/// `bitstream` child calls it via `super::drive`; the AAC pipeline enters
/// through [`decode_fill_sbr`] instead.
/// One channel's §7-§10 QMF reconstruction: analysis QMF of the core PCM into
/// the low-band buffer (carrying the 8-slot LPC lookback), then full HF
/// reconstruction ([`run_channel_hf`]) or the upsample-only fallback, producing
/// this frame's 32 assembled output slots. Factored out of [`drive`] so the
/// plain per-channel loop and the PS mono path share one implementation
/// (bit-identical to the pre-PS inline form).
fn channel_x_out(
    cs: &mut SbrChannel,
    parsed: &Option<(bool, [ChParse; 2], [EnvLinear; 2])>,
    tables: Option<Tables>,
    header: Option<SbrHeader>,
    ch: usize,
    core_pcm: &[f32; 1024],
) -> [[Cpx; N_SYN]; TIME_SLOTS_RATE] {
    // (A) Analysis QMF of the LC core PCM → fresh 32 slots, placed into the
    // low-band buffer at [T_HFGEN, 40); slots [0,8) carry the previous frame's
    // last 8 analysis slots (§7.1 / §7.4 / §8 LPC lookback).
    let mut fresh = [[Cpx::default(); N_ANA]; TIME_SLOTS_RATE];
    qmf_analysis(core_pcm, &mut cs.qmf_ana, &mut fresh);
    cs.x_low.copy_within(TIME_SLOTS_RATE..MAX_SLOTS, 0);
    cs.x_low[T_HFGEN..MAX_SLOTS].copy_from_slice(&fresh);

    let mut x_out = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
    let full_ok = match (parsed, tables, header) {
        (Some((_coupling, cp, env_lin)), Some(t), Some(hdr)) => run_channel_hf(
            cs,
            &cp[ch],
            &env_lin[ch],
            &t.ft,
            &t.patches,
            &hdr,
            &mut x_out,
        ),
        _ => false,
    };
    if !full_ok {
        assemble_upsample(&cs.x_low, &mut x_out);
    }
    x_out
}

fn drive(
    payload: &mut SbrPayload,
    id_aac: IdAac,
    state: &mut SbrState,
    core_pcm: &[[f32; 1024]; 2],
    n_ch: usize,
    out: &mut SbrOutput,
) -> bool {
    let n_ch = n_ch.clamp(1, 2);
    let tables = state.tables;
    let header = state.header;

    // Parse sbr_data (best-effort) + dequant (§6.4). A parse/table failure
    // degrades this frame to upsample-only, but QMF analysis+synthesis still
    // runs so the output length stays 2048/ch.
    let parsed: Option<(bool, [ChParse; 2], [EnvLinear; 2])> = match (tables, header) {
        (Some(t), Some(hdr)) => {
            match parse_sbr_data(payload, &id_aac, &mut state.ch, &t.ft, hdr.amp_res, n_ch) {
                Ok((coupling, cp)) => {
                    let env_lin = if coupling {
                        let (l, r) = dequant_coupled(&cp[0].env_q, &cp[1].env_q);
                        [l, r]
                    } else {
                        [dequant_sce(&cp[0].env_q), dequant_sce(&cp[1].env_q)]
                    };
                    Some((coupling, cp, env_lin))
                }
                Err(_) => None,
            }
        }
        _ => None,
    };

    // R3: parse the trailing `bs_extension` loop NOW — after `sbr_data`, before
    // synthesis — so PS's per-frame parameters are decoded before `apply`
    // consumes the QMF matrix. This only advances `payload.bits` (synthesis
    // never reads it), so the non-PS path stays bit-identical.
    bitstream::parse_extensions(payload, state);

    // PS path: a mono HE-AACv2 core with an active PS header. Reconstruct ch0's
    // mono QMF matrix, upmix it to a stereo pair, then synthesize each through
    // its own persistent synthesis-QMF state (R4: ch[0] = left, ch[1] = right).
    if n_ch == 1 && state.ps.is_active() {
        let x_out = channel_x_out(&mut state.ch[0], &parsed, tables, header, 0, &core_pcm[0]);
        let mut xl = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        let mut xr = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        if state.ps.apply(&x_out, &mut xl, &mut xr).is_ok() {
            let (c0, c1) = state.ch.split_at_mut(1);
            qmf_synthesis(&xl, &mut c0[0].qmf_syn, &mut out.pcm[0]);
            qmf_synthesis(&xr, &mut c1[0].qmf_syn, &mut out.pcm[1]);
            out.channels = 2; // R6: mono core promoted to stereo.
        } else {
            // PS failed on a malformed frame: fall back to mono passthrough.
            qmf_synthesis(&x_out, &mut state.ch[0].qmf_syn, &mut out.pcm[0]);
            out.channels = 1;
        }
    } else {
        // Plain SBR (LC+SBR, or a stereo core): the existing per-channel loop,
        // unchanged and bit-identical. `ch` indexes `state.ch`, `core_pcm` and
        // `out.pcm` in lockstep, so a range loop is clearer than zipping them.
        #[allow(clippy::needless_range_loop)]
        for ch in 0..n_ch {
            let x_out = channel_x_out(
                &mut state.ch[ch],
                &parsed,
                tables,
                header,
                ch,
                &core_pcm[ch],
            );
            // (C) Synthesis QMF → 2048 real samples for this channel (§7.2).
            qmf_synthesis(&x_out, &mut state.ch[ch].qmf_syn, &mut out.pcm[ch]);
        }
        out.channels = n_ch;
    }

    out.written = 2048;
    true
}

/// The full HF reconstruction chain for one channel (§8-§10, steps 3-5),
/// producing `x_out`. Returns `false` (→ upsample-only) if `hf_adjustment`
/// rejects the frame.
#[allow(clippy::too_many_arguments)]
fn run_channel_hf(
    cs: &mut SbrChannel,
    cp: &ChParse,
    env_lin: &EnvLinear,
    ft: &FreqTables,
    patches: &Patches,
    hdr: &SbrHeader,
    x_out: &mut [[Cpx; N_SYN]; TIME_SLOTS_RATE],
) -> bool {
    // §8.1 inverse-filter LPC coefficients per low band.
    let mut alpha0 = [Cpx::default(); N_ANA];
    let mut alpha1 = [Cpx::default(); N_ANA];
    calc_prediction_coef(&cs.x_low, ft.k0 as usize, &mut alpha0, &mut alpha1);

    // §8.2 chirp factors (carries `cs.chirp` across frames).
    let mut bw = [0.0f32; 5];
    calc_chirp_factors(&cp.invf, ft.n_q as usize, &mut cs.chirp, &mut bw);

    // §8.4 HF generation → X_high.
    let mut x_high = [[Cpx::default(); N_SYN]; MAX_SLOTS];
    hf_generation(
        &cs.x_low,
        &alpha0,
        &alpha1,
        &bw,
        ft,
        patches,
        &cp.grid,
        &mut x_high,
    );

    // The previous frame's trailing envelope border governs this frame's
    // overlap carry (`i_Temp`), and `hf_adjustment` overwrites
    // `cs.hf_adj.t_env_num_env_old` with THIS frame's border, so read it first.
    let t_env_old = cs.hf_adj.t_env_num_env_old as usize;

    // §9 HF adjustment → Y (38 slots wide).
    let mut y_cur = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];
    if hf_adjustment(
        &x_high,
        env_lin,
        &cp.grid,
        ft,
        hdr,
        &cp.add_harmonic,
        &mut cs.hf_adj,
        &mut y_cur,
    )
    .is_err()
    {
        return false;
    }

    // §10 step 5: splice low + high for this frame's 32 output slots.
    let mut y32 = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
    y32.copy_from_slice(&y_cur[..TIME_SLOTS_RATE]);
    assemble(&cs.x_low, &y32, ft, x_out);

    // §10 overlap (`i_Temp`/`Ypos`): when the PREVIOUS frame's grid ran past
    // slot 16, its Y overlap slots [32, 32+i_Temp) are THIS frame's first
    // i_Temp output high-band slots (FFmpeg `sbr_x_gen`).
    let i_temp = (2 * t_env_old)
        .saturating_sub(TIME_SLOTS_RATE)
        .min(TIME_SLOTS_RATE);
    let kx = (ft.kx as usize).min(N_SYN);
    let kx_m = (ft.kx as usize + ft.m as usize).min(N_SYN);
    // `i` indexes `x_out` and (offset) `cs.y_prev` in lockstep — a plain range
    // loop is clearer than zipping two differently-offset slices.
    #[allow(clippy::needless_range_loop)]
    for i in 0..i_temp {
        if let Some(prev_slot) = cs.y_prev.get(i + TIME_SLOTS_RATE) {
            x_out[i][kx..kx_m].copy_from_slice(&prev_slot[kx..kx_m]);
        }
    }

    // Retain this frame's full Y for the next frame's overlap.
    cs.y_prev = y_cur;
    true
}
