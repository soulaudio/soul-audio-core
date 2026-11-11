#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-loudness` — a portable `no_std`, no-alloc, `libm`-free streaming
//! loudness analyzer implementing **ITU-R BS.1770-4** integrated loudness (the
//! measurement **EBU R128** and **ReplayGain 2.0** are built on), plus BS.1770
//! true-peak.
//!
//! Feed interleaved PCM in arbitrary chunk sizes; read back integrated LUFS,
//! max true-peak (dBTP), and a ReplayGain-2.0 track gain. Gating uses a
//! fixed-bin histogram (libebur128's histogram mode) so the analyzer holds
//! O(1) memory regardless of track length — it runs on the DAP as well as the
//! desktop.

mod analyzer;
mod math;

pub use analyzer::{NO_LOUDNESS, R128Analyzer, REPLAYGAIN2_TARGET_LUFS};
