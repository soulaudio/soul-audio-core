//! The shared per-frame Opus decode core ([`OpusFrameDecoder`]) plus the
//! raw-packet [`ConformanceDecoder`] wrapper used for RFC 8251 validation.
//!
//! [`OpusFrameDecoder`] owns the CELT + SILK sub-engines and the minimal
//! cross-frame state `opus_decode_frame` needs (`prev_mode` / `prev_redundancy`
//! / `final_range`) to decode ONE internal Opus frame with the full libopus
//! v1.5.2 `opus_decode_native` → `opus_decode_frame` flow — including the
//! **redundancy** and **mode-transition** handling (SILK↔CELT / CELT↔Hybrid
//! switches use a redundant CELT-only frame, `smooth_fade` cross-fades, and an
//! `OPUS_RESET_STATE` on a mode change). **Both** decode paths embed it:
//! * the Ogg [`super::OpusDecoder`] wraps it with the container-level concerns
//!   (Ogg demux, `OpusHead` pre-skip / end-trim / output gain, lost-packet PLC),
//!   applying the redundancy/transition logic on every mode switch of an
//!   adaptive `.opus` stream; and
//! * [`ConformanceDecoder`] wraps it with the opus_demo `.bit` packet framing.
//!
//! The RFC 8251 test vectors (`testvectorNN.bit`) are **not** Ogg-Opus — they
//! are opus_demo's raw framing (`[len][enc_final_range][packet]`, stripped by
//! the test harness). [`ConformanceDecoder`] decodes one raw Opus packet at a
//! time at a caller-supplied output channel count (1 **or** 2, independent of
//! any `OpusHead`) with **no** Ogg demux, `OpusHead`, pre-skip trim, or output
//! gain — matching `opus_demo -d` (which decodes with `skip = 0`, i.e. emits the
//! full untrimmed decoder output the reference `.dec` files were generated
//! from). It is `#[doc(hidden)] pub` purely so the `tests/opus_conformance.rs`
//! integration test can drive it; it is not part of the stable decoder API.

use super::celt::decoder::CeltDecoder;
use super::celt::mdct::WINDOW;
use super::range::RangeDecoder;
use super::silk::decoder::SilkDecoder;
use super::toc::{self, Config, Mode};

/// Largest per-channel CELT / Hybrid frame length (20 ms fullband, `120 << 3`).
const MAX_FRAME_N: usize = 960;
/// Largest per-channel frame length of *any* Opus mode — a 60 ms SILK-only
/// frame (`2880` samples at 48 kHz). SILK-only packets reach 40/60 ms, so the
/// per-frame float buffer must be sized for this, not the 20 ms CELT limit.
const MAX_PCM_N: usize = 2880;
/// 48 kHz samples one 20 ms internal SILK frame resamples to (mono, per frame).
const SILK_FRAME_N: usize = 960;
/// 2.5 ms at 48 kHz (libopus `F2_5`) — the redundancy / transition overlap.
const F2_5: usize = 120;
/// 5 ms at 48 kHz (libopus `F5`) — the redundant CELT frame length.
const F5: usize = 240;

/// The shared per-frame Opus decode core (libopus `opus_decode_frame`). Owns
/// the CELT + SILK sub-engines and the minimal cross-frame state needed
/// (`prev_mode` / `prev_redundancy` / `final_range`) to handle mode transitions
/// and redundancy. Embedded by BOTH the Ogg [`super::OpusDecoder`] and the
/// raw-packet [`ConformanceDecoder`], so the transition logic cannot diverge
/// between the two paths.
///
/// `pub(crate)` (not part of the public API): the Ogg decoder uses it directly;
/// the `tests/` integration crate reaches it only through the
/// [`ConformanceDecoder`] wrapper below.
pub(crate) struct OpusFrameDecoder {
    celt: CeltDecoder,
    silk: SilkDecoder,
    /// Output (API) channel count, 1 or 2 — fixed for the stream, independent
    /// of each packet's TOC stereo bit (libopus `st->channels`).
    channels: usize,
    /// Mode of the previous decoded frame (libopus `st->prev_mode`), or `None`
    /// before the first frame.
    prev_mode: Option<Mode>,
    /// Whether the previous frame ended with SILK→CELT redundancy still "in
    /// flight" (libopus `st->prev_redundancy`).
    prev_redundancy: bool,
    /// Range coder final state of the most recently decoded frame (libopus
    /// `st->rangeFinal = dec.rng ^ redundant_rng`). Exposed for the RFC 8251 /
    /// `opus_demo` per-packet final-range check — an exact match with the
    /// bitstream's stored `enc_final_range` proves the range decode is bit-exact.
    final_range: u32,
}

