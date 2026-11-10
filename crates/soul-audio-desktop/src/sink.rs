//! Desktop [`Sink`] backed by cpal — the default output device fed through a
//! lock-free SPSC ring.
//!
//! The [`Deck`](soul_audio_core::Deck) pushes interleaved `i32` **canonical
//! 24-bit** PCM (`±2^23`) via [`Sink::write`]; cpal's real-time callback pops
//! from the ring and converts to the device sample format. Because the deck
//! already normalized every track to the same 24-bit scale, the sink is
//! bit-depth-agnostic — it never needs to know the source depth. `write` blocks
//! when the ring is full (backpressure), pacing the pump to the device clock; an
//! empty ring plays silence (a soft underrun).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_core::{Sink, SinkError, StreamInfo};

use soul_audio_core::convert::{self, CANONICAL_BITS};

/// Falling-smoothstep fade-out length for a seek-discard, in milliseconds. On a
/// seek the buffered pre-seek audio still sitting in the ring is ramped down to
/// zero over this window *before* the ring is cleared, so the output descends
/// smoothly to silence instead of stepping from the last buffered sample straight
/// to 0 — that step is a format-agnostic click. It pairs with the deck's rising
/// fade-IN on the post-seek side (`deck.rs`): old→0 here, brief silence, then new
/// 0→full there = a click-free splice. 8 ms ≈ a few hundred samples at 48 kHz, so
/// the ack is still released far inside `discard`'s ~200 ms bound.
const DISCARD_FADE_MS: u32 = 8;

/// The slice of the SPSC ring consumer the real-time fill needs. Abstracting it
/// lets the flush/fade logic in [`Fill`] be unit-tested against an in-memory fake
/// — the cpal callback is otherwise only reachable with a live output device.
trait Ring {
    /// Pop the next buffered sample, or `None` on underrun.
    fn pop(&mut self) -> Option<i32>;
    /// Drop all buffered samples.
    fn clear(&mut self);
}

/// Newtype over the cpal SPSC ring consumer. A concrete named type (rather than
/// the `<HeapRb<i32> as Split>::Cons` projection) so [`Ring`] can also be
/// implemented for the test fake without a coherence conflict.
struct CpalRing(<HeapRb<i32> as Split>::Cons);

impl Ring for CpalRing {
    fn pop(&mut self) -> Option<i32> {
        self.0.try_pop()
    }
    fn clear(&mut self) {
        self.0.clear();
    }
}

/// Real-time fill state shared by the f32 and i16 callbacks: the smoothed volume
/// gain plus the seek-discard fade-out. Allocation-free; every method runs on the
/// audio thread.
struct Fill {
    /// Smoothed output gain, ramped toward the target volume per sample so a
    /// volume change is heard on already-buffered audio without a click.
    cur_gain: f32,
    /// Interleaved samples of seek fade-out still owed. `0` = idle. Armed to
    /// `fade_len` when a discard request is first seen, counted down one per faded
    /// sample; on reaching `0` the ring is cleared and the discard ack released.
    fade_remaining: u32,
    /// Fade-out length in interleaved samples (`DISCARD_FADE_MS` at the device
    /// rate × channels). `0` disables the fade (an instant clear + ack).
    fade_len: u32,
}

impl Fill {
    fn new(fade_len: u32) -> Self {
        Self {
            cur_gain: 1.0,
            fade_remaining: 0,
            fade_len,
        }
    }

    /// Called once at the top of each callback. If a seek-discard is pending and
    /// no fade is already in flight, arm the fade-out so the buffered head ramps
    /// down in [`next`](Fill::next) rather than being hard-cleared here. A zero
    /// `fade_len` keeps the original instant clear + ack (and the Release store
    /// publishes the clear to the producer's Acquire-load in [`Sink::discard`], so
    /// it only resumes writing post-seek audio once the clear has happened). While
    /// a fade is in flight (`fade_remaining > 0`) the flag is left set until
    /// `next` finishes the ramp, so this never re-arms mid-fade.
    fn begin(&mut self, flush: &AtomicBool, ring: &mut impl Ring) {
        if self.fade_remaining == 0 && flush.load(Ordering::Acquire) {
            if self.fade_len == 0 {
                ring.clear();
                flush.store(false, Ordering::Release);
            } else {
                self.fade_remaining = self.fade_len;
            }
        }
    }

