//! Audio **output** abstraction and the **player** pipeline.
//!
//! A [`Sink`] is where decoded, rate-matched PCM goes — a desktop cpal stream, an
//! embedded SAI/I²S DMA, or a test double. Concrete sinks live in the platform
//! shells (they need std / hardware); this crate holds only the trait plus the
//! platform-agnostic [`Player`], which drives **decode → resample → channel-map →
//! write** and is identical on every target.
//!
//! Everything here is `no_std` and no-alloc: the [`Player`] carries fixed scratch
//! buffers, and a [`Sink`]'s [`write`](Sink::write) applies backpressure so the
//! producer paces to the device's real-time clock.

use crate::decoder::{DecodeError, Decoder};
use crate::model::StreamInfo;
use crate::resample::{Quality, ResampleError, Resampler};

/// Fixed resampler output block (frames) the player writes at a time.
const OUTPUT_FRAMES: usize = 256;
/// Max channels handled by the player scratch.
const MAX_CH: usize = 8;
/// Input-frame scratch capacity. Covers the largest `input_frames_next` the
/// resampler can demand within its supported downsample range.
const IN_FIFO_FRAMES: usize = 4096;

/// Output-device errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// The output device / stream is gone.
    Closed,
    /// The requested stream configuration is unsupported.
    Unsupported,
}

/// An audio output that consumes interleaved `i32` PCM at a fixed rate.
pub trait Sink {
    /// The rate / channels / format the device consumes.
    fn stream_info(&self) -> StreamInfo;
    /// Write **all** of `frames` (interleaved `i32`, length = frame count ×
    /// channels), blocking as needed for buffer space. Returns on completion.
    ///
    /// # Errors
    /// [`SinkError`] if the device is gone or the write cannot complete.
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError>;
    /// Play out any buffered audio and stop (end of stream).
    ///
    /// # Errors
    /// [`SinkError`] if the device is gone.
    fn flush(&mut self) -> Result<(), SinkError>;
}

/// Player construction / run errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerError {
    /// The sink channel count is incompatible with the source (and it is not the
    /// supported mono→stereo case).
    ChannelMismatch,
    /// The rate pair needs a resampler that could not be built (or a demand
    /// beyond the player's scratch).
    Resample(ResampleError),
    /// A decode error.
    Decode(DecodeError),
    /// A sink error.
    Sink(SinkError),
}

/// How source channels map to sink channels.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChannelMap {
    /// Equal channel counts — copy through.
    Passthrough,
    /// Mono source → stereo sink — duplicate each sample.
    MonoToStereo,
}

/// Drives a [`Decoder`] through an optional [`Resampler`] and channel map into a
/// [`Sink`], until end of stream.
pub struct Player<D, K> {
    decoder: D,
    sink: K,
    resampler: Option<Resampler>,
    in_ch: usize,
    map: ChannelMap,
    /// Decoded input scratch (interleaved, source channels).
    in_buf: [i32; IN_FIFO_FRAMES * MAX_CH],
    /// Resampler output scratch (interleaved, source channels).
    rs_buf: [i32; OUTPUT_FRAMES * MAX_CH],
    /// Channel-mapped write scratch (interleaved, sink channels).
    out_buf: [i32; OUTPUT_FRAMES * MAX_CH],
}

impl<D: Decoder, K: Sink> Player<D, K> {
    /// Inspect both ends, pick the channel map, and build a resampler if the
    /// rates differ. `quality` selects the resampler tier (unused at matching
    /// rates).
    ///
    /// # Errors
    /// [`PlayerError::ChannelMismatch`] for an unsupported channel pairing;
    /// [`PlayerError::Resample`] if the resampler cannot be built for the rates.
    pub fn new(decoder: D, sink: K, quality: Quality) -> Result<Self, PlayerError> {
        let si = decoder.stream_info();
        let so = sink.stream_info();
        let in_ch = si.channels as usize;
        let out_ch = so.channels as usize;
        let map = if in_ch == out_ch {
            ChannelMap::Passthrough
        } else if in_ch == 1 && out_ch == 2 {
            ChannelMap::MonoToStereo
        } else {
            return Err(PlayerError::ChannelMismatch);
        };
        let resampler = if si.sample_rate == so.sample_rate {
            None
        } else {
            let r = Resampler::new(
                in_ch,
                si.sample_rate,
                so.sample_rate,
                1.0,
                OUTPUT_FRAMES,
                quality,
            )
            .map_err(PlayerError::Resample)?;
            // The scratch must cover the largest input demand.
            if r.input_frames_next() * in_ch > IN_FIFO_FRAMES * MAX_CH {
                return Err(PlayerError::Resample(ResampleError::RatioRangeTooWide));
            }
            Some(r)
        };
        Ok(Self {
            decoder,
            sink,
            resampler,
            in_ch,
            map,
            in_buf: [0; IN_FIFO_FRAMES * MAX_CH],
            rs_buf: [0; OUTPUT_FRAMES * MAX_CH],
            out_buf: [0; OUTPUT_FRAMES * MAX_CH],
        })
    }

