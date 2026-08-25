//! The playback **deck** — the producer-side player mechanism.
//!
//! A [`Deck`] plays a [`SeekableDecoder`] into a [`Sink`], resampling to the sink
//! rate, normalizing to a canonical scale, and applying a ramped volume. It holds
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
/// Input scratch capacity (frames) — covers the resampler's largest demand within
/// its supported downsample range at `BLOCK` output frames.
const IN_FIFO: usize = 2560;

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
    /// Per-channel source frames consumed so far (playback position).
    pos: u64,
    total: Option<u64>,
    /// Decoder hit end of stream.
    eof: bool,
    /// Remaining silence-fed pumps to drain the resampler tail after `eof`.
    drain: usize,
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
    /// Target volume 0.0..=1.0 and the ramped value chasing it.
    volume: f32,
    cur_vol: f32,
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
            volume: 1.0,
            cur_vol: 1.0,
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
        Ok(Track {
            total: decoder.total_frames(),
            decoder,
            resampler,
            channels: ch,
            map,
            norm: libm_exp2(f32::from(CANONICAL_BITS as i16 - i16::from(bits))),
            rate: si.sample_rate,
            pos: 0,
            eof: false,
            drain: 0,
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

    /// Set the target volume (0.0..=1.0); applied with a short ramp.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    /// Seek the current track to per-channel `frame`.
    ///
    /// # Errors
    /// [`DeckError::Decode`] (incl. `Unsupported`) if the track can't seek.
    pub fn seek(&mut self, frame: u64) -> Result<u64, DeckError> {
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

    /// Produce and write one output block from the current track. Returns `true`
    /// when the track has fully finished (decoder EOF + resampler tail drained).
    fn produce_block(&mut self) -> Result<bool, DeckError> {
        let sink_ch = self.sink_channels;
        // Decide the resampled/plain source block and how many frames it yields.
        let (out_frames, finished) = if self.current.as_ref().unwrap().resampler.is_some() {
            self.block_resampled()?
        } else {
            self.block_plain()?
        };
        // Normalize + volume + channel-map rs_buf → out_buf, then write.
        let track = self.current.as_ref().unwrap();
        let src_ch = track.channels;
        let norm = track.norm;
        let (v0, v1) = (self.cur_vol, self.volume);
        for f in 0..out_frames {
            let t = (f as f32 + 1.0) / out_frames as f32;
            let vol = v0 + (v1 - v0) * t;
            let g = norm * vol;
            for oc in 0..sink_ch {
                let sc = match track.map {
                    Map::Passthrough => oc,
                    Map::MonoToStereo => 0,
                };
                let s = self.rs_buf[f * src_ch + sc] as f32 * g;
                // Round to nearest without `f32::round` (std-only in no_std).
                let r = if s >= 0.0 { s + 0.5 } else { s - 0.5 };
                self.out_buf[f * sink_ch + oc] =
                    r.clamp(-CANONICAL_MAX - 1.0, CANONICAL_MAX) as i32;
            }
        }
        self.cur_vol = self.volume;
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
        track.pos += have as u64;
        Ok((have, have < BLOCK))
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

    struct MockSink {
        rate: u32,
        ch: usize,
        written: Vec<i32>,
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
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.load(const_dec(48_000, 1, 100, 1000)).unwrap();
        run(&mut deck);
        assert_eq!(deck.sink.written.len(), 1000);
        assert!(deck.sink.written.iter().all(|&s| s == 25_600));
        assert_eq!(deck.state(), DeckState::Ended);
    }

    #[test]
    fn volume_scales_and_gapless_preload_swaps() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
        };
        let mut deck: Deck<_, _, 1> = Deck::new(sink, Quality::Fast);
        deck.set_volume(0.5);
        deck.load(const_dec(48_000, 1, 100, 500)).unwrap(); // A → 25600*0.5 = 12800
        assert_eq!(deck.free_preload_slots(), 1);
        deck.preload(const_dec(48_000, 1, 60, 500)).unwrap(); // B → 15360*0.5 = 7680
        assert_eq!(deck.free_preload_slots(), 0);
        let swaps = run(&mut deck);
        assert_eq!(swaps, 1, "one gapless swap");
        let out = &deck.sink.written;
        assert_eq!(out.len(), 1000, "both tracks, no gap");
        // Volume ramps 1.0→0.5 over the first block; check the settled regions.
        assert_eq!(out[300], 12_800, "A at half volume");
        assert_eq!(out[900], 7_680, "B at half volume");
    }

    #[test]
    fn advance_promotes_preload_immediately() {
        let sink = MockSink {
            rate: 48_000,
            ch: 1,
            written: Vec::new(),
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
    fn channels_over_max_are_rejected_not_overflow() {
        // A device with > MAX_CH channels + an equal-rate, equal-channel source
        // takes the no-resampler path; without the guard it would overflow the
        // MAX_CH-sized scratch buffers. It must be rejected, not panic.
        let sink = MockSink {
            rate: 48_000,
            ch: 9,
            written: Vec::new(),
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
}