    /// Produce the next source sample for the output buffer. Applies the smoothed
    /// volume gain and, while a seek fade-out is in flight, a falling smoothstep
    /// gain (`g = t·t·(3−2t)`, `t = fade_remaining / fade_len`, so `g` runs 1→0 —
    /// the mirror of the deck's rising fade, no trig for a shared curve). When the
    /// fade completes it clears the ring's remaining pre-seek audio and releases
    /// the discard ack. `None` = ring underran (downstream fills silence).
    fn next(&mut self, ring: &mut impl Ring, flush: &AtomicBool, target: f32) -> Option<i32> {
        self.cur_gain += (target - self.cur_gain) * 0.01;
        if self.fade_remaining == 0 {
            return ring.pop().map(|s| (s as f32 * self.cur_gain) as i32);
        }
        let t = self.fade_remaining as f32 / self.fade_len as f32;
        let g = t * t * (3.0 - 2.0 * t);
        let out = ring.pop().map(|s| (s as f32 * self.cur_gain * g) as i32);
        self.fade_remaining -= 1;
        if self.fade_remaining == 0 {
            ring.clear();
            flush.store(false, Ordering::Release);
        }
        out
    }
}

/// A cpal output stream presented as a [`Sink`] consuming canonical 24-bit PCM.
pub struct CpalSink {
    prod: <HeapRb<i32> as Split>::Prod,
    /// Seek-discard handshake with the real-time callback. [`Sink::discard`]
    /// stores `true` (a request); the callback clears the ring, then stores
    /// `false` (the ack). The producer waits for the ack, so post-seek audio it
    /// writes next is never caught by the clear.
    flush: Arc<AtomicBool>,
    /// Target output volume (`f32::to_bits`, default `1.0`), read and ramped
    /// toward per-sample by the real-time callback so changes are heard
    /// immediately (applied to already-buffered samples as they play) instead of
    /// lagging by the ring's ~200 ms depth.
    volume: Arc<AtomicU32>,
    // Held to keep the stream alive; dropping it stops playback.
    _stream: Stream,
    rate: u32,
    channels: u16,
}

impl CpalSink {
    /// Open the default output device. Input is always canonical 24-bit PCM (the
    /// deck normalizes every track to it), so no per-source bit depth is needed.
    pub fn new() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("no default output device")?;
        let supported = device
            .default_output_config()
            .context("querying default output config")?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let channels = config.channels;
        let rate = config.sample_rate.0;

        // ~200 ms of buffering. Deep enough to ride out pump-thread scheduling
        // jitter (the pump decodes well ahead of real-time, including DSD at
        // ~3x), but shallow enough that a volume or pause change is heard
        // promptly instead of after a second of already-queued audio drains.
        // (A seek clears the ring outright via `discard`.)
        const BUFFER_MS: usize = 200;
        let cap = (rate as usize * channels as usize * BUFFER_MS) / 1000;
        let (prod, cons) = HeapRb::<i32>::new(cap).split();
        let mut cons = CpalRing(cons);
        let err_fn = |e| eprintln!("cpal stream error: {e}");

        let flush = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));

        // On a seek, `discard` requests a ring clear via `flush`. The callback
        // honors it through `Fill`: ramp the buffered pre-seek audio down to zero
        // over a short fade (killing the splice click), THEN clear the remainder
        // and release the ack (`store(false)`). Ordering matters — the ack's
        // Release publishes the clear to the producer's Acquire-load in `discard`,
        // so the producer only resumes writing (post-seek audio) once the clear
        // has happened.
        let cb_flush = Arc::clone(&flush);
        let cb_volume = Arc::clone(&volume);
        // Fade-out length in interleaved samples: `DISCARD_FADE_MS` at the device
        // rate × channels, rounded. Computed in u64 so no rate/channel pair can
        // overflow the intermediate product.
        let fade_len = ((u64::from(DISCARD_FADE_MS) * u64::from(rate) * u64::from(channels) + 500)
            / 1000) as u32;

        // The callback pops from the ring, applies a smoothed gain (so a volume
        // change is heard on already-buffered samples — instantly, not after the
        // ring drains — without an audible click) plus any seek fade-out, and
        // converts via the unit-tested `convert` functions (silence on underrun).
        // `Fill` holds that per-sample state and is unit-tested device-free.
        let stream = match sample_format {
            SampleFormat::F32 => {
                let mut fill = Fill::new(fade_len);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            fill.begin(&cb_flush, &mut cons);
                            convert::fill_f32(
                                || {
                                    let target = f32::from_bits(cb_volume.load(Ordering::Relaxed));
                                    fill.next(&mut cons, &cb_flush, target)
                                },
                                data,
                            );
                        },
                        err_fn,
                        None,
                    )
                    .context("building f32 output stream")?
            }
            SampleFormat::I16 => {
                let mut fill = Fill::new(fade_len);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _| {
                            fill.begin(&cb_flush, &mut cons);
                            convert::fill_i16(
                                || {
                                    let target = f32::from_bits(cb_volume.load(Ordering::Relaxed));
                                    fill.next(&mut cons, &cb_flush, target)
                                },
                                data,
                            );
                        },
                        err_fn,
                        None,
                    )
                    .context("building i16 output stream")?
            }
            other => return Err(anyhow!("unsupported device sample format: {other:?}")),
        };
        stream.play().context("starting stream")?;
        Ok(Self {
            prod,
            flush,
            volume,
            _stream: stream,
            rate,
            channels,
        })
    }
}

