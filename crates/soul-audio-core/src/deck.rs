//! The playback **deck** — the producer-side player mechanism.
//!
//! A [`Deck`] plays a [`SeekableDecoder`] into a [`Sink`], resampling to the sink
//! rate and normalizing to a canonical scale. Volume is applied downstream, at
//! the [`Sink`]'s output stage (see [`Sink::set_volume`]), so changes are heard
//! immediately instead of lagging by the sink's output buffer depth. It holds
//! a **preload** buffer (`PRELOAD` slots) of already-opened decoders so the next
//! track is primed for a **gapless** swap. It is `no_std`, no-alloc, and sync:
//! [`pump`](Deck::pump) does one block of work each call and is driven by the
//! platform (a desktop thread, an embassy task) — see the playback-engine spec.
//!
//! Opening tracks (slow I/O) happens *off* this path; the deck only receives
//! already-opened decoders via [`load`](Deck::load) / [`preload`](Deck::preload).

use crate::decoder::{DecodeError, SeekableDecoder};
use crate::model::SampleFormat;
use crate::resample::{Quality, ResampleError, Resampler};
use crate::sink::{Sink, SinkError};

/// Canonical output scale: samples are normalized to signed 24-bit (`±2^23`),
/// which is exact in f32 and independent of the source bit depth — so tracks of
/// differing depth play back at a consistent level and the sink is depth-agnostic.
const CANONICAL_BITS: i32 = 24;
const CANONICAL_MAX: f32 = 8_388_607.0; // 2^23 - 1
const BLOCK: usize = 256; // resampler output frames per pump
const MAX_CH: usize = 8;
/// Seek de-click fade length, in milliseconds. On a seek the output is ramped up
/// over this long with a raised-cosine (smoothstep) gain, so the splice from the
/// just-cleared sink buffer to the first post-seek sample is a smooth rise from
/// zero rather than a full-level step — a step is a format-agnostic click, even on
/// bit-exact formats. 8 ms ≈ a few hundred frames at 44.1 kHz.
const FADE_MS: u32 = 8;
/// Input scratch capacity (frames) — covers the resampler's largest demand within
/// its supported downsample range at `BLOCK` output frames.
const IN_FIFO: usize = 2560;
/// Sane clamp for the combined ReplayGain, in dB, so an absurd caller value can
/// never produce a NaN/inf or a wild linear factor. `-60 dB` ≈ ×0.001, `+24 dB`
/// ≈ ×15.85 (which still hard-clips at the existing output clamp).
const RG_MIN_DB: f32 = -60.0;
const RG_MAX_DB: f32 = 24.0;

/// Transport state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeckState {
    /// No track / stopped.
    Stopped,
    /// Producing audio.
    Playing,
    /// Paused (retains position; `pump` is a no-op).
    Paused,
    /// The current track (and any preloads) ran out.
    Ended,
}

/// What one [`pump`](Deck::pump) did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Progress {
    /// Nothing to do (stopped/paused/no track).
    Idle,
    /// Wrote one block of audio.
    Played,
    /// The current track ended and a preloaded one took over (gapless).
    TrackChanged,
    /// The current track ended and no preload was queued.
    Ended,
}

/// Deck errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckError {
    /// A track's channel count is incompatible with the sink (and not mono→stereo).
    ChannelMismatch,
    /// Resampler construction failed for the track/sink rate pair.
    Resample(ResampleError),
    /// A decode error.
    Decode(DecodeError),
    /// A sink error.
    Sink(SinkError),
}

impl core::fmt::Display for DeckError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DeckError::ChannelMismatch => f.write_str("incompatible channel counts"),
            DeckError::Resample(e) => write!(f, "resample: {e}"),
            DeckError::Decode(e) => write!(f, "decode: {e}"),
            DeckError::Sink(e) => write!(f, "sink: {e}"),
        }
    }
}

impl core::error::Error for DeckError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            DeckError::Resample(e) => Some(e),
            DeckError::Decode(e) => Some(e),
            DeckError::Sink(e) => Some(e),
            DeckError::ChannelMismatch => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Map {
    Passthrough,
    MonoToStereo,
}

/// The currently-playing track and its per-track pipeline state.
struct Track<D> {
    decoder: D,
    resampler: Option<Resampler>,
    channels: usize,
    map: Map,
    /// Source→canonical scale (`2^(24-bits)`).
    norm: f32,
    rate: u32,
    /// Per-channel source frames consumed so far (playback position), counted
    /// from the first *playable* frame (after the gapless encoder-delay skip).
    pos: u64,
    total: Option<u64>,
    /// Gapless **playable length** — the number of post-skip frames to emit
    /// before stopping, from [`Decoder::playable_frames`]. `Some` trims the
    /// track's trailing encoder padding (so the next track abuts gaplessly);
    /// `None` plays to the decoder's end. Counted against `pos`.
    emit_cap: Option<u64>,
    /// Gapless encoder-delay frames still to discard before emitting output —
    /// the leading priming the decoder reports via [`Decoder::encoder_delay`].
    /// `0` once drained (or for a delay-free track). Draining it at each track's
    /// first block is what makes lossy track boundaries gapless.
    skip_remaining: u64,
    /// Decoder hit end of stream.
    eof: bool,
    /// Remaining silence-fed pumps to drain the resampler tail after `eof`.
    drain: usize,
    /// Seek de-click: output frames of fade-in still owed. Armed to `fade_len`
    /// (see [`Deck::fade_len`]) by [`seek`](Deck::seek); decremented one per
    /// emitted output frame in [`produce_block`](Deck::produce_block) until it
    /// reaches `0` (full scale). `0` for a freshly loaded/preloaded track, so a
    /// normal play/gapless swap never fades — only an explicit seek does.
    fade_remaining: u32,
}

