//! **DoP output policy**: given a DSD source and what the output device can
//! actually take, decide how to deliver it — native DSD, DoP, or decimated PCM
//! — and say what carrier the device must be opened at.
//!
//! The adapters that do the work already exist ([`crate::dsd`]). What was
//! missing is the decision, and it is worth having in one place, platform-free
//! and tested, because every one of its details is a way to send a DAC
//! something it will reproduce as noise:
//!
//! - **The carrier rate is `dsd_rate / 16`**, not the DSD rate: DoP packs 16
//!   one-bit samples per channel into one 24-bit PCM word. DSD64 → 176.4 kHz,
//!   DSD128 → 352.8 kHz, DSD256 → 705.6 kHz, DSD512 → 1411.2 kHz. Open the
//!   device at the DSD rate and you get a DAC refusing the format, or playing
//!   at a sixteenth speed.
//! - **At least 24 bits must reach the device, unaltered.** The marker byte is
//!   the word's top byte; a 16-bit device truncates it away, and *any*
//!   processing — a volume multiply, a sample-rate conversion, a mixer — turns
//!   the markers into noise the DAC will happily play at full level. So DoP
//!   requires exclusive output with no gain, no resampling and no DSP. This is
//!   not a preference: the failure mode is loud.
//! - **Falling back is normal.** A device that cannot take the carrier rate, or
//!   cannot be held exclusively, gets decimated PCM — which plays correctly
//!   everywhere. The caller asks for the best mode and gets the best available
//!   one, with [`DopPlan::fell_back`] telling the UI which it was.

use crate::model::{DsdMode, DsdRate, SampleFormat};

/// DSD samples packed into one DoP word, per channel. Two bytes of 1-bit
/// samples (16 bits) sit in the low 16 bits of a 24-bit word, under the marker.
pub const DSD_BITS_PER_DOP_FRAME: u32 = 16;

/// What an output device can take — the facts the decision needs, named so a
/// platform layer can fill them in from WASAPI / CoreAudio / ALSA without this
/// module knowing about any of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DeviceCapabilities {
    /// The device can be driven exclusively (no OS mixer in the path). DoP
    /// without this is not DoP — the mixer would rewrite the markers.
    pub exclusive: bool,
    /// The device advertises DoP (a DAC that recognises the `0x05`/`0xFA`
    /// markers). Nothing in the PCM stream tells it apart from noise, so this
    /// has to come from the device/driver or the user.
    pub dop: bool,
    /// The device accepts a native 1-bit DSD stream (ASIO native DSD, a DAP's
    /// SAI straight to the DAC).
    pub native_dsd: bool,
    /// The highest PCM rate the device accepts, in Hz. The DoP carrier must fit
    /// under it.
    pub max_pcm_rate: u32,
    /// Bits the device actually receives per sample. DoP needs ≥ 24: the marker
    /// is the top byte of a 24-bit word.
    pub bits: u8,
}

/// How to deliver a DSD source to a device, and what the device must be set to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DopPlan {
    /// The delivery mode to build the decoder adapter for ([`crate::dsd`]).
    pub mode: DsdMode,
    /// The rate the output device must be opened at:
    /// - [`DsdMode::Dop`]: the DoP carrier, `dsd_rate / 16`.
    /// - [`DsdMode::Decimate`]: the decimated PCM rate the adapter produces.
    /// - [`DsdMode::Native`]: the 1-bit stream rate itself (the device is not
    ///   being fed PCM at all).
    pub device_rate: u32,
    /// The sample format the device must be opened with. DoP is 24-bit
    /// integer; decimated PCM is the adapter's own output format.
    pub format: SampleFormat,
    /// Whether this plan is a fallback from a better mode the device could not
    /// take — for the UI to say "DSD64 (decimated: device is not DoP-capable)"
    /// rather than silently sounding different from what was asked for.
    pub fell_back: bool,
    /// Whether this plan delivers the file's DSD bits to the DAC untouched
    /// (native or DoP). `false` for decimation, which is a conversion —
    /// correct, but not the same bits.
    pub bit_exact_dsd: bool,
}

/// The DoP carrier rate for a DSD rate: `dsd_rate / 16`.
///
/// DSD64 → 176 400 Hz, DSD128 → 352 800, DSD256 → 705 600, DSD512 → 1 411 200.
#[must_use]
pub const fn dop_carrier_rate(rate: DsdRate) -> u32 {
    rate.hz() / DSD_BITS_PER_DOP_FRAME
}