impl Sink for CpalSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: CoreFmt::Int {
                bits: CANONICAL_BITS,
            },
        }
    }

    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        let mut off = 0;
        while off < frames.len() {
            off += self.prod.push_slice(&frames[off..]);
            if off < frames.len() {
                // Ring full — wait for the callback to drain some.
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        // Wait for the callback to play out what's buffered.
        while self.prod.occupied_len() > 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(())
    }

    fn discard(&mut self) -> Result<(), SinkError> {
        // Request a ring clear, then wait (bounded) for the callback's ack. The
        // wait guarantees the buffered pre-seek audio is gone before the pump
        // writes the post-seek block; the bound (~200 ms) means a dead stream
        // whose callback never runs can't hang the pump.
        self.flush.store(true, Ordering::Release);
        for _ in 0..100 {
            if !self.flush.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    //! Device-free tests for the seek-discard splice. They drive the real
    //! real-time fill ([`Fill`]) against an in-memory ring, so the sample-to-sample
    //! discontinuity a listener hears on seek is measured with no cpal device.
    use super::*;
    use std::collections::VecDeque;

    /// 16-bit source value 100 at canonical 24-bit scale (`100 << 8`) — full-scale
    /// steady pre-seek audio. Matches the deck fade-in test so the two curves line
    /// up by inspection.
    const FULL: i32 = 25_600;
    /// Fade length (interleaved samples) used in the tests: equals the deck's
    /// `fade_len` at 48 kHz mono, and a multiple of 4 so the smoothstep waypoints
    /// land on exact dyadic fractions of `FULL`.
    const FADE_LEN: u32 = 384;

    /// In-memory stand-in for the ring consumer: pops buffered samples FIFO and
    /// records whether it was cleared.
    struct FakeRing {
        buf: VecDeque<i32>,
        cleared: bool,
    }
    impl FakeRing {
        /// A ring holding `n` samples of steady full-scale pre-seek audio.
        fn steady(n: usize) -> Self {
            Self {
                buf: std::iter::repeat_n(FULL, n).collect(),
                cleared: false,
            }
        }
    }
    impl Ring for FakeRing {
        fn pop(&mut self) -> Option<i32> {
            self.buf.pop_front()
        }
        fn clear(&mut self) {
            self.buf.clear();
            self.cleared = true;
        }
    }

    fn max_abs_delta(xs: &[i32]) -> i64 {
        xs.windows(2)
            .map(|w| i64::from(w[1]).abs_diff(i64::from(w[0])) as i64)
            .max()
            .unwrap_or(0)
    }

    /// Emit `steady` samples of pre-seek audio, then raise the discard flag and
    /// service it (one callback: `begin` once, then `after` `next` calls). Returns
    /// the full output stream (canonical i32, underrun → 0).
    fn run_seek_discard(fade_len: u32, steady: usize, after: usize) -> (Vec<i32>, FakeRing, bool) {
        let mut fill = Fill::new(fade_len);
        let mut ring = FakeRing::steady(steady + after + 8);
        let flush = AtomicBool::new(false);
        let mut out = Vec::new();
        for _ in 0..steady {
            out.push(fill.next(&mut ring, &flush, 1.0).unwrap_or(0));
        }
        // Seek: the producer requests the discard, then blocks for the ack.
        flush.store(true, Ordering::Release);
        fill.begin(&flush, &mut ring);
        for _ in 0..after {
            out.push(fill.next(&mut ring, &flush, 1.0).unwrap_or(0));
        }
        let acked = !flush.load(Ordering::Acquire);
        (out, ring, acked)
    }

    #[test]
    fn seek_discard_without_fade_steps_to_silence() {
        // Characterizes the click with the fade disabled (`fade_len == 0`, the
        // instant-clear fallback): the output steps from the last buffered sample
        // (FULL) straight to 0 the moment the flush is serviced. That full-scale
        // step IS the click — the RED number we measure.
        let (out, ring, acked) = run_seek_discard(0, 64, 64);
        assert_eq!(out[63], FULL, "last pre-seek sample is full scale");
        assert_eq!(out[64], 0, "instant clear drops straight to silence");
        assert_eq!(
            max_abs_delta(&out[63..]),
            i64::from(FULL),
            "hard step FULL→0 = the click"
        );
        assert!(ring.cleared && acked, "cleared + acked immediately");
    }

    #[test]
    fn seek_discard_fade_declicks_the_splice() {
        // With the fade armed the splice must be smooth: the buffered head ramps
        // from full down to zero, so the largest adjacent step is a tiny fraction
        // of a hard FULL→0 jump.
        let (out, _ring, _acked) = run_seek_discard(FADE_LEN, 64, FADE_LEN as usize + 64);
        let delta = max_abs_delta(&out[63..]);
        assert!(
            delta < i64::from(FULL) / 50,
            "splice delta {delta} must be «FULL/50 ({}); a hard clear would be {FULL}",
            FULL / 50
        );
    }

    #[test]
    fn seek_discard_fade_is_monotonic_to_zero_with_exact_waypoints() {
        // Service the discard with no steady prefix, so index 0 is the first faded
        // sample.
        let mut fill = Fill::new(FADE_LEN);
        let mut ring = FakeRing::steady(FADE_LEN as usize + 64);
        let flush = AtomicBool::new(false);
        flush.store(true, Ordering::Release);
        fill.begin(&flush, &mut ring);
        let fade: Vec<i32> = (0..FADE_LEN as usize + 8)
            .map(|_| fill.next(&mut ring, &flush, 1.0).unwrap_or(0))
            .collect();

        // Starts at full — continuous with the last pre-seek sample (t=1 ⇒ g=1).
        assert_eq!(fade[0], FULL, "first faded sample is full level");
        // Monotonically non-increasing across the whole fade (a clean descent).
        for i in 1..FADE_LEN as usize {
            assert!(
                fade[i] <= fade[i - 1],
                "fade must fall at {i}: {} > {}",
                fade[i],
                fade[i - 1]
            );
        }
        // Falling-smoothstep waypoints are exact (FULL × dyadic fractions):
        //   t=0.75 → 0.84375 → 21600 ; t=0.5 → 0.5 → 12800 ; t=0.25 → 0.15625 → 4000.
        let n = FADE_LEN as usize;
        assert_eq!(fade[n / 4], 21_600, "quarter in ⇒ smoothstep(0.75)");
        assert_eq!(fade[n / 2], 12_800, "half in ⇒ smoothstep(0.5)");
        assert_eq!(
            fade[3 * n / 4],
            4_000,
            "three-quarters in ⇒ smoothstep(0.25)"
        );
        // Lands at silence; the remainder is cleared and the ack released.
        assert_eq!(fade[n - 1], 0, "fade ends at zero");
        assert!(
            ring.cleared,
            "remaining pre-seek audio cleared after the fade"
        );
        assert!(
            !flush.load(Ordering::Acquire),
            "discard ack released after the clear"
        );
        assert_eq!(fade[n + 4], 0, "stays silent past the fade (no re-trigger)");
    }

    #[test]
    fn fade_out_respects_volume_gain() {
        // The fade multiplies on top of the smoothed volume gain, so a half-volume
        // stream fades out from half scale, not full.
        let mut fill = Fill::new(FADE_LEN);
        let mut ring = FakeRing::steady(FADE_LEN as usize + 8);
        let flush = AtomicBool::new(false);
        // Pre-ramp the volume gain to a steady 0.5 so the first faded sample is
        // half scale (not mid-ramp).
        fill.cur_gain = 0.5;
        flush.store(true, Ordering::Release);
        fill.begin(&flush, &mut ring);
        let first = fill.next(&mut ring, &flush, 0.5).unwrap_or(0);
        assert_eq!(first, FULL / 2, "fade starts from the volume-scaled level");
    }
}
