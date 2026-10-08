//! SAI1 master-transmit I²S output for an external PCM5102 DAC — **firmware
//! only**, and **NOT yet validated on hardware** (sub-project A of #32; no board
//! available at authoring time). It cross-builds for thumbv7em; the audio config
//! below is best-effort per the embassy-stm32 0.6 API + the I²S/PCM5102 spec and
//! must be bench-verified.
//!
//! # PCM5102 wiring (bench, when a board exists)
//! - `SCK`  ← SAI `MCLK_A`  (or tie `SCK=GND` and set `config.nodiv=true` to run
//!   the PCM5102 off BCLK via its internal PLL)
//! - `BCK`  ← SAI `SCK_A` (bit clock)   · `LRCK` ← SAI `FS_A`   · `DIN` ← SAI `SD_A`
//! - `FMT`  = GND (I²S) · `XSMT` = 3V3 (un-mute) · `FLT/DEMP/A3V3` per board
//!
//! # VERIFY ON HARDWARE
//! - MCLK = 256·fs (12.288 MHz @ 48 kHz), LRCK = 48 kHz, BCLK = 64·fs — scope it.
//! - If the tone is distorted/channel-swapped: `bit_order` (I²S = MSB-first),
//!   `frame_sync_polarity` (LRCK low = left), `frame_sync_offset`, `slot_size`.
//! - If the DMA faults: the ring buffer must be in DMA-reachable SRAM with the
//!   D-cache off (it is, at H743 reset) or an MPU non-cacheable region.

use embassy_stm32::peripherals::SAI1;
use embassy_stm32::sai::{
    BitOrder, ClockStrobe, Config, DataSize, Error, FrameSyncDefinition, FrameSyncOffset,
    FrameSyncPolarity, MasterClockDivider, Mode, Protocol, Sai, SlotSize, StereoMono, TxRx,
};

/// Philips I²S, 24-bit data in 32-bit slots, stereo, SAI as master with MCLK.
/// (`master_clock_divider`/FS-active-length are left to bench tuning — see the
/// module's "VERIFY ON HARDWARE".)
pub fn i2s_config() -> Config {
    let mut c = Config::default(); // Master + Transmitter defaults
    c.mode = Mode::Master;
    c.tx_rx = TxRx::Transmitter;
    c.protocol = Protocol::Free;
    c.slot_size = SlotSize::Channel32; // 32-bit wide slot
    c.data_size = DataSize::Data24; // 24-bit audio data
    c.stereo_mono = StereoMono::Stereo;
    c.bit_order = BitOrder::MsbFirst; // I²S is MSB-first
    c.frame_sync_offset = FrameSyncOffset::BeforeFirstBit; // Philips: FS one BCLK early
    c.frame_sync_polarity = FrameSyncPolarity::ActiveLow; // LRCK low = left
    c.frame_sync_definition = FrameSyncDefinition::ChannelIdentification;
    c.frame_length = 64; // 2 × 32-bit slots
    c.clock_strobe = ClockStrobe::Falling; // I²S data valid on the falling edge
    c.master_clock_divider = MasterClockDivider::DIV1; // TODO(hw): tune for 256·fs
    c
}

/// Owns the SAI peripheral configured for I²S master-TX over a DMA ring buffer.
/// The `i32` streaming seam phase C's deck integration will drive.
pub struct SaiOut<'d> {
    sai: Sai<'d, SAI1, u32>,
    scratch: [u32; CHUNK],
}

/// Interleaved stereo samples per `play` call (frames = CHUNK / 2).
pub const CHUNK: usize = 2048;

impl<'d> SaiOut<'d> {
    /// Wrap an already-constructed master-TX `Sai` (built in `main` with the
    /// board's SAI1 pins + DMA + ring buffer).
    pub fn new(sai: Sai<'d, SAI1, u32>) -> Self {
        Self {
            sai,
            scratch: [0; CHUNK],
        }
    }

    /// Stream one chunk of interleaved stereo frames. Converts the pipeline's
    /// canonical `i32` samples (24-bit audio in the low bits) to the SAI 32-bit
    /// slot word (left-justified 24-bit), then awaits the DMA ring write —
    /// back-pressure paces the producer to the audio clock.
    ///
    /// `frames.len()` must be ≤ [`CHUNK`]; extra is ignored.
    pub async fn play(&mut self, frames: &[i32]) -> Result<(), Error> {
        let n = frames.len().min(CHUNK);
        for (dst, &s) in self.scratch[..n].iter_mut().zip(frames) {
            // 24-bit sample (low bits) → left-justified in a 32-bit I²S slot.
            *dst = (s as u32) << 8;
        }
        self.sai.write(&self.scratch[..n]).await
    }
}
