#![no_std]
#![no_main]

//! STM32H743 (Nucleo-H743ZI) Embassy firmware — SAI/I²S audio output.
//!
//! Sub-project A of #32: brings up SAI1 as an I²S master transmitter over a DMA
//! ring and streams a square-wave test tone (440 Hz left / 659 Hz right) to an
//! external PCM5102 DAC. **NOT yet validated on hardware** — no board was
//! available at authoring time; this cross-builds for thumbv7em and is
//! flash-ready, but the clock tree, SAI I²S register config, pins, and DMA are
//! best-effort and must be bench-verified (see `sai_out` "VERIFY ON HARDWARE"
//! and the design doc `docs/superpowers/specs/2026-10-08-dap-sai-output-design.md`).

use defmt::info;
use defmt_rtt as _; // global defmt logger over RTT
use panic_probe as _; // panic handler

use embassy_executor::Spawner;
use embassy_stm32::sai::{Sai, split_subblocks};
use embassy_stm32::{bind_interrupts, dma, peripherals};
use static_cell::StaticCell;

mod sai_out;
/// SD-card byte source (phase B). Kept compiled; unused until the board SD stack
/// is wired.
mod sd_source;

use sai_out::{CHUNK, SaiOut};

bind_interrupts!(struct Irqs {
    DMA1_STREAM0 => dma::InterruptHandler<peripherals::DMA1_CH0>;
});

/// SAI DMA ring buffer — must live in DMA-reachable SRAM (D-cache is off at H743
/// reset, so default `.bss` AXI SRAM is fine; see `sai_out` notes).
static DMA_BUF: StaticCell<[u32; CHUNK]> = StaticCell::new();

const SAMPLE_RATE: u32 = 48_000;
/// ~ -3 dBFS of the 24-bit range (left-justified later into the 32-bit slot).
const AMPLITUDE: i32 = 0x0040_0000;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // TODO(hardware): configure the H7 clock tree so the SAI1 kernel clock yields
    // MCLK = 256·48 kHz = 12.288 MHz (a PLL leg + `rcc.mux.sai1sel`). Default
    // clocks here are NOT correct for 48 kHz output — bench item (see plan T2).
    let p = embassy_stm32::init(Default::default());
    info!("soul-audio-firmware: SAI bring-up");

    // SAI1 sub-block A, master-TX I²S with MCLK, over the DMA ring buffer.
    // Pins (Nucleo-H743ZI, SAI1_A AF): SCK=PE5, SD=PE6, FS=PE4, MCLK=PE2 —
    // VERIFY against the board pinout + embassy's pin traits.
    let dma_buf = DMA_BUF.init([0u32; CHUNK]);
    let (sub_a, _sub_b) = split_subblocks(p.SAI1);
    let sai = Sai::new_asynchronous_with_mclk(
        sub_a,
        p.PE5, // SCK_A  (bit clock)
        p.PE6, // SD_A   (data)
        p.PE4, // FS_A   (LRCK)
        p.PE2, // MCLK_A
        p.DMA1_CH0,
        dma_buf,
        Irqs,
        sai_out::i2s_config(),
    );
    let mut out = SaiOut::new(sai);

    // Square-wave test tone: left 440 Hz, right 659 Hz (recognizable first-light
    // signal). Integer phase accumulators — no `libm`.
    let period_l = SAMPLE_RATE / 440; // samples per full cycle
    let period_r = SAMPLE_RATE / 659;
    let (mut ph_l, mut ph_r) = (0u32, 0u32);
    let mut buf = [0i32; CHUNK];

    loop {
        for frame in buf.chunks_exact_mut(2) {
            frame[0] = if ph_l < period_l / 2 {
                AMPLITUDE
            } else {
                -AMPLITUDE
            };
            frame[1] = if ph_r < period_r / 2 {
                AMPLITUDE
            } else {
                -AMPLITUDE
            };
            ph_l += 1;
            if ph_l >= period_l {
                ph_l = 0;
            }
            ph_r += 1;
            if ph_r >= period_r {
                ph_r = 0;
            }
        }
        if out.play(&buf).await.is_err() {
            defmt::warn!("SAI write error (underrun?)");
        }
    }
}