/// A streaming player of [`SeekableDecoder`] tracks into a [`Sink`], with a
/// `PRELOAD`-deep gapless preload buffer.
pub struct Deck<K, D, const PRELOAD: usize> {
    sink: K,
    sink_rate: u32,
    sink_channels: usize,
    quality: Quality,
    current: Option<Track<D>>,
    /// Preloaded tracks are **already built** (channel-map + resampler), so an
    /// incompatible upcoming track is rejected at [`preload`](Deck::preload) time
    /// and the gapless swap in [`pump`](Deck::pump) cannot fail.
    preloads: [Option<Track<D>>; PRELOAD],
    preload_len: usize,
    state: DeckState,
    /// Seek de-click fade length in output frames: `round(FADE_MS * sink_rate /
    /// 1000)`. Fixed per sink; a seek arms `Track::fade_remaining` to this.
    fade_len: u32,
    /// Combined ReplayGain, in dB, applied to the deck's output (set by the
    /// caller via [`set_replay_gain`](Deck::set_replay_gain)). `0.0` is unity.
    rg_gain_db: f32,
    /// Cached linear multiplier `10^(rg_gain_db/20)` so the dB→linear conversion
    /// isn't recomputed per sample. `1.0` (exact) when `rg_gain_db == 0.0`.
    rg_linear: f32,
    in_buf: [i32; IN_FIFO * MAX_CH],
    rs_buf: [i32; BLOCK * MAX_CH],
    out_buf: [i32; BLOCK * MAX_CH],
}

impl<K: Sink, D: SeekableDecoder, const PRELOAD: usize> Deck<K, D, PRELOAD> {
    /// Create a deck feeding `sink`. `quality` selects the resampler tier.
    pub fn new(sink: K, quality: Quality) -> Self {
        let si = sink.stream_info();
        Self {
            sink_rate: si.sample_rate,
            sink_channels: si.channels as usize,
            sink,
            quality,
            current: None,
            preloads: core::array::from_fn(|_| None),
            preload_len: 0,
            state: DeckState::Stopped,
            // round(FADE_MS * sink_rate / 1000) with integer rounding (no f32::round
            // in no_std). `+ 500` is the round-half-up bias for the /1000 divide.
            fade_len: (FADE_MS * si.sample_rate + 500) / 1000,
            rg_gain_db: 0.0,
            rg_linear: 1.0,
            in_buf: [0; IN_FIFO * MAX_CH],
            rs_buf: [0; BLOCK * MAX_CH],
            out_buf: [0; BLOCK * MAX_CH],
        }
    }

    /// Build the per-track pipeline (channel map + resampler) for `decoder`.
    fn build_track(&self, decoder: D) -> Result<Track<D>, DeckError> {
        let si = decoder.stream_info();
        let ch = si.channels as usize;
        // Scratch buffers are sized for `MAX_CH`; the equal-rate path has no
        // resampler to reject wider streams, so guard here (else a >8-ch device +
        // source would overflow `in_buf`/`rs_buf`/`out_buf`).
        if ch > MAX_CH || self.sink_channels > MAX_CH {
            return Err(DeckError::ChannelMismatch);
        }
        let map = if ch == self.sink_channels {
            Map::Passthrough
        } else if ch == 1 && self.sink_channels == 2 {
            Map::MonoToStereo
        } else {
            return Err(DeckError::ChannelMismatch);
        };
        let resampler = if si.sample_rate == self.sink_rate {
            None
        } else {
            let r = Resampler::new(ch, si.sample_rate, self.sink_rate, 1.0, BLOCK, self.quality)
                .map_err(DeckError::Resample)?;
            if r.input_frames_next() > IN_FIFO {
                return Err(DeckError::Resample(ResampleError::RatioRangeTooWide));
            }
            Some(r)
        };
        let bits = match si.format {
            SampleFormat::Int { bits } | SampleFormat::Float { bits } => bits,
            SampleFormat::Dsd { .. } => return Err(DeckError::ChannelMismatch),
        };
        // Gapless: discard `encoder_delay` leading frames, then emit either the
        // decoder's declared playable length (`playable_frames` — trims the
        // trailing padding too) or, when it doesn't declare one, everything left
        // after the delay. `total` (the reported duration) follows the same:
        // the playable length if known, else the delay-trimmed stream length.
        let encoder_delay = decoder.encoder_delay();
        let emit_cap = decoder.playable_frames();
        let total = emit_cap.or_else(|| {
            decoder
                .total_frames()
                .map(|t| t.saturating_sub(encoder_delay))
        });
        Ok(Track {
            total,
            decoder,
            resampler,
            channels: ch,
            map,
            norm: libm_exp2(f32::from(CANONICAL_BITS as i16 - i16::from(bits))),
            rate: si.sample_rate,
            pos: 0,
            emit_cap,
            skip_remaining: encoder_delay,
            eof: false,
            drain: 0,
            fade_remaining: 0,
        })
    }

    /// Play `decoder` immediately, discarding the current track and any preloads.
    ///
    /// # Errors
    /// [`DeckError::ChannelMismatch`] / [`DeckError::Resample`].
    pub fn load(&mut self, decoder: D) -> Result<(), DeckError> {
        let track = self.build_track(decoder)?;
        self.clear_preloads();
        self.current = Some(track);
        self.state = DeckState::Playing;
        Ok(())
    }

    /// Stage `decoder` in the next free preload slot for a gapless swap. The
    /// track is built (and thus validated) now, so an incompatible track is
    /// rejected here rather than at the swap.
    ///
    /// # Errors
    /// [`DeckError::Sink`] (`Unsupported`) if no preload slot is free;
    /// [`DeckError::ChannelMismatch`] / [`DeckError::Resample`] if the track is
    /// incompatible with the sink.
    pub fn preload(&mut self, decoder: D) -> Result<(), DeckError> {
        if self.preload_len >= PRELOAD {
            return Err(DeckError::Sink(SinkError::Unsupported));
        }
        let track = self.build_track(decoder)?;
        self.preloads[self.preload_len] = Some(track);
        self.preload_len += 1;
        Ok(())
    }

    /// Number of preload slots currently free.
    #[must_use]
    pub fn free_preload_slots(&self) -> usize {
        PRELOAD - self.preload_len
    }

    /// Immediately promote the front preloaded track to current (a hard skip that
    /// reuses the already-primed track). Returns `true` if a preload was
    /// available; otherwise the deck is unchanged. Infallible — the track was
    /// already built at [`preload`](Deck::preload) time.
    pub fn advance(&mut self) -> bool {
        if self.preload_len == 0 {
            return false;
        }
        self.current = Some(self.pop_preload());
        self.state = DeckState::Playing;
        true
    }

    /// Drop all staged preloads.
    pub fn clear_preloads(&mut self) {
        for slot in &mut self.preloads {
            *slot = None;
        }
        self.preload_len = 0;
    }

