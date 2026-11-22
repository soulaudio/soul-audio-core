//! The cpal-free half of the output path: the SPSC ring, the real-time [`Fill`]
//! (volume smoothing + seek-discard fade) and [`push_blocking`]. Shared by
//! [`CpalSink`](crate::CpalSink), the WASAPI exclusive sink and the platform-neutral
//! [`ExclusiveEngine`](crate::exclusive_engine::ExclusiveEngine) — kept out of `sink.rs`
//! so the engine and its fake HAL build without cpal (no ALSA on Linux CI).

use std::sync::atomic::{AtomicBool, Ordering};

use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Producer, Split};
use soul_audio_core::SinkError;

/// Falling-smoothstep fade-out length for a seek-discard, in milliseconds. On a
/// seek the buffered pre-seek audio still sitting in the ring is ramped down to
/// zero over this window *before* the ring is cleared, so the output descends
/// smoothly to silence instead of stepping from the last buffered sample straight
/// to 0 — that step is a format-agnostic click. It pairs with the deck's rising
/// fade-IN on the post-seek side (`deck.rs`): old→0 here, brief silence, then new
/// 0→full there = a click-free splice. 8 ms ≈ a few hundred samples at 48 kHz, so
/// the ack is still released far inside `discard`'s ~200 ms bound.
pub(crate) const DISCARD_FADE_MS: u32 = 8;

/// The slice of the SPSC ring consumer the real-time fill needs. Abstracting it
/// lets the flush/fade logic in [`Fill`] be unit-tested against an in-memory fake
/// — the cpal callback is otherwise only reachable with a live output device.
pub(crate) trait Ring {
    /// Pop the next buffered sample, or `None` on underrun.
    fn pop(&mut self) -> Option<i32>;
    /// Drop all buffered samples.
    fn clear(&mut self);
}

/// Newtype over the cpal SPSC ring consumer. A concrete named type (rather than
/// the `<HeapRb<i32> as Split>::Cons` projection) so [`Ring`] can also be
/// implemented for the test fake without a coherence conflict.
pub(crate) struct CpalRing(pub(crate) <HeapRb<i32> as Split>::Cons);

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
pub(crate) struct Fill {
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
    pub(crate) fn new(fade_len: u32) -> Self {
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
    pub(crate) fn begin(&mut self, flush: &AtomicBool, ring: &mut impl Ring) {
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
    pub(crate) fn next(
        &mut self,
        ring: &mut impl Ring,
        flush: &AtomicBool,
        target: f32,
    ) -> Option<i32> {
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

/// Push **all** of `frames` into the ring, sleeping while it is full (the backpressure
/// that paces the pump to the device clock) — but return [`SinkError::Closed`] as soon
/// as `gone()` reports the device has disappeared (an unplugged DAC / headphones), since
/// then nothing will ever drain the ring and a plain loop would block forever.
pub(crate) fn push_blocking(
    prod: &mut impl Producer<Item = i32>,
    frames: &[i32],
    gone: impl Fn() -> bool,
) -> Result<(), SinkError> {
    if gone() {
        return Err(SinkError::Closed);
    }
    let mut off = 0;
    while off < frames.len() {
        off += prod.push_slice(&frames[off..]);
        if off < frames.len() {
            if gone() {
                return Err(SinkError::Closed);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Device-free tests for the seek-discard splice. They drive the real
    //! real-time fill ([`Fill`]) against an in-memory ring, so the sample-to-sample
    //! discontinuity a listener hears on seek is measured with no cpal device.
    use super::*;
    use ringbuf::traits::Observer;
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

    /// `max |x[i] − 2·x[i−1] + x[i−2]|` — the discrete second difference's peak.
    /// A smoothstep ramp (C¹ at both ends, bounded curvature) keeps this tiny
    /// (≈ FULL·6/FADE_LEN²); a slope break — a hard clear, or a *linear* ramp's
    /// corners — spikes it (a step ≈ 2·FULL, a linear corner ≈ FULL/FADE_LEN).
    fn second_difference_peak(xs: &[i32]) -> i64 {
        xs.windows(3)
            .map(|w| (i64::from(w[2]) - 2 * i64::from(w[1]) + i64::from(w[0])).abs())
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
        // No slope break anywhere across the fade-out (the real de-click gate,
        // complementing the monotonic check above). The audible shape is a steady
        // full level continuing INTO the fade and out to silence, so prepend two
        // steady-FULL samples to include the steady→fade junction: a smoothstep is
        // C¹ at t=1 (zero slope) so that junction is smooth, as is the fade→silence
        // junction at t=0. The peak second difference therefore stays ≈ FULL·6/n²
        // (a couple of LSB), orders of magnitude below a hard clear's 0→FULL step
        // (≈ 2·FULL) or even a linear ramp's corner (≈ FULL/n ≈ 66). Bound K is set
        // well under that corner so the test proves *smoothstep*, not merely
        // *some* ramp.
        let shaped: Vec<i32> = [FULL, FULL]
            .into_iter()
            .chain(fade.iter().copied())
            .collect();
        let d2 = second_difference_peak(&shaped);
        let k = i64::from(FULL) / 1000; // 25 — below a linear corner (~66), far below a step
        assert!(
            d2 <= k,
            "fade-out second-difference peak {d2} exceeds {k} — the ramp has a slope \
             break (not a bounded-curvature smoothstep); a hard clear would be ~{}",
            2 * i64::from(FULL)
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
    fn push_blocking_writes_everything_when_there_is_room() {
        let (mut prod, mut cons) = HeapRb::<i32>::new(16).split();
        push_blocking(&mut prod, &[1, 2, 3, 4], || false).unwrap();
        assert_eq!(cons.try_pop(), Some(1));
        assert_eq!(cons.occupied_len(), 3);
    }

    #[test]
    fn push_blocking_on_a_gone_device_fails_immediately() {
        let (mut prod, _cons) = HeapRb::<i32>::new(16).split();
        assert_eq!(
            push_blocking(&mut prod, &[1], || true),
            Err(SinkError::Closed)
        );
        assert_eq!(prod.occupied_len(), 0, "nothing written to a dead device");
    }

    #[test]
    fn push_blocking_returns_closed_instead_of_hanging_on_a_full_ring() {
        // The device dies mid-write with the ring full: nobody will drain it. Before the
        // fix this looped forever; now it returns promptly with Closed.
        use std::sync::atomic::AtomicBool;
        let (mut prod, _cons) = HeapRb::<i32>::new(4).split();
        let dead = std::sync::Arc::new(AtomicBool::new(false));
        let d = std::sync::Arc::clone(&dead);
        let killer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            d.store(true, Ordering::Relaxed);
        });
        let t = std::time::Instant::now();
        let r = push_blocking(&mut prod, &[0; 64], || dead.load(Ordering::Relaxed));
        killer.join().unwrap();
        assert_eq!(r, Err(SinkError::Closed));
        assert!(t.elapsed() < std::time::Duration::from_millis(500));
    }

    #[test]
    fn push_blocking_waits_for_the_consumer_while_the_device_lives() {
        let (mut prod, mut cons) = HeapRb::<i32>::new(4).split();
        let drainer = std::thread::spawn(move || {
            let mut got = Vec::new();
            while got.len() < 32 {
                if let Some(v) = cons.try_pop() {
                    got.push(v);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            got
        });
        let frames: Vec<i32> = (0..32).collect();
        push_blocking(&mut prod, &frames, || false).unwrap();
        assert_eq!(
            drainer.join().unwrap(),
            frames,
            "backpressure, in order, nothing lost"
        );
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
