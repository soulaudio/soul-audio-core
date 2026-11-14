//! A direct-form-I **biquad IIR equalizer** — the real-time DSP primitive the
//! deck applies to decoded PCM, and the `DSP / EQ` stage of the signal path.
//!
//! Core only **applies** coefficients: a handful of multiply-adds per sample, so
//! this stays strictly `no_std` with no transcendental math. The coefficient
//! **design** (peaking / shelf filters, which needs `sin`/`cos`/`sqrt`/`pow`)
//! lives on the std host, which sets each band with [`Biquad::new`]. A bypass
//! band ([`Biquad::bypass`], the default) is a bit-exact passthrough.

/// The most channels an [`Equalizer`] keeps independent filter state for (matches
/// the deck's channel cap). Audio with more channels passes through unprocessed.
pub const MAX_CHANNELS: usize = 8;

/// A direct-form-I biquad section applying coefficients already normalised by
/// `a0`:
///
/// `y[n] = b0·x[n] + b1·x[n-1] + b2·x[n-2] − a1·y[n-1] − a2·y[n-2]`
///
/// The default is a flat **bypass** (`b0 = 1`, the rest `0`), a passthrough.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::bypass()
    }
}

impl Biquad {
    /// A flat passthrough section (`y = x`).
    #[must_use]
    pub const fn bypass() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    /// A section from coefficients **already normalised by `a0`** (so `a0 = 1`):
    /// the host divides the RBJ-cookbook coefficients through by `a0` before
    /// calling this. `b*` are the feed-forward and `a1`/`a2` the feedback taps.
    #[must_use]
    pub const fn new(b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) -> Self {
        Self { b0, b1, b2, a1, a2 }
    }

    /// Whether this section is a flat passthrough (so processing can skip it).
    #[must_use]
    pub fn is_bypass(&self) -> bool {
        *self == Self::bypass()
    }

    /// Advance one sample through this section, updating `s` (the per-channel
    /// delay line for THIS section).
    #[inline]
    fn tick(&self, x: f32, s: &mut BiquadState) -> f32 {
        let y = self.b0 * x + self.b1 * s.x1 + self.b2 * s.x2 - self.a1 * s.y1 - self.a2 * s.y2;
        s.x2 = s.x1;
        s.x1 = x;
        s.y2 = s.y1;
        s.y1 = y;
        y
    }
}

/// Per-channel, per-section delay line (`x[n-1]`, `x[n-2]`, `y[n-1]`, `y[n-2]`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

/// A `BANDS`-band equalizer: `BANDS` biquad sections applied in series, per
/// channel, over up to [`MAX_CHANNELS`] interleaved channels. Bands default to
/// bypass; set them with [`Self::set_band`]. `enabled = false` is a hard bypass.
///
/// `no_std` / no-alloc: the band coefficients and the per-channel delay lines are
/// fixed-size arrays. Processing is in `f32`; the deck converts to/from its `i32`
/// canonical samples at the call site (and clamps on the way back).
pub struct Equalizer<const BANDS: usize> {
    bands: [Biquad; BANDS],
    state: [[BiquadState; BANDS]; MAX_CHANNELS],
    enabled: bool,
}