    /// Decode → (resample) → channel-map → write, to end of stream, then flush.
    /// Blocks, paced by the sink's backpressure.
    ///
    /// # Errors
    /// Propagates decode / sink / resample errors.
    pub fn run(&mut self) -> Result<(), PlayerError> {
        if self.resampler.is_some() {
            self.run_resampled()
        } else {
            self.run_passthrough()
        }
    }

    /// Fill `in_buf[..need*in_ch]` from the decoder; returns the frames obtained
    /// (`< need` at end of stream — the remainder is zero-padded).
    fn fill_input(&mut self, need: usize) -> Result<usize, PlayerError> {
        let ch = self.in_ch;
        let mut have = 0usize;
        while have < need {
            let n = self
                .decoder
                .decode_into(&mut self.in_buf[have * ch..need * ch])
                .map_err(PlayerError::Decode)?;
            if n == 0 {
                for s in &mut self.in_buf[have * ch..need * ch] {
                    *s = 0;
                }
                break;
            }
            have += n / ch;
        }
        Ok(have)
    }

    fn run_passthrough(&mut self) -> Result<(), PlayerError> {
        let ch = self.in_ch;
        loop {
            let have = self.fill_input(OUTPUT_FRAMES)?;
            if have == 0 {
                break;
            }
            let n = map_channels(&self.in_buf, &mut self.out_buf, have, ch, self.map);
            self.sink
                .write(&self.out_buf[..n])
                .map_err(PlayerError::Sink)?;
            if have < OUTPUT_FRAMES {
                break; // end of stream
            }
        }
        self.sink.flush().map_err(PlayerError::Sink)
    }

    fn run_resampled(&mut self) -> Result<(), PlayerError> {
        let in_ch = self.in_ch;
        // `latency()` extra input frames flush the filter tail after EOF.
        let mut flush_frames =
            self.resampler.as_ref().map_or(0, Resampler::latency) + OUTPUT_FRAMES;
        loop {
            let need = self.resampler.as_ref().unwrap().input_frames_next();
            let have = self.fill_input(need)?;
            let eof = have < need;

            let r = self.resampler.as_mut().unwrap();
            r.process(
                &self.in_buf[..need * in_ch],
                &mut self.rs_buf[..OUTPUT_FRAMES * in_ch],
            )
            .map_err(PlayerError::Resample)?;
            let n = map_channels(
                &self.rs_buf,
                &mut self.out_buf,
                OUTPUT_FRAMES,
                in_ch,
                self.map,
            );
            self.sink
                .write(&self.out_buf[..n])
                .map_err(PlayerError::Sink)?;

            if eof {
                // Drain the resampler tail with silence, then stop.
                if flush_frames <= need {
                    break;
                }
                flush_frames -= need;
            }
        }
        self.sink.flush().map_err(PlayerError::Sink)
    }
}

