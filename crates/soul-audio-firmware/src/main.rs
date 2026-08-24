#![no_std]
#![no_main]

//! STM32H743 Embassy shell for `soul-audio-core`.
//!
//! TEMPLATE: this demonstrates the cross-crate wiring (Embassy init → construct
//! a decoder from an `embedded-io` byte source → log the result over RTT).
//! Decoders are stubs, and real SAI/I2S DMA output is intentionally out of
//! scope — both are marked with `TODO`.

use defmt::info;
use defmt_rtt as _; // global defmt logger over RTT
use panic_probe as _; // panic handler

use embassy_executor::Spawner;
use soul_audio_core::{SliceSource, open};

/// SD-card byte source (an `embedded-io` `SeekSource` over an `embedded-sdmmc`
/// file). Firmware-only — the core never depends on `embedded-sdmmc`. Wired to a
/// real card during the SAI/DMA firmware work.
mod sd_source;

/// Placeholder audio source. Replace with a real one (SD card, flash, USB, …).
static AUDIO: &[u8] = &[];

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // Bring up the HAL with default clocks. NOTE: real audio needs explicit
    // H7 clock + SAI configuration — deliberately out of template scope.
    let _p = embassy_stm32::init(Default::default());
    info!("soul-audio-firmware up");

    // Cross-crate wiring: probe an embedded-io byte source and build a decoder.
    // On the host this would be a file via `embedded-io-adapters`; here it's a
    // `SliceSource` over in-flash bytes (which is itself a `BufSource`, so it
    // feeds `open` directly). Leaf decoders are stubs → decode is Unsupported.
    let source = SliceSource::new(AUDIO);
    match open(source) {
        Ok(_decoder) => {
            info!("decoder wired");
            // TODO: stream `Decoder::decode_into` output to SAI/I2S DMA.
        }
        Err(_) => info!("no decoder for source — implement soul-audio-core::formats"),
    }

    // Idle heartbeat. Replace with a real SAI DMA streaming loop.
    loop {
        embassy_time::Timer::after_millis(1000).await;
    }
}