impl<const BANDS: usize> Default for Equalizer<BANDS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const BANDS: usize> Equalizer<BANDS> {
    /// A flat (all-bypass), enabled equalizer with cleared history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bands: [Biquad::bypass(); BANDS],
            state: [[BiquadState::default(); BANDS]; MAX_CHANNELS],
            enabled: true,
        }
    }

    /// Set band `i`'s coefficients (ignored if `i >= BANDS`). Does not touch the
    /// delay lines, so a live coefficient change doesn't click.
    pub fn set_band(&mut self, i: usize, band: Biquad) {
        if let Some(slot) = self.bands.get_mut(i) {
            *slot = band;
        }
    }

    /// Band `i`'s current coefficients, or `None` if `i >= BANDS`.
    #[must_use]
    pub fn band(&self, i: usize) -> Option<Biquad> {
        self.bands.get(i).copied()
    }

    /// Enable/disable the whole EQ. Disabled is a hard passthrough (and the host
    /// should also skip calling [`Self::process`], but it's a no-op regardless).
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Whether the EQ is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether processing would change the signal at all: enabled AND at least one
    /// non-bypass band. The host can skip the `i32`↔`f32` round-trip when this is
    /// `false`.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.bands.iter().any(|b| !b.is_bypass())
    }

    /// Clear all filter history — call on seek / track change so a stale delay
    /// line can't ring into the new audio.
    pub fn reset(&mut self) {
        self.state = [[BiquadState::default(); BANDS]; MAX_CHANNELS];
    }

    /// Process `buf` in place as `channels` interleaved channels: each sample runs
    /// through every band in series, carrying that channel's own delay lines.
    /// A no-op when disabled, with no bands, with zero channels, or with more than
    /// [`MAX_CHANNELS`] channels (passes through unprocessed).
    pub fn process(&mut self, buf: &mut [f32], channels: usize) {
        if !self.enabled || BANDS == 0 || channels == 0 || channels > MAX_CHANNELS {
            return;
        }
        for (i, x) in buf.iter_mut().enumerate() {
            let ch = i % channels;
            let mut v = *x;
            for b in 0..BANDS {
                v = self.bands[b].tick(v, &mut self.state[ch][b]);
            }
            *x = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;
    use rustfft::{FftPlanner, num_complex::Complex};

    /// Test-only RBJ "Audio EQ Cookbook" peaking filter design (uses std math —
    /// this is exactly the kind of transcendental coefficient design that lives on
    /// the host, not in the `no_std` core). Produces `a0`-normalised coefficients.
    fn design_peaking(f0: f32, q: f32, gain_db: f32, fs: f32) -> Biquad {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / (2.0 * q);
        let cos_w0 = w0.cos();
        let a0 = 1.0 + alpha / a;
        Biquad::new(
            (1.0 + alpha * a) / a0,
            (-2.0 * cos_w0) / a0,
            (1.0 - alpha * a) / a0,
            (-2.0 * cos_w0) / a0,
            (1.0 - alpha / a) / a0,
        )
    }

    /// Magnitude (dB) of the filter's response at `freq`, measured from its
    /// impulse response via an FFT.
    fn response_db(eq: &mut Equalizer<4>, freq: f32, fs: f32, n: usize) -> f32 {
        let mut buf = vec![0.0f32; n];
        buf[0] = 1.0; // unit impulse, mono
        eq.reset();
        eq.process(&mut buf, 1);
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(n);
        let mut spec: Vec<Complex<f32>> = buf.iter().map(|&x| Complex::new(x, 0.0)).collect();
        fft.process(&mut spec);
        let bin = (freq * n as f32 / fs).round() as usize;
        20.0 * spec[bin].norm().log10()
    }

    #[test]
    fn bypass_is_identity() {
        let mut eq = Equalizer::<4>::new();
        let mut buf = [0.1, -0.3, 0.7, -0.9, 0.2, 0.5];
        let orig = buf;
        eq.process(&mut buf, 2);
        assert_eq!(buf, orig, "an all-bypass EQ leaves the signal untouched");
        assert!(!eq.is_active(), "all-bypass is inactive");
    }

    #[test]
    fn disabled_is_passthrough() {
        let mut eq = Equalizer::<4>::new();
        eq.set_band(0, design_peaking(1000.0, 1.0, 6.0, 48_000.0));
        assert!(eq.is_active(), "a set band makes it active");
        eq.set_enabled(false);
        assert!(!eq.is_active(), "disabled is inactive");
        let mut buf = [0.1, -0.3, 0.7, -0.9];
        let orig = buf;
        eq.process(&mut buf, 1);
        assert_eq!(buf, orig, "a disabled EQ is a hard passthrough");
    }

    #[test]
    fn peaking_band_boosts_its_center_frequency_and_leaves_the_rest_flat() {
        let fs = 48_000.0;
        let n = 8192;
        let mut eq = Equalizer::<4>::new();
        eq.set_band(0, design_peaking(1000.0, 1.0, 6.0, fs));
        let at_fc = response_db(&mut eq, 1000.0, fs, n);
        let low = response_db(&mut eq, 80.0, fs, n);
        let high = response_db(&mut eq, 12_000.0, fs, n);
        assert!(
            (at_fc - 6.0).abs() < 0.5,
            "+6 dB peaking at 1 kHz should read ~+6 dB there, got {at_fc:.2} dB"
        );
        assert!(low.abs() < 1.0, "flat well below fc, got {low:.2} dB");
        assert!(high.abs() < 1.0, "flat well above fc, got {high:.2} dB");
    }

    #[test]
    fn bands_compose_in_series() {
        // Two +6 dB peaks at the same fc stack to ~+12 dB.
        let fs = 48_000.0;
        let n = 8192;
        let mut eq = Equalizer::<4>::new();
        eq.set_band(0, design_peaking(1000.0, 1.0, 6.0, fs));
        eq.set_band(1, design_peaking(1000.0, 1.0, 6.0, fs));
        let at_fc = response_db(&mut eq, 1000.0, fs, n);
        assert!(
            (at_fc - 12.0).abs() < 0.6,
            "two +6 dB bands stack to ~+12 dB, got {at_fc:.2} dB"
        );
    }

    #[test]
    fn channels_carry_independent_state() {
        // A sharp filter on a stereo buffer: each channel must filter only its own
        // samples. If the state were shared, the interleaved streams would bleed.
        let mut shared = Equalizer::<2>::new();
        shared.set_band(0, design_peaking(1000.0, 4.0, 9.0, 48_000.0));
        // Left = impulse, right = silence.
        let mut stereo = [1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        shared.process(&mut stereo, 2);
        // The right channel started silent and gets no input, so it stays exactly
        // zero — only possible if its delay line is independent of the left's.
        for i in (1..stereo.len()).step_by(2) {
            assert_eq!(stereo[i], 0.0, "right channel bled from the left at {i}");
        }
        // The left channel, fed an impulse, produces a non-trivial ringing tail.
        assert!(
            stereo.iter().step_by(2).any(|&x| x != 0.0 && x != 1.0),
            "left channel should ring from the impulse"
        );
    }

    #[test]
    fn reset_clears_the_ringing_tail() {
        let mut eq = Equalizer::<2>::new();
        eq.set_band(0, design_peaking(1000.0, 4.0, 9.0, 48_000.0));
        let mut buf = [1.0f32, 0.0, 0.0, 0.0];
        eq.process(&mut buf, 1); // leaves a non-zero delay line
        eq.reset();
        // After reset, a zero input must produce exactly zero output (no ringing
        // from the previous block's history).
        let mut quiet = [0.0f32; 4];
        eq.process(&mut quiet, 1);
        assert_eq!(quiet, [0.0; 4], "reset must clear the filter history");
    }
}