impl OpusFrameDecoder {
    /// A fresh decoder emitting `channels` (1 or 2) interleaved 48 kHz channels.
    #[must_use]
    pub(crate) fn new(channels: usize) -> Self {
        let channels = channels.clamp(1, 2);
        Self {
            celt: CeltDecoder::new(channels),
            silk: SilkDecoder::new(),
            channels,
            prev_mode: None,
            prev_redundancy: false,
            final_range: 0,
        }
    }

    /// The most recently decoded frame's range coder final state (see
    /// [`Self::final_range`]).
    #[must_use]
    pub(crate) fn final_range(&self) -> u32 {
        self.final_range
    }

    /// Decode one raw Opus packet into `out` (interleaved `i32` slots, `channels`
    /// per sample) at unity output gain, returning the number of `i32` written.
    /// `out` must hold at least `frame_count * frame_size_48k * channels` slots
    /// (a 120 ms stereo packet is `5760 * 2`). A malformed packet writes nothing
    /// and returns 0 (never panics). Mirrors `opus_decode_native`: split the
    /// packet into its constituent frames, then decode each with the full
    /// `opus_decode_frame` flow. Used by the raw-packet conformance path (the Ogg
    /// path drives [`Self::decode_frame`] directly so it can trim/gain each
    /// frame).
    pub(crate) fn decode_packet(&mut self, data: &[u8], out: &mut [i32]) -> usize {
        let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
        let (config, count) = match toc::frames(data, &mut frames) {
            Ok(v) => v,
            Err(_) => return 0,
        };
        let mut written = 0usize;
        for &frame in frames.iter().take(count) {
            written += self.decode_frame(frame, config, 1.0, &mut out[written..]);
        }
        written
    }

    /// Decode one internal Opus frame (`opus_decode_frame`) into `out`, applying
    /// the linear `output_gain` (the `OpusHead` gain on the Ogg path; `1.0` on
    /// the raw conformance path), returning the interleaved sample count.
    /// `config` is the packet TOC. Updates the cross-frame transition state
    /// (`prev_mode` / `prev_redundancy` / `final_range`) exactly as
    /// `opus_decode_frame` does, so successive frames — across a mode switch —
    /// decode with the correct redundancy / reset behavior regardless of which
    /// path calls this.
    pub(crate) fn decode_frame(
        &mut self,
        frame: &[u8],
        config: Config,
        output_gain: f32,
        out: &mut [i32],
    ) -> usize {
        let channels = self.channels;
        let n = usize::from(config.frame_size_48k); // per-channel samples
        let total = n * channels;
        if out.len() < total {
            return 0;
        }

        match config.mode {
            Mode::CeltOnly => self.decode_celt_only(frame, config, output_gain, out, n),
            Mode::SilkOnly | Mode::Hybrid => {
                self.decode_silk_or_hybrid(frame, config, output_gain, out, n)
            }
        }
    }

    /// CELT-only frame: a plain `celt_decode_with_ec` at start band 0.
    fn decode_celt_only(
        &mut self,
        frame: &[u8],
        config: Config,
        output_gain: f32,
        out: &mut [i32],
        n: usize,
    ) -> usize {
        let channels = self.channels;
        // CELT state must be discarded on a mode change (libopus
        // `OPUS_RESET_STATE` when `mode != prev_mode && prev_mode > 0 &&
        // !prev_redundancy`).
        if self.needs_celt_reset(Mode::CeltOnly) {
            self.celt = CeltDecoder::new(channels);
        }
        let mut tmp = [0i32; MAX_FRAME_N * 2];
        // CELT applies `output_gain` in its de-emphasis stage (matching the Ogg
        // decoder's original CELT-only arm); the raw path passes `1.0`.
        let produced = self.celt.decode_frame(frame, config, output_gain, &mut tmp);
        let take = produced.min(n * channels);
        out[..take].copy_from_slice(&tmp[..take]);
        self.final_range = if frame.len() <= 1 {
            0
        } else {
            self.celt.last_rng()
        };
        self.prev_mode = Some(Mode::CeltOnly);
        self.prev_redundancy = false;
        n * channels
    }