    fn pop_preload(&mut self) -> Track<D> {
        let track = self.preloads[0].take().expect("preload present");
        for i in 1..self.preload_len {
            self.preloads[i - 1] = self.preloads[i].take();
        }
        self.preload_len -= 1;
        track
    }

    /// Resume/begin playback.
    pub fn play(&mut self) {
        if self.current.is_some() {
            self.state = DeckState::Playing;
        }
    }

    /// Pause (retains position). `pump` becomes a no-op; the sink plays out what is
    /// already buffered and then silence.
    pub fn pause(&mut self) {
        if self.state == DeckState::Playing {
            self.state = DeckState::Paused;
        }
    }

    /// Stop and clear the current track and preloads.
    pub fn stop(&mut self) {
        self.current = None;
        self.clear_preloads();
        self.state = DeckState::Stopped;
    }

    /// Forward the target volume to the sink (0.0..=1.0), where it is applied at
    /// the output stage so the change is heard immediately.
    pub fn set_volume(&mut self, volume: f32) {
        self.sink.set_volume(volume.clamp(0.0, 1.0));
    }

    /// Set the combined ReplayGain, in dB, applied to the deck's output so
    /// playback can be loudness-normalized. `0.0` is unity (an exact,
    /// bit-identical passthrough). The caller (backend) is responsible for
    /// computing this value — picking track vs album gain, adding any preamp,
    /// reconciling the tag's reference level to a target — and setting it at a
    /// track boundary; the deck just applies one constant gain, so there is no
    /// per-sample ramp and no click to manage. `gain_db` is clamped to
    /// `[-60, 24]` dB. A positive gain that pushes a sample past full scale is
    /// still hard-clipped by the output clamp (peak-limited gains avoid it).
    pub fn set_replay_gain(&mut self, gain_db: f32) {
        let db = gain_db.clamp(RG_MIN_DB, RG_MAX_DB);
        self.rg_gain_db = db;
        // Keep unity exact (1.0) so a 0 dB gain is a bit-identical passthrough.
        self.rg_linear = if db == 0.0 { 1.0 } else { pow10_db(db) };
    }

    /// The currently-applied combined ReplayGain, in dB (`0.0` = unity).
    #[must_use]
    pub fn replay_gain_db(&self) -> f32 {
        self.rg_gain_db
    }

    /// Seek the current track to per-channel `frame`.
    ///
    /// # Errors
    /// [`DeckError::Decode`] (incl. `Unsupported`) if the track can't seek.
    pub fn seek(&mut self, frame: u64) -> Result<u64, DeckError> {
        let fade_len = self.fade_len;
        let track = self
            .current
            .as_mut()
            .ok_or(DeckError::Decode(DecodeError::Unsupported))?;
        let landed = track.decoder.seek_to(frame).map_err(DeckError::Decode)?;
        track.pos = landed;
        track.eof = false;
        track.drain = 0;
        if let Some(r) = track.resampler.as_mut() {
            r.reset();
        }
        // Arm the seek de-click fade-in (re-arming overwrites any in-flight fade),
        // so `produce_block` ramps the first post-seek frames up from zero.
        track.fade_remaining = fade_len;
        // The `track` borrow of `self.current` ends above; drop the sink's
        // already-buffered pre-seek audio so the landing point is heard promptly
        // instead of after the old buffer drains.
        self.sink.discard().map_err(DeckError::Sink)?;
        Ok(landed)
    }

    /// Current transport state.
    #[must_use]
    pub fn state(&self) -> DeckState {
        self.state
    }

    /// Borrow the output sink (e.g. to query its stream info).
    #[must_use]
    pub fn sink(&self) -> &K {
        &self.sink
    }

    /// Mutably borrow the output sink — the driver's hook for sink-side controls
    /// such as pausing/resuming the concrete output device.
    pub fn sink_mut(&mut self) -> &mut K {
        &mut self.sink
    }

    /// Per-channel source-frame position of the current track.
    #[must_use]
    pub fn position_frames(&self) -> u64 {
        self.current.as_ref().map_or(0, |t| t.pos)
    }

    /// Total per-channel frames of the current track, if known.
    #[must_use]
    pub fn duration_frames(&self) -> Option<u64> {
        self.current.as_ref().and_then(|t| t.total)
    }