/// Map `frames` interleaved source frames (`src_ch` channels) from `src` into
/// `dst`, returning the number of interleaved samples written. Passthrough copies
/// `frames × src_ch`; mono→stereo duplicates each sample.
fn map_channels(
    src: &[i32],
    dst: &mut [i32],
    frames: usize,
    src_ch: usize,
    map: ChannelMap,
) -> usize {
    match map {
        ChannelMap::Passthrough => {
            let n = frames * src_ch;
            dst[..n].copy_from_slice(&src[..n]);
            n
        }
        ChannelMap::MonoToStereo => {
            for f in 0..frames {
                dst[2 * f] = src[f];
                dst[2 * f + 1] = src[f];
            }
            frames * 2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SampleFormat;
    extern crate alloc;
    use alloc::vec::Vec;

    /// A decoder yielding a fixed ramp: frame `f`, channel `c` = `f*10 + c + 1`.
    /// Emits `frames` total, honoring any output-buffer size (partial fills).
    struct RampDecoder {
        rate: u32,
        ch: usize,
        frames: usize,
        pos: usize,
    }
    impl Decoder for RampDecoder {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: self.rate,
                channels: self.ch as u16,
                format: SampleFormat::Int { bits: 16 },
            }
        }
        fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
            let want = (out.len() / self.ch).min(self.frames - self.pos);
            for f in 0..want {
                for c in 0..self.ch {
                    out[f * self.ch + c] = ((self.pos + f) * 10 + c + 1) as i32;
                }
            }
            self.pos += want;
            Ok(want * self.ch)
        }
    }

    /// A sink that records everything written and reports a fixed config.
    struct MockSink {
        rate: u32,
        ch: usize,
        written: Vec<i32>,
        flushed: bool,
    }
    impl MockSink {
        fn new(rate: u32, ch: usize) -> Self {
            Self {
                rate,
                ch,
                written: Vec::new(),
                flushed: false,
            }
        }
    }
    impl Sink for MockSink {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: self.rate,
                channels: self.ch as u16,
                format: SampleFormat::Int { bits: 16 },
            }
        }
        fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
            self.written.extend_from_slice(frames);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), SinkError> {
            self.flushed = true;
            Ok(())
        }
    }

    #[test]
    fn passthrough_matches_decoded_pcm() {
        // Same rate + channels → the sink receives exactly the decoded stream.
        let dec = RampDecoder {
            rate: 48_000,
            ch: 2,
            frames: 1000,
            pos: 0,
        };
        let sink = MockSink::new(48_000, 2);
        let mut p = Player::new(dec, sink, Quality::Fast).unwrap();
        p.run().unwrap();
        assert!(p.sink.flushed);
        assert_eq!(p.sink.written.len(), 1000 * 2);
        for f in 0..1000 {
            assert_eq!(p.sink.written[f * 2], (f * 10 + 1) as i32);
            assert_eq!(p.sink.written[f * 2 + 1], (f * 10 + 2) as i32);
        }
    }

    #[test]
    fn mono_source_to_stereo_sink_duplicates() {
        let dec = RampDecoder {
            rate: 48_000,
            ch: 1,
            frames: 500,
            pos: 0,
        };
        let sink = MockSink::new(48_000, 2);
        let mut p = Player::new(dec, sink, Quality::Fast).unwrap();
        p.run().unwrap();
        assert_eq!(p.sink.written.len(), 500 * 2);
        for f in 0..500 {
            let v = (f * 10 + 1) as i32;
            assert_eq!(p.sink.written[f * 2], v);
            assert_eq!(p.sink.written[f * 2 + 1], v);
        }
    }

    #[test]
    fn incompatible_channels_error() {
        // Stereo source → mono sink is unsupported.
        let dec = RampDecoder {
            rate: 48_000,
            ch: 2,
            frames: 10,
            pos: 0,
        };
        let sink = MockSink::new(48_000, 1);
        assert_eq!(
            Player::new(dec, sink, Quality::Fast).err(),
            Some(PlayerError::ChannelMismatch)
        );
    }

    #[test]
    fn rate_conversion_produces_scaled_frame_count() {
        // 44.1k → 48k upsample: sink gets ~ in_frames × 48000/44100 more frames,
        // and matches a direct Resampler run of the same signal.
        let frames_in = 4000usize;
        let dec = RampDecoder {
            rate: 44_100,
            ch: 1,
            frames: frames_in,
            pos: 0,
        };
        let sink = MockSink::new(48_000, 1);
        let mut p = Player::new(dec, sink, Quality::Hq).unwrap();
        p.run().unwrap();
        let got = p.sink.written.len();
        let expect = (frames_in as f64 * 48_000.0 / 44_100.0) as usize;
        // Within a couple of blocks of the ideal (tail flush + block granularity).
        assert!(
            got >= expect.saturating_sub(3 * OUTPUT_FRAMES) && got <= expect + 3 * OUTPUT_FRAMES,
            "resampled frame count {got} vs ~{expect}"
        );
        assert!(p.sink.flushed);
        assert!(p.sink.written.iter().any(|&s| s != 0), "silent output");
    }
}