    /// SILK-only or Hybrid frame — a faithful port of `opus_decode_frame`'s
    /// non-CELT-only path (libopus v1.5.2): decode the SILK layer, parse the
    /// redundancy header, decode the redundant CELT-only frame (which primes the
    /// CELT decoder across a SILK↔CELT boundary), decode the Hybrid high band,
    /// then sum / cross-fade everything into one frame of interleaved PCM.
    fn decode_silk_or_hybrid(
        &mut self,
        frame: &[u8],
        config: Config,
        output_gain: f32,
        out: &mut [i32],
        n: usize,
    ) -> usize {
        let channels = self.channels;
        let hybrid = matches!(config.mode, Mode::Hybrid);
        let prev_mode = self.prev_mode;
        let prev_redundancy = self.prev_redundancy;

        let mut rd = RangeDecoder::new(frame);
        // (1) SILK layer (gain 1.0). SILK-only: the full output; Hybrid: the
        // 0-8 kHz band later summed with the CELT high band.
        let mut silk_out = [0i32; 3 * SILK_FRAME_N * 2];
        let silk_ret = self
            .silk
            .decode_packet(&mut rd, config, channels, &mut silk_out, 1.0);
        let silk_n = match silk_ret {
            Ok(v) => v / channels,
            Err(_) => {
                // PLC as silence (no zero-length packets appear in the RFC 8251
                // corpus, so this path is defensive only).
                out[..n * channels].fill(0);
                self.prev_mode = Some(config.mode);
                self.prev_redundancy = false;
                return n * channels;
            }
        };

        // Frame working buffer in the ±32768 float domain (libopus keeps `pcm`
        // as float and rounds to i16 only at the API boundary; we defer the
        // round so the redundancy cross-fades apply in full precision). Sized
        // for a 60 ms SILK-only frame (`MAX_PCM_N`), the largest any mode emits.
        let mut pcm = [0.0f32; MAX_PCM_N * 2];
        for i in 0..silk_n.min(n) * channels {
            pcm[i] = silk_out[i] as f32;
        }

        // (2) Redundancy header (only present when spare bits remain after SILK).
        let len = frame.len();
        let mut redundancy =
            !frame.is_empty() && rd.tell() + 17 + if hybrid { 20 } else { 0 } <= 8 * len as u32;
        if redundancy && hybrid {
            redundancy = rd.decode_bit_logp(12) != 0;
        }
        let mut redundancy_bytes = 0usize;
        let mut celt_to_silk = false;
        let mut len_main = len;
        if redundancy {
            celt_to_silk = rd.decode_bit_logp(1) != 0;
            redundancy_bytes = if hybrid {
                rd.dec_uint(256) as usize + 2
            } else {
                len.saturating_sub((rd.tell() as usize + 7) >> 3)
            };
            if len >= redundancy_bytes && (len - redundancy_bytes) * 8 >= rd.tell() as usize {
                len_main = len - redundancy_bytes;
                rd.shrink_storage(redundancy_bytes);
            } else {
                redundancy = false;
                redundancy_bytes = 0;
            }
        }
        let redundant_slice = frame
            .get(len_main..len_main + redundancy_bytes)
            .unwrap_or(&[]);

        // (3) Redundant frame for CELT→SILK — decoded *before* the main CELT
        // decode, using the current (previous-frame) CELT state (no reset).
        let mut redundant_audio = [0.0f32; F5 * 2];
        let mut redundant_rng = 0u32;
        if redundancy && celt_to_silk {
            redundant_rng =
                self.decode_redundant(redundant_slice, config, false, &mut redundant_audio);
        }

        if hybrid {
            // (4) Main CELT high band (start band 17), from the shortened coder.
            if !prev_redundancy && self.needs_celt_reset(Mode::Hybrid) {
                self.celt = CeltDecoder::new(channels);
            }
            let mut celt_sig = [0f32; MAX_FRAME_N * 2];
            let celt_n = self
                .celt
                .decode_hybrid_high(&mut rd, len_main, config, &mut celt_sig)
                / channels;
            for i in 0..n.min(silk_n).min(celt_n) * channels {
                pcm[i] += celt_sig[i];
            }
        } else if prev_mode == Some(Mode::Hybrid)
            && !(redundancy && celt_to_silk && prev_redundancy)
        {
            // (4') Hybrid→SILK: a 2.5 ms CELT silence frame fades out the CELT
            // MDCT tail, added onto the SILK low band's first 2.5 ms.
            let mut silence = [0.0f32; F2_5 * 2];
            self.decode_silence_fade(config, &mut silence);
            for i in 0..F2_5 * channels {
                pcm[i] += silence[i];
            }
        }

        // The main range coder is now fully consumed (SILK + redundancy header,
        // plus the Hybrid high band). Snapshot its final `rng` for the RFC 8251
        // per-packet final-range check before the separate redundant decode.
        let main_rng = rd.rng();

        // (5) Redundant frame for SILK→CELT — reset CELT, decode (this primes
        // the CELT state for the following CELT frame), cross-fade into the last
        // 2.5 ms.
        if redundancy && !celt_to_silk {
            redundant_rng =
                self.decode_redundant(redundant_slice, config, true, &mut redundant_audio);
            let off = (n - F2_5) * channels;
            smooth_fade_in_place(
                &mut pcm[off..off + F2_5 * channels],
                &redundant_audio[F2_5 * channels..2 * F2_5 * channels],
                channels,
            );
        }

        // (6) Redundant frame for CELT→SILK — overwrite the first 2.5 ms with the
        // redundant CELT audio, then cross-fade the next 2.5 ms into the SILK.
        if redundancy && celt_to_silk && (prev_mode != Some(Mode::SilkOnly) || prev_redundancy) {
            pcm[..F2_5 * channels].copy_from_slice(&redundant_audio[..F2_5 * channels]);
            smooth_fade_first(
                &redundant_audio[F2_5 * channels..2 * F2_5 * channels],
                &mut pcm[F2_5 * channels..2 * F2_5 * channels],
                channels,
            );
        }

        // (7) Apply the linear `OpusHead` output gain (libopus applies
        // `st->decode_gain` to the final float `pcm` before `float2int16`; the
        // raw conformance path passes `1.0`, an exact identity) and round the
        // float frame to interleaved i16 (libopus `float2int16`).
        for i in 0..n * channels {
            out[i] = round_clip_i16(pcm[i] * output_gain);
        }

        // libopus `st->rangeFinal = dec.rng ^ redundant_rng` (0 for a <=1-byte
        // DTX/silence frame).
        self.final_range = if len <= 1 {
            0
        } else {
            main_rng ^ redundant_rng
        };
        self.prev_mode = Some(config.mode);
        self.prev_redundancy = redundancy && !celt_to_silk;
        n * channels
    }