    /// Source sample rate of the current track.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.current.as_ref().map_or(0, |t| t.rate)
    }

    /// Do one block of playback: decode → resample → normalize → volume → write.
    ///
    /// # Errors
    /// Propagates decode / resample / sink errors.
    pub fn pump(&mut self) -> Result<Progress, DeckError> {
        if self.state != DeckState::Playing {
            return Ok(Progress::Idle);
        }
        if self.current.is_none() {
            self.state = DeckState::Ended;
            return Ok(Progress::Ended);
        }
        let done = self.produce_block()?;
        if !done {
            return Ok(Progress::Played);
        }
        // Current track finished — swap to a preload (gapless) or end. The swap
        // is infallible: preloaded tracks were built at `preload` time.
        if self.preload_len > 0 {
            self.current = Some(self.pop_preload());
            Ok(Progress::TrackChanged)
        } else {
            self.current = None;
            self.state = DeckState::Ended;
            Ok(Progress::Ended)
        }
    }

    /// Discard the current track's gapless encoder-delay (leading priming)
    /// source frames before any output is produced. A no-op after the first
    /// block, for a delay-free track, or once the decoder hits EOF. Decodes into
    /// `in_buf` and throws the frames away, so the first emitted sample is the
    /// track's first *playable* sample — which is what makes a lossy track
    /// boundary (where the next track was preloaded) gapless.
    fn drain_encoder_delay(&mut self) -> Result<(), DeckError> {
        let track = self.current.as_mut().unwrap();
        if track.skip_remaining == 0 || track.eof {
            return Ok(());
        }
        let ch = track.channels;
        while track.skip_remaining > 0 && !track.eof {
            // Bounded by `in_buf`'s frame capacity; a large delay drains over
            // several decode calls.
            let want = track.skip_remaining.min(IN_FIFO as u64) as usize;
            let n = track
                .decoder
                .decode_into(&mut self.in_buf[..want * ch])
                .map_err(DeckError::Decode)?;
            if n == 0 {
                // Track shorter than its declared delay (degenerate/truncated):
                // nothing left to play.
                track.eof = true;
                break;
            }
            track.skip_remaining -= (n / ch) as u64;
        }
        Ok(())
    }

    /// Produce and write one output block from the current track. Returns `true`
    /// when the track has fully finished (decoder EOF + resampler tail drained).
    fn produce_block(&mut self) -> Result<bool, DeckError> {
        self.drain_encoder_delay()?;
        let sink_ch = self.sink_channels;
        // Decide the resampled/plain source block and how many frames it yields.
        let (out_frames, finished) = if self.current.as_ref().unwrap().resampler.is_some() {
            self.block_resampled()?
        } else {
            self.block_plain()?
        };
        // Normalize + channel-map rs_buf → out_buf, then write. Volume is applied
        // downstream at the sink's output stage (see `Sink::set_volume`). A seek
        // fade-in (if armed) scales each frame by a rising smoothstep gain; copied
        // out so the per-frame decrement doesn't need a mutable track borrow here.
        let (src_ch, g, map, mut fade_remaining) = {
            let track = self.current.as_ref().unwrap();
            (track.channels, track.norm, track.map, track.fade_remaining)
        };
        // Fold the constant ReplayGain linear factor into the per-track
        // normalization scale once (it's constant within a track), so the
        // per-sample output cost is unchanged. Applied BEFORE the existing
        // output clamp, so a boost past full scale still hard-clips there.
        let g = g * self.rg_linear;
        let fade_len = self.fade_len;
        for f in 0..out_frames {
            // Seek de-click gain for this output frame: smoothstep `t*t*(3 - 2*t)`
            // with `t = fade_pos / fade_len ∈ [0,1]` — a raised-cosine-like S-curve
            // needing no trig (no_std-safe) whose zero-slope endpoints add no click
            // of their own (unlike a linear ramp). `t=0` → 0 at the splice, `t→1`
            // → full scale. One gain for all channels keeps the image stable.
            let fade = if fade_remaining > 0 {
                let t = (fade_len - fade_remaining) as f32 / fade_len as f32;
                fade_remaining -= 1;
                t * t * (3.0 - 2.0 * t)
            } else {
                1.0
            };
            for oc in 0..sink_ch {
                let sc = match map {
                    Map::Passthrough => oc,
                    Map::MonoToStereo => 0,
                };
                let s = self.rs_buf[f * src_ch + sc] as f32 * g * fade;
                // Round to nearest without `f32::round` (std-only in no_std).
                let r = if s >= 0.0 { s + 0.5 } else { s - 0.5 };
                self.out_buf[f * sink_ch + oc] =
                    r.clamp(-CANONICAL_MAX - 1.0, CANONICAL_MAX) as i32;
            }
        }
        // Persist the fade progress for the next block (a fade can span several).
        self.current.as_mut().unwrap().fade_remaining = fade_remaining;
        self.sink
            .write(&self.out_buf[..out_frames * sink_ch])
            .map_err(DeckError::Sink)?;
        Ok(finished)
    }

    /// Resampler path: fill `rs_buf` with one `BLOCK`-frame resampled block.
    fn block_resampled(&mut self) -> Result<(usize, bool), DeckError> {
        let track = self.current.as_mut().unwrap();
        let ch = track.channels;
        let need = track.resampler.as_ref().unwrap().input_frames_next();
        // Fill `need` input frames (zero-padded past EOF).
        let mut have = 0usize;
        if !track.eof {
            while have < need {
                let n = track
                    .decoder
                    .decode_into(&mut self.in_buf[have * ch..need * ch])
                    .map_err(DeckError::Decode)?;
                if n == 0 {
                    track.eof = true;
                    let lat = track.resampler.as_ref().unwrap().latency();
                    track.drain = lat / BLOCK + 2;
                    break;
                }
                have += n / ch;
            }
        }
        // Gapless trailing trim: never feed past the playable length. Clamp the
        // real frames to the remaining room and treat reaching the cap like EOF
        // so the resampler drains its tail (then zero-pad as the EOF path does).
        if let Some(cap) = track.emit_cap {
            let room = cap.saturating_sub(track.pos);
            if have as u64 > room {
                have = room as usize;
            }
            if !track.eof && track.pos + have as u64 >= cap {
                track.eof = true;
                let lat = track.resampler.as_ref().unwrap().latency();
                track.drain = lat / BLOCK + 2;
            }
        }
        for s in &mut self.in_buf[have * ch..need * ch] {
            *s = 0;
        }
        track.pos += have as u64;
        track
            .resampler
            .as_mut()
            .unwrap()
            .process(&self.in_buf[..need * ch], &mut self.rs_buf[..BLOCK * ch])
            .map_err(DeckError::Resample)?;
        let finished = if track.eof {
            track.drain = track.drain.saturating_sub(1);
            track.drain == 0
        } else {
            false
        };
        Ok((BLOCK, finished))
    }

    /// Pass-through path (rates equal): decode up to `BLOCK` frames into `rs_buf`.
    fn block_plain(&mut self) -> Result<(usize, bool), DeckError> {
        let track = self.current.as_mut().unwrap();
        let ch = track.channels;
        let mut have = 0usize;
        while have < BLOCK {
            let n = track
                .decoder
                .decode_into(&mut self.rs_buf[have * ch..BLOCK * ch])
                .map_err(DeckError::Decode)?;
            if n == 0 {
                break;
            }
            have += n / ch;
        }
        let mut finished = have < BLOCK; // decoder end of stream
        // Gapless trailing trim: stop at the playable length, dropping the
        // trailing padding (no resampler here, so finishing is immediate).
        if let Some(cap) = track.emit_cap {
            let room = cap.saturating_sub(track.pos);
            if have as u64 >= room {
                have = room as usize;
                finished = true;
            }
        }
        track.pos += have as u64;
        Ok((have, finished))
    }
}

/// `2^x` for small integer-ish `x` without `std`/`libm` — used only for the
/// per-track normalization scale (`x` in `-8..=23`).
fn libm_exp2(x: f32) -> f32 {
    // x is always an integer here (24 - bits).
    let n = x as i32;
    if n >= 0 {
        (1u64 << n) as f32
    } else {
        1.0 / (1u64 << (-n)) as f32
    }
}

