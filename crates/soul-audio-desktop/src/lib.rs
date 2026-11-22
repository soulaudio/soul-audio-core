//! Shared **std desktop-playback** building blocks for `soul-audio`, extracted
//! from the CLI so the desktop app (Plan 3) and the CLI share one
//! implementation: concrete file sources, the owned decoder + `open_track`, and
//! (behind the `playback` feature) the cpal sink. Pure canonical-24-bit →
//! device-format conversion lives in `soul_audio_core::convert`; the sink calls
//! it, so a future DMA sink shares the same math.
#![forbid(unsafe_code)]
#![deny(unconditional_recursion)]

/// CoreAudio exclusive / bit-perfect output (macOS only, behind `coreaudio-exclusive`).
#[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
pub mod coreaudio_exclusive;
pub mod decoder;
/// The platform-neutral exclusive-output engine (hog / native rate / restore), driven by a
/// `Hal` — CoreAudio on macOS, the fake device world in tests.
#[cfg(any(feature = "playback", feature = "test-util"))]
pub mod exclusive_engine;
/// Device-free exclusive-output policy (rate choice + physical-format ranking), unit-tested
/// on every OS and used by the CoreAudio exclusive sink.
pub mod exclusive_policy;
/// Scriptable fake HAL + device profiles for testing the exclusive engine.
#[cfg(feature = "test-util")]
pub mod fake_hal;
#[cfg(any(feature = "playback", feature = "test-util"))]
mod ring;
#[cfg(feature = "playback")]
pub mod sink;
pub mod sources;
/// WASAPI exclusive / bit-perfect output (Windows only, behind `wasapi-exclusive`).
#[cfg(all(windows, feature = "wasapi-exclusive"))]
pub mod wasapi_exclusive;

#[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
pub use coreaudio_exclusive::{
    CoreAudioExclusiveSink, CoreAudioHal, ExclusiveError, coreaudio_output_device_names, hog_owner,
    nominal_rate, physical_format_ladder,
};
pub use decoder::{
    OwnedDecoder, ReadSeek, Src, open_seekable_src, open_track, vorbis_arena, wavpack_pcm,
};
#[cfg(feature = "playback")]
pub use sink::{CpalSink, default_output_config, default_output_device_name, output_device_names};
pub use sources::{file_source, tcp_source};
#[cfg(all(windows, feature = "wasapi-exclusive"))]
pub use wasapi_exclusive::{ExclusiveError, WasapiExclusiveSink};