    /// Decode a 5 ms (`F5`) redundant CELT-only frame at start band 0 into the
    /// raw ±32768 float signal `redundant_audio` (libopus `opus_decode_frame`'s
    /// `redundant_audio`). When `reset` is set the CELT decoder is first cleared
    /// (`OPUS_RESET_STATE`, the SILK→CELT case); otherwise it decodes on the
    /// current CELT state (the CELT→SILK case). The synthesized state also
    /// **primes** the CELT decoder for the next real CELT frame.
    fn decode_redundant(
        &mut self,
        bytes: &[u8],
        config: Config,
        reset: bool,
        redundant_audio: &mut [f32],
    ) -> u32 {
        if bytes.is_empty() {
            return 0;
        }
        if reset {
            self.celt = CeltDecoder::new(self.channels);
        }
        let rconfig = Config {
            mode: Mode::CeltOnly,
            bandwidth: config.bandwidth,
            frame_size_48k: F5 as u16,
            stereo: config.stereo,
        };
        let mut rd = RangeDecoder::new(bytes);
        self.celt
            .decode_signal(&mut rd, bytes.len(), rconfig, 0, redundant_audio);
        self.celt.last_rng()
    }

    /// Decode the 2.5 ms (`F2_5`) all-silence CELT frame (`0xFF, 0xFF`) libopus
    /// uses to fade out the CELT MDCT on a Hybrid→SILK transition (start band 0).
    fn decode_silence_fade(&mut self, config: Config, out: &mut [f32]) {
        let rconfig = Config {
            mode: Mode::CeltOnly,
            bandwidth: config.bandwidth,
            frame_size_48k: F2_5 as u16,
            stereo: config.stereo,
        };
        let silence = [0xFFu8, 0xFF];
        let mut rd = RangeDecoder::new(&silence);
        self.celt.decode_signal(&mut rd, 2, rconfig, 0, out);
    }