/// The rate [`crate::dsd::PlaybackDecimateAdapter`] produces for a DSD rate:
/// the 8:1 decimation of the 1-bit stream down to the 44.1 kHz family
/// (`dsd_rate / 64`), i.e. always 44 100 Hz — the rate every device takes.
#[must_use]
pub const fn decimated_rate(rate: DsdRate) -> u32 {
    // The playback decimator targets the CD-family base rate regardless of the
    // DSD rate, so a DSD512 file plays at the same device rate as a DSD64 one.
    let _ = rate;
    44_100
}

/// Choose how to deliver a `rate` DSD source to a device with `caps`,
/// preferring `preferred`.
///
/// The preference is honoured only when the device can actually take it:
///
/// | preferred | granted when |
/// |---|---|
/// | [`DsdMode::Native`] | the device takes native DSD |
/// | [`DsdMode::Dop`] | the device is exclusive **and** advertises DoP **and** takes ≥ 24 bits **and** accepts the carrier rate |
/// | [`DsdMode::Decimate`] | always |
///
/// A denied preference falls back — native to DoP, DoP to decimation — and the
/// plan records it ([`DopPlan::fell_back`]). Decimation always works, so this
/// never fails.
#[must_use]
pub fn plan(rate: DsdRate, caps: &DeviceCapabilities, preferred: DsdMode) -> DopPlan {
    let native = DopPlan {
        mode: DsdMode::Native,
        device_rate: rate.hz(),
        // Native DSD is not PCM at all; the format names what it is.
        format: SampleFormat::Dsd { rate },
        fell_back: false,
        bit_exact_dsd: true,
    };
    let carrier = dop_carrier_rate(rate);
    let dop = DopPlan {
        mode: DsdMode::Dop,
        device_rate: carrier,
        format: SampleFormat::Int { bits: 24 },
        fell_back: false,
        bit_exact_dsd: true,
    };
    let decimate = DopPlan {
        mode: DsdMode::Decimate,
        device_rate: decimated_rate(rate),
        format: SampleFormat::Int { bits: 24 },
        fell_back: false,
        bit_exact_dsd: false,
    };

    let dop_ok = caps.exclusive && caps.dop && caps.bits >= 24 && caps.max_pcm_rate >= carrier;

    match preferred {
        DsdMode::Native if caps.native_dsd => native,
        // A device without native DSD may still take DoP — that is the usual
        // case for a USB DAC, so try it before giving up on bit-exact DSD.
        DsdMode::Native | DsdMode::Dop if dop_ok => DopPlan {
            fell_back: preferred == DsdMode::Native,
            ..dop
        },
        DsdMode::Decimate => decimate,
        _ => DopPlan {
            fell_back: true,
            ..decimate
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DoP-capable USB DAC: exclusive, 24-bit, up to 768 kHz.
    fn dop_dac() -> DeviceCapabilities {
        DeviceCapabilities {
            exclusive: true,
            dop: true,
            native_dsd: false,
            max_pcm_rate: 768_000,
            bits: 24,
        }
    }

    /// A plain shared-mode output (the OS mixer).
    fn shared_output() -> DeviceCapabilities {
        DeviceCapabilities {
            exclusive: false,
            dop: false,
            native_dsd: false,
            max_pcm_rate: 192_000,
            bits: 32,
        }
    }

    #[test]
    fn the_dop_carrier_is_the_dsd_rate_over_sixteen() {
        assert_eq!(dop_carrier_rate(DsdRate::Dsd64), 176_400);
        assert_eq!(dop_carrier_rate(DsdRate::Dsd128), 352_800);
        assert_eq!(dop_carrier_rate(DsdRate::Dsd256), 705_600);
        assert_eq!(dop_carrier_rate(DsdRate::Dsd512), 1_411_200);
        // Which is 16 one-bit samples per channel per PCM word, exactly.
        for rate in [
            DsdRate::Dsd64,
            DsdRate::Dsd128,
            DsdRate::Dsd256,
            DsdRate::Dsd512,
        ] {
            assert_eq!(
                dop_carrier_rate(rate) * DSD_BITS_PER_DOP_FRAME,
                rate.hz(),
                "{rate:?}"
            );
        }
    }

    #[test]
    fn a_dop_dac_gets_dop_at_the_carrier_rate() {
        let p = plan(DsdRate::Dsd64, &dop_dac(), DsdMode::Dop);
        assert_eq!(p.mode, DsdMode::Dop);
        assert_eq!(p.device_rate, 176_400, "the carrier, not the DSD rate");
        assert_eq!(p.format, SampleFormat::Int { bits: 24 });
        assert!(p.bit_exact_dsd, "DoP delivers the file's DSD bits");
        assert!(!p.fell_back);
    }

    #[test]
    fn native_is_preferred_when_the_device_takes_it_and_falls_back_to_dop() {
        let mut caps = dop_dac();
        caps.native_dsd = true;
        let p = plan(DsdRate::Dsd128, &caps, DsdMode::Native);
        assert_eq!(p.mode, DsdMode::Native);
        assert_eq!(p.device_rate, DsdRate::Dsd128.hz());
        assert_eq!(
            p.format,
            SampleFormat::Dsd {
                rate: DsdRate::Dsd128
            }
        );
        assert!(p.bit_exact_dsd && !p.fell_back);

        // A USB DAC with no native path still gets bit-exact DSD, via DoP —
        // and the plan says it was a fallback.
        let p = plan(DsdRate::Dsd128, &dop_dac(), DsdMode::Native);
        assert_eq!(p.mode, DsdMode::Dop);
        assert_eq!(p.device_rate, 352_800);
        assert!(p.bit_exact_dsd);
        assert!(p.fell_back, "native was asked for and not granted");
    }

    /// Every way a device can fail to carry DoP, one at a time. Each of these
    /// would be audible as full-level noise if DoP were sent anyway.
    #[test]
    fn dop_is_refused_whenever_the_markers_could_not_survive() {
        let cases = [
            ("shared mode (the OS mixer would rewrite the markers)", {
                let mut c = dop_dac();
                c.exclusive = false;
                c
            }),
            ("the DAC does not advertise DoP", {
                let mut c = dop_dac();
                c.dop = false;
                c
            }),
            ("16-bit device (the marker byte is truncated away)", {
                let mut c = dop_dac();
                c.bits = 16;
                c
            }),
            ("the device cannot take the 176.4 kHz carrier", {
                let mut c = dop_dac();
                c.max_pcm_rate = 96_000;
                c
            }),
        ];
        for (why, caps) in cases {
            let p = plan(DsdRate::Dsd64, &caps, DsdMode::Dop);
            assert_eq!(p.mode, DsdMode::Decimate, "{why}");
            assert!(p.fell_back, "{why}: a denied preference must be reported");
            assert!(!p.bit_exact_dsd, "{why}: decimation is a conversion");
            assert_eq!(p.device_rate, 44_100, "{why}");
        }
    }

    #[test]
    fn a_shared_output_always_gets_decimated_pcm() {
        for preferred in [DsdMode::Native, DsdMode::Dop, DsdMode::Decimate] {
            let p = plan(DsdRate::Dsd256, &shared_output(), preferred);
            assert_eq!(p.mode, DsdMode::Decimate, "{preferred:?}");
            assert_eq!(p.device_rate, 44_100);
            assert_eq!(p.fell_back, preferred != DsdMode::Decimate);
        }
    }

    #[test]
    fn asking_for_decimation_is_never_a_fallback() {
        let mut caps = dop_dac();
        caps.native_dsd = true;
        let p = plan(DsdRate::Dsd64, &caps, DsdMode::Decimate);
        assert_eq!(p.mode, DsdMode::Decimate);
        assert!(!p.fell_back, "the user asked for PCM and got PCM");
        assert!(!p.bit_exact_dsd);
    }

    /// The high rates are where the carrier runs out of room: a 768 kHz device
    /// carries DSD256 but not DSD512, and must fall back for that file alone.
    #[test]
    fn a_high_dsd_rate_falls_back_only_when_the_carrier_does_not_fit() {
        let caps = dop_dac(); // 768 kHz ceiling
        let p = plan(DsdRate::Dsd256, &caps, DsdMode::Dop);
        assert_eq!(p.mode, DsdMode::Dop, "705.6 kHz fits under 768 kHz");
        let p = plan(DsdRate::Dsd512, &caps, DsdMode::Dop);
        assert_eq!(p.mode, DsdMode::Decimate, "1411.2 kHz does not");
        assert!(p.fell_back);
    }
}