/// `floor(x)` without `f32::floor` (std-only): a truncating `as i32` cast rounds
/// toward zero, so only a negative non-integer needs the −1 correction.
fn floor_i32(x: f32) -> i32 {
    let t = x as i32;
    if (t as f32) > x { t - 1 } else { t }
}

/// `10^(db/20)` — the linear amplitude multiplier for a gain in decibels —
/// without `f32::powf`/`exp2` (std-only; `libm` is banned). Uses the identity
/// `10^(db/20) = 2^(db/20 · log2(10))`: the exponent is split into its integer
/// part (built by direct IEEE-754 exponent-field construction, exact for any
/// in-range exponent) and a fractional part in `[0,1)` from a Taylor series of
/// `2^f = e^{f·ln2}` (coefficients `(ln2)^k / k!`), accurate to under `1e-5`
/// over the unit interval — the `exp2`/bit-trick house style of [`libm_exp2`]
/// and `formats::aac::sbr`'s `pow2`. `db` is pre-clamped by its only caller to
/// `[-60, 24]`; the exponent is clamped again defensively so the exponent-bit
/// construction can never go out of range.
fn pow10_db(db: f32) -> f32 {
    use core::f32::consts::{LN_2, LOG2_10};
    let y = ((db / 20.0) * LOG2_10).clamp(-40.0, 40.0);
    let ip = floor_i32(y);
    let f = y - ip as f32; // [0, 1)
    // 2^f via the truncated Taylor series of e^{f·ln2} (Horner form); the
    // coefficients are (ln2)^k / k! for k = 1..=7.
    let p = 1.0
        + f * (LN_2
            + f * (0.240_226_5
                + f * (0.055_504_11
                    + f * (0.009_618_129
                        + f * (0.001_333_356 + f * (0.000_154_035 + f * 0.000_015_253))))));
    let two_ip = f32::from_bits(((ip + 127) as u32) << 23); // 2^ip
    two_ip * p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::Decoder;
    use crate::model::StreamInfo;
    extern crate alloc;
    use alloc::vec::Vec;

    /// A decoder emitting `frames` of a constant per-channel `value` at a given
    /// rate/channels/bits. Honors any output-buffer size; seekable; known length.
    struct ConstDecoder {
        rate: u32,
        ch: usize,
        bits: u8,
        value: i32,
        frames: u64,
        pos: u64,
    }
    impl Decoder for ConstDecoder {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: self.rate,
                channels: self.ch as u16,
                format: SampleFormat::Int { bits: self.bits },
            }
        }
        fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
            let want = ((out.len() / self.ch) as u64).min(self.frames - self.pos) as usize;
            for s in &mut out[..want * self.ch] {
                *s = self.value;
            }
            self.pos += want as u64;
            Ok(want * self.ch)
        }
    }
    impl SeekableDecoder for ConstDecoder {
        fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
            self.pos = sample.min(self.frames);
            Ok(self.pos)
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.frames)
        }
    }

    /// Emits a per-channel ramp — sample value == the source frame index — at
    /// 48 kHz, 24-bit (so the deck's normalization is the identity: norm
    /// `2^(24-24) = 1`), reporting a gapless `encoder_delay`. Lets a test see
    /// exactly which source frames the deck emitted: a value of `k` in the sink
    /// is source frame `k`, so a skipped leading `delay` shows up as the first
    /// emitted value being `delay`.
    struct RampDecoder {
        ch: usize,
        frames: u64,
        pos: u64,
        delay: u64,
        playable: Option<u64>,
    }
    impl Decoder for RampDecoder {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: 48_000,
                channels: self.ch as u16,
                format: SampleFormat::Int { bits: 24 },
            }
        }
        fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
            let want = ((out.len() / self.ch) as u64).min(self.frames - self.pos) as usize;
            for f in 0..want {
                for c in 0..self.ch {
                    out[f * self.ch + c] = (self.pos + f as u64) as i32;
                }
            }
            self.pos += want as u64;
            Ok(want * self.ch)
        }
        fn encoder_delay(&self) -> u64 {
            self.delay
        }
        fn playable_frames(&self) -> Option<u64> {
            self.playable
        }
    }
    impl SeekableDecoder for RampDecoder {
        fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
            self.pos = sample.min(self.frames);
            Ok(self.pos)
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.frames)
        }
    }

    struct MockSink {
        rate: u32,
        ch: usize,
        written: Vec<i32>,
        last_volume: Option<f32>,
    }
    impl Sink for MockSink {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: self.rate,
                channels: self.ch as u16,
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
        fn set_volume(&mut self, volume: f32) {
            self.last_volume = Some(volume);
        }
    }

    fn const_dec(rate: u32, ch: usize, value: i32, frames: u64) -> ConstDecoder {
        ConstDecoder {
            rate,
            ch,
            bits: 16,
            value,
            frames,
            pos: 0,
        }
    }

    /// Pump to `Ended`, returning (all output, count of TrackChanged events).
    fn run<K: Sink, D: SeekableDecoder, const P: usize>(deck: &mut Deck<K, D, P>) -> usize {
        let mut swaps = 0;
        for _ in 0..100_000 {
            match deck.pump().unwrap() {
                Progress::Ended | Progress::Idle => break,
                Progress::TrackChanged => swaps += 1,
                Progress::Played => {}
            }
        }
        swaps
    }

    #[test]
    fn passthrough_normalizes_to_24bit() {
        // 16-bit source value 100 → canonical 24-bit = 100 << 8 = 25600.
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.load(const_dec(48_000, 1, 100, 1000)).unwrap();
        run(&mut deck);
        assert_eq!(deck.sink.written.len(), 1000);
        assert!(deck.sink.written.iter().all(|&s| s == 25_600));
        assert_eq!(deck.state(), DeckState::Ended);
    }

    #[test]
    fn gapless_encoder_delay_trims_leading_priming() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        // 600-frame ramp with a 100-frame gapless encoder delay.
        deck.load(RampDecoder {
            ch: 1,
            frames: 600,
            pos: 0,
            delay: 100,
            playable: None,
        })
        .unwrap();
        // Duration reflects the trimmed (playable) length, not the full stream.
        assert_eq!(deck.duration_frames(), Some(500));
        run(&mut deck);
        let out = &deck.sink.written;
        assert_eq!(
            out.len(),
            500,
            "600 frames minus the 100-frame encoder delay"
        );
        assert_eq!(
            out[0], 100,
            "first emitted sample is source frame 100 — the 100-frame priming was skipped"
        );
        assert_eq!(
            out[499], 599,
            "last emitted sample is the final source frame"
        );
    }

    #[test]
    fn gapless_trailing_padding_is_trimmed() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        // 600-frame ramp: 100-frame leading delay, playable length 400 → the
        // 100 trailing frames (500..=599) are the padding and must be dropped.
        deck.load(RampDecoder {
            ch: 1,
            frames: 600,
            pos: 0,
            delay: 100,
            playable: Some(400),
        })
        .unwrap();
        assert_eq!(
            deck.duration_frames(),
            Some(400),
            "duration is the playable length"
        );
        run(&mut deck);
        let out = &deck.sink.written;
        assert_eq!(
            out.len(),
            400,
            "emit exactly the playable length (100 trailing dropped)"
        );
        assert_eq!(out[0], 100, "starts after the leading skip");
        assert_eq!(
            out[399], 499,
            "last emitted frame is 499 — source frames 500..=599 (trailing padding) dropped"
        );
    }

    #[test]
    fn gapless_swap_trims_both_ends() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        // A: delay 0, 300 frames, playable 250 → trailing 50 trimmed.
        // B: delay 50, 200 frames, playable 100 → leading 50 + trailing 50 trimmed.
        deck.load(RampDecoder {
            ch: 1,
            frames: 300,
            pos: 0,
            delay: 0,
            playable: Some(250),
        })
        .unwrap();
        deck.preload(RampDecoder {
            ch: 1,
            frames: 200,
            pos: 0,
            delay: 50,
            playable: Some(100),
        })
        .unwrap();
        let swaps = run(&mut deck);
        assert_eq!(swaps, 1, "one gapless swap");
        let out = &deck.sink.written;
        // A: 250 (0..=249); B: 100 (50..=149). A's tail AND B's head trimmed.
        assert_eq!(
            out.len(),
            350,
            "A playable (250) + B playable (100), no gap"
        );
        assert_eq!(
            out[249], 249,
            "A's last playable frame (its 50-frame tail trimmed)"
        );
        assert_eq!(out[250], 50, "B resumes at its first playable frame");
        assert_eq!(out[349], 149, "B's last playable frame (its tail trimmed)");
    }

    #[test]
    fn gapless_swap_trims_next_track_priming() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        // A: no delay, 300 frames (ramp 0..=299). B: 50-frame priming, 200 frames.
        deck.load(RampDecoder {
            ch: 1,
            frames: 300,
            pos: 0,
            delay: 0,
            playable: None,
        })
        .unwrap();
        deck.preload(RampDecoder {
            ch: 1,
            frames: 200,
            pos: 0,
            delay: 50,
            playable: None,
        })
        .unwrap();
        let swaps = run(&mut deck);
        assert_eq!(swaps, 1, "one gapless swap");
        let out = &deck.sink.written;
        // A: 300 frames (0..=299); B: 200-50 = 150 frames (50..=199). No gap.
        assert_eq!(
            out.len(),
            450,
            "A (300) + B trimmed (150), no silence between"
        );
        assert_eq!(out[299], 299, "A's final frame");
        assert_eq!(
            out[300], 50,
            "B resumes at its first playable frame (50) — its priming was trimmed at the swap"
        );
    }

    #[test]
    fn gapless_preload_swaps() {
        // Volume is no longer applied in the deck (it's forwarded to the sink's
        // output stage — see `set_volume_forwards_to_sink`), so this only tests
        // the gapless preload swap itself: full-scale normalized output.
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.load(const_dec(48_000, 1, 100, 500)).unwrap(); // A → 25600
        assert_eq!(deck.free_preload_slots(), 1);
        deck.preload(const_dec(48_000, 1, 60, 500)).unwrap(); // B → 15360
        assert_eq!(deck.free_preload_slots(), 0);
        let swaps = run(&mut deck);
        assert_eq!(swaps, 1, "one gapless swap");
        let out = &deck.sink.written;
        assert_eq!(out.len(), 1000, "both tracks, no gap");
        assert_eq!(out[300], 25_600, "A at full scale");
        assert_eq!(out[900], 15_360, "B at full scale");
    }

    #[test]
    fn set_volume_forwards_to_sink() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, ConstDecoder, 1> = Deck::new(sink, Quality::Fast);
        deck.set_volume(0.5);
        assert_eq!(deck.sink().last_volume, Some(0.5));
        deck.set_volume(1.5);
        assert_eq!(deck.sink().last_volume, Some(1.0), "clamped to 1.0");
        deck.set_volume(-0.5);
        assert_eq!(deck.sink().last_volume, Some(0.0), "clamped to 0.0");
    }

    #[test]
    fn advance_promotes_preload_immediately() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.load(const_dec(48_000, 1, 100, 10_000)).unwrap();
        deck.preload(const_dec(48_000, 1, 60, 500)).unwrap();
        deck.pump().unwrap(); // play some of A
        assert!(deck.advance(), "had a preload");
        assert_eq!(deck.free_preload_slots(), 1);
        // Now playing B (value 60 → 15360); drain to end.
        run(&mut deck);
        assert!(
            deck.sink
                .written
                .iter()
                .rev()
                .take(100)
                .all(|&s| s == 15_360)
        );
    }

    #[test]
    fn mono_source_to_stereo_sink_duplicates() {
        let sink = MockSink {
            rate: 48_000,
            ch: 2,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.load(const_dec(48_000, 1, 100, 300)).unwrap();
        run(&mut deck);
        assert_eq!(deck.sink.written.len(), 300 * 2);
        assert!(deck.sink.written.iter().all(|&s| s == 25_600));
    }

    #[test]
    fn rate_conversion_produces_scaled_nonsilent_output() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Hq);
        deck.load(const_dec(44_100, 1, 100, 4000)).unwrap();
        run(&mut deck);
        let n = deck.sink.written.len();
        let expect = (4000.0 * 48_000.0 / 44_100.0) as usize;
        assert!(
            n.abs_diff(expect) <= 3 * BLOCK,
            "resampled len {n} vs ~{expect}"
        );
        // Constant input → ~constant 25600 output (DC preserved through resample).
        assert!((deck.sink.written[500] - 25_600).abs() <= 2);
    }

    #[test]
    fn transport_pause_stop_and_seek() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.load(const_dec(48_000, 1, 100, 10_000)).unwrap();
        assert_eq!(deck.seek(5000).unwrap(), 5000);
        assert_eq!(deck.position_frames(), 5000);
        assert_eq!(deck.duration_frames(), Some(10_000));
        deck.pause();
        assert_eq!(deck.pump().unwrap(), Progress::Idle);
        deck.play();
        assert_eq!(deck.pump().unwrap(), Progress::Played);
        deck.stop();
        assert_eq!(deck.state(), DeckState::Stopped);
        assert_eq!(deck.pump().unwrap(), Progress::Idle);
    }

    #[test]
    fn seek_discards_buffered_sink_audio() {
        // A seek is a playback discontinuity: the deck must tell the sink to drop
        // its already-buffered pre-seek audio, else that audio plays out before
        // the new position is heard (an audible seek lag). This asserts the
        // deck→sink `discard` wiring; the ring-clear itself lives in the concrete
        // desktop sink.
        struct DiscardSink {
            rate: u32,
            ch: usize,
            discards: usize,
        }
        impl Sink for DiscardSink {
            fn stream_info(&self) -> StreamInfo {
                StreamInfo {
                    sample_rate: self.rate,
                    channels: self.ch as u16,
                    format: SampleFormat::Int { bits: 24 },
                }
            }
            fn write(&mut self, _frames: &[i32]) -> Result<(), SinkError> {
                Ok(())
            }
            fn flush(&mut self) -> Result<(), SinkError> {
                Ok(())
            }
            fn discard(&mut self) -> Result<(), SinkError> {
                self.discards += 1;
                Ok(())
            }
        }

        let sink = DiscardSink {
            rate: 48_000,
            ch: 1,
            discards: 0,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.load(const_dec(48_000, 1, 100, 10_000)).unwrap();
        deck.pump().unwrap(); // buffer some audio first
        assert_eq!(deck.sink().discards, 0, "no discard before seeking");
        assert_eq!(deck.seek(5000).unwrap(), 5000);
        assert_eq!(deck.position_frames(), 5000);
        assert_eq!(
            deck.sink().discards,
            1,
            "seek must discard the buffered pre-seek audio exactly once"
        );
    }

    #[test]
    fn seek_fades_in_to_declick_the_splice() {
        // A seek is a hard splice: the sink buffer is cleared and the first
        // post-seek sample jumps from the (now-silent) ring straight to full level
        // — a step, i.e. a click, on EVERY format. The deck must ramp the first
        // frames up from zero with a smoothstep gain so there is no step.
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        // 16-bit constant 100 → canonical full scale 25600. Long enough to pump
        // several blocks after the seek.
        deck.load(const_dec(48_000, 1, 100, 100_000)).unwrap();

        // Non-seek path is unchanged: a first play fades NOT — full level at once.
        deck.pump().unwrap();
        let pre_len = deck.sink.written.len();
        assert_eq!(pre_len, BLOCK, "one block pumped pre-seek");
        assert!(
            deck.sink.written.iter().all(|&s| s == 25_600),
            "no fade before a seek — first play is full scale"
        );

        // fade_len = round(8 ms * 48000 / 1000) = 384 output frames (> BLOCK, so
        // the fade deliberately spans more than one pump).
        let fade_len = 384usize;
        deck.seek(5000).unwrap();
        // Pump past the fade (3 blocks = 768 frames ≥ fade_len).
        deck.pump().unwrap();
        deck.pump().unwrap();
        deck.pump().unwrap();
        let post = &deck.sink.written[pre_len..];
        let full = 25_600i32;

        // Splice point: starts at exactly zero (smoothstep t=0) — no step/click.
        assert_eq!(post[0], 0, "fade starts at zero at the splice");
        // Monotonic non-decreasing across the whole fade (a clean rise).
        for i in 1..=fade_len {
            assert!(
                post[i] >= post[i - 1],
                "fade must rise monotonically at frame {i}: {} < {}",
                post[i],
                post[i - 1]
            );
        }
        // Smoothstep waypoints are exact here (25600 × dyadic fractions):
        //   t=0.25 → 0.15625 → 4000 ; t=0.5 → 0.5 → 12800 ; t=0.75 → 0.84375 → 21600.
        assert_eq!(post[fade_len / 4], 4000, "quarter-fade = smoothstep(0.25)");
        assert_eq!(
            post[fade_len / 2],
            12_800,
            "mid-fade = smoothstep(0.5) = half"
        );
        assert_eq!(
            post[3 * fade_len / 4],
            21_600,
            "three-quarter-fade = smoothstep(0.75)"
        );
        // Still rising just before the end, then full scale by frame fade_len.
        assert!(
            post[fade_len - 1] < full,
            "below full at the last fade frame: {}",
            post[fade_len - 1]
        );
        assert_eq!(post[fade_len], full, "full scale by frame fade_len (384)");
        // And it stays at full afterwards (fade does not re-trigger).
        assert_eq!(post[fade_len + 100], full, "full scale past the fade");
    }

    #[test]
    fn seek_output_matches_natural_playback_after_warmup() {
        // Decode-path proof that the residual seek click is NOT a decode defect.
        // A `RampDecoder` emits value == source frame index, so we can read back
        // exactly which frames the deck produced. Claim: once the de-click fade has
        // warmed up, every post-seek frame equals the frame a natural play-through
        // to that position would have emitted — the splice is bit-exact. Any click
        // a listener still hears therefore lives in the output stage (the sink's
        // ring clear), not here.
        fn ramp(frames: u64) -> RampDecoder {
            RampDecoder {
                ch: 1,
                frames,
                pos: 0,
                delay: 0,
                playable: None,
            }
        }

        // Natural play-through from 0: output[i] == source frame i.
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut natural: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        natural.load(ramp(20_000)).unwrap();
        for _ in 0..40 {
            natural.pump().unwrap();
        }
        let nat = &natural.sink.written;
        assert_eq!(nat[7_000], 7_000, "natural output frame index is the value");

        // Seek to P, then pump past the fade.
        let p = 5_000usize;
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut seeked: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        seeked.load(ramp(20_000)).unwrap();
        seeked.pump().unwrap(); // buffer pre-seek audio (the sink discards it IRL)
        let pre = seeked.sink.written.len();
        seeked.seek(p as u64).unwrap();
        for _ in 0..40 {
            seeked.pump().unwrap();
        }
        let post = &seeked.sink.written[pre..];

        let fade_len = 384usize; // round(8 ms * 48000 / 1000)
        // New side ramps up from zero (the deck fade-in) — not a step.
        assert_eq!(post[0], 0, "post-seek output starts at zero, not a jump");
        // After the fade: bit-exact. Emitted frame j is source frame P+j, which is
        // exactly what the natural play-through emitted at P+j.
        for j in fade_len..fade_len + 2_000 {
            assert_eq!(
                post[j],
                (p + j) as i32,
                "post-seek frame {j} decodes source frame P+j"
            );
            assert_eq!(
                post[j],
                nat[p + j],
                "seek is bit-exact vs natural play-through at P+{j}"
            );
        }
    }

    #[test]
    fn channels_over_max_are_rejected_not_overflow() {
        // A device with > MAX_CH channels + an equal-rate, equal-channel source
        // takes the no-resampler path; without the guard it would overflow the
        // MAX_CH-sized scratch buffers. It must be rejected, not panic.
        let sink = MockSink {
            rate: 48_000,
            ch: 9,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        assert_eq!(
            deck.load(const_dec(48_000, 9, 100, 100)).err(),
            Some(DeckError::ChannelMismatch)
        );
    }

    #[test]
    fn incompatible_preload_is_rejected_immediately() {
        // `preload` builds (validates) the track now, so an incompatible upcoming
        // track errors here — not mid-playback at the gapless swap.
        let sink = MockSink {
            rate: 48_000,
            ch: 2,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.load(const_dec(48_000, 2, 100, 500)).unwrap(); // stereo → OK
        // 3 channels is neither passthrough (≠ 2) nor mono→stereo.
        assert_eq!(
            deck.preload(const_dec(48_000, 3, 100, 500)).err(),
            Some(DeckError::ChannelMismatch)
        );
        // The deck keeps playing the current track; the bad preload never staged.
        assert_eq!(deck.free_preload_slots(), 1);
        assert_eq!(deck.state(), DeckState::Playing);
    }

    // --- ReplayGain output gain (Part 1) ---------------------------------

    /// Build a deck over a 16-bit `ConstDecoder` (norm = 2^(24-16) = 256),
    /// optionally set a ReplayGain, run to Ended and return the written samples.
    fn run_rg(value: i32, frames: u64, gain_db: Option<f32>) -> Vec<i32> {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
            last_volume: None,
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        if let Some(db) = gain_db {
            deck.set_replay_gain(db);
        }
        deck.load(const_dec(48_000, 1, value, frames)).unwrap();
        run(&mut deck);
        deck.sink.written
    }

    #[test]
    fn replay_gain_unity_is_bit_identical() {
        // `set_replay_gain(0.0)` must be an exact passthrough: identical, sample
        // for sample, to no gain at all (unity linear factor is exactly 1.0).
        let plain = run_rg(12_345, 1000, None);
        let unity = run_rg(12_345, 1000, Some(0.0));
        assert_eq!(plain, unity, "0 dB ReplayGain must be bit-identical");
        // And it's the canonical 24-bit value, not something re-scaled.
        assert!(unity.iter().all(|&s| s == 12_345 * 256));
    }

    #[test]
    fn replay_gain_attenuates() {
        // value 10000, 16-bit → canonical 2_560_000.
        let base = 10_000 * 256;
        // -6.0206 dB ≈ ×0.5.
        let half = run_rg(10_000, 500, Some(-6.0206));
        assert!(
            half.iter().all(|&s| (s - base / 2).abs() <= 3),
            "-6.0206 dB must halve: got {}",
            half[0]
        );
        // -20 dB = ×0.1.
        let tenth = run_rg(10_000, 500, Some(-20.0));
        assert!(
            tenth.iter().all(|&s| (s - base / 10).abs() <= 3),
            "-20 dB must be ×0.1: got {}",
            tenth[0]
        );
    }

    #[test]
    fn replay_gain_boosts() {
        // value 1000 → canonical 256_000; +6.0206 dB ≈ ×2 (well under full scale).
        let base = 1000 * 256;
        let doubled = run_rg(1000, 500, Some(6.0206));
        assert!(
            doubled.iter().all(|&s| (s - base * 2).abs() <= 3),
            "+6.0206 dB must double: got {}",
            doubled[0]
        );
    }

    #[test]
    fn replay_gain_boost_still_hard_clips() {
        // value 30000 → canonical 7_680_000; +20 dB (×10) overflows full scale
        // and must clamp to CANONICAL_MAX, not panic or wrap.
        let out = run_rg(30_000, 500, Some(20.0));
        assert!(
            out.iter().all(|&s| s == CANONICAL_MAX as i32),
            "a boost past full scale must clamp to CANONICAL_MAX: got {}",
            out[0]
        );
    }

    #[test]
    fn replay_gain_db_clamped_to_sane_range() {
        // Absurd requests are clamped into [-60, 24] dB before use (no NaN/inf,
        // no absurd linear factor). A +1000 dB request behaves like +24 dB.
        let a = run_rg(100, 100, Some(1000.0));
        let b = run_rg(100, 100, Some(24.0));
        assert_eq!(a, b, "boost clamps to +24 dB");
        let c = run_rg(100, 100, Some(-1000.0));
        let d = run_rg(100, 100, Some(-60.0));
        assert_eq!(c, d, "attenuation clamps to -60 dB");
    }

    #[test]
    fn pow10_db_matches_powf_reference() {
        // Unity is exact.
        assert_eq!(pow10_db(0.0), 1.0);
        // The designed round-number points.
        assert!((pow10_db(-6.0206) - 0.5).abs() < 1e-4);
        assert!((pow10_db(-20.0) - 0.1).abs() < 1e-4);
        assert!((pow10_db(6.0206) - 2.0).abs() < 1e-4);
        // Cross-check against std powf (tests link std) across the range.
        for &db in &[-60.0, -24.0, -15.0, -6.0, -3.0, 0.0, 3.0, 6.0, 12.0, 24.0] {
            let got = pow10_db(db);
            let want = 10f32.powf(db / 20.0);
            assert!(
                (got / want - 1.0).abs() < 1e-4,
                "pow10_db({db}) = {got}, powf ref {want}"
            );
        }
    }
}