    /// libopus: discard CELT state when the mode changes to/from a non-CELT
    /// mode boundary that requires it (`mode != prev_mode && prev_mode > 0 &&
    /// !prev_redundancy`), before decoding CELT for this frame.
    fn needs_celt_reset(&self, mode: Mode) -> bool {
        match self.prev_mode {
            Some(prev) => prev != mode && !self.prev_redundancy,
            None => false,
        }
    }
}

/// Raw-packet Opus decoder for RFC 8251 conformance — a thin wrapper over the
/// shared [`OpusFrameDecoder`] core that adds only the opus_demo `.bit` packet
/// framing (`opus_decode_native`: split a packet into its constituent frames and
/// decode each at unity gain) and the `final_range` accessor. It decodes with no
/// Ogg demux, `OpusHead`, pre-skip trim, or output gain (matching `opus_demo -d`
/// / `skip = 0`), so its output is byte-identical to the pre-unification
/// standalone decoder. `#[doc(hidden)] pub` purely so `tests/opus_conformance.rs`
/// can drive it; it is not part of the stable decoder API and is never
/// instantiated on the shipping Ogg path.
#[doc(hidden)]
pub struct ConformanceDecoder {
    core: OpusFrameDecoder,
}

impl ConformanceDecoder {
    /// A fresh decoder emitting `channels` (1 or 2) interleaved 48 kHz channels.
    #[must_use]
    pub fn new(channels: usize) -> Self {
        Self {
            core: OpusFrameDecoder::new(channels),
        }
    }

    /// The most recently decoded frame's range coder final state (libopus
    /// `st->rangeFinal`) — the RFC 8251 / `opus_demo` per-packet final-range
    /// check compares this against the bitstream's stored `enc_final_range`.
    #[must_use]
    pub fn final_range(&self) -> u32 {
        self.core.final_range()
    }

    /// Decode one raw Opus packet into `out` (interleaved `i32` slots, `channels`
    /// per sample) at unity gain, returning the number of `i32` written. A
    /// malformed packet writes nothing and returns 0 (never panics).
    pub fn decode_packet(&mut self, data: &[u8], out: &mut [i32]) -> usize {
        self.core.decode_packet(data, out)
    }
}

/// libopus `float2int16` of a single sample: round to nearest, clamp to i16.
fn round_clip_i16(x: f32) -> i32 {
    let r = if x >= 0.0 {
        (x + 0.5) as i32
    } else {
        (x - 0.5) as i32
    };
    r.clamp(-32768, 32767)
}

/// libopus `smooth_fade` with the CELT overlap window, in the SILK→CELT
/// orientation: `region` is both the first input (`in1`) and the output; `in2`
/// is the incoming (redundant CELT) signal it fades **towards**.
/// `region[i] = w·in2[i] + (1-w)·region[i]`, `w = window[i]²` (48 kHz → inc 1).
fn smooth_fade_in_place(region: &mut [f32], in2: &[f32], channels: usize) {
    let overlap = region.len() / channels;
    for (i, &win) in WINDOW.iter().enumerate().take(overlap) {
        let w = win * win;
        for c in 0..channels {
            let k = i * channels + c;
            region[k] = w * in2[k] + (1.0 - w) * region[k];
        }
    }
}

/// libopus `smooth_fade` in the CELT→SILK orientation: `region` is both the
/// second input (`in2`) and the output; `in1` is the outgoing (redundant CELT)
/// signal it fades **away from**. `region[i] = w·region[i] + (1-w)·in1[i]`.
fn smooth_fade_first(in1: &[f32], region: &mut [f32], channels: usize) {
    let overlap = region.len() / channels;
    for (i, &win) in WINDOW.iter().enumerate().take(overlap) {
        let w = win * win;
        for c in 0..channels {
            let k = i * channels + c;
            region[k] = w * region[k] + (1.0 - w) * in1[k];
        }
    }
}
