//! Tagless MPEG Layer I/II (`.mp1`/`.mp2`) streams must still report a track
//! duration: `sniff` routes a bare MPEG frame (no ID3) to the facts reader, and
//! the (now layer-aware) frame parser derives the length from the bitrate +
//! samples-per-frame. Before this, only Layer III was parsed, so an `.mp2`
//! showed `00:00` in the library.
//!
//! `std`-only (file I/O via `embedded-io-adapters`).
#![cfg(feature = "std")]

use std::fs::File;
use std::path::{Path, PathBuf};

use embedded_io_adapters::std::FromStd;
use soul_audio_tags::{Field, Picture, StreamFacts, TagSink, read};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// Facts-only sink: these streams carry no text tags or pictures.
struct Null;
impl TagSink for Null {
    fn text(&mut self, _field: Option<Field>, _raw_key: &str, _value: &str) {}
    fn picture(&mut self, _pic: &Picture) {}
}

fn facts(file: &str) -> StreamFacts {
    let mut src = FromStd::new(File::open(fixture(file)).expect("open fixture"));
    let mut scratch = [0u8; 4096];
    read(&mut src, &mut scratch, &mut Null).expect("read facts")
}

#[test]
fn bare_layer2_mp2_reports_duration() {
    let f = facts("tone.mp2");
    assert!(
        f.total_frames.unwrap_or(0) > 0,
        "Layer II (.mp2) reported no duration: {f:?}"
    );
    assert!(f.sample_rate.is_some(), "no sample rate: {f:?}");
    assert!(f.channels.is_some(), "no channel count: {f:?}");
}

#[test]
fn bare_layer1_mp1_reports_duration() {
    let f = facts("tone-layer1.mp1");
    assert!(
        f.total_frames.unwrap_or(0) > 0,
        "Layer I (.mp1) reported no duration: {f:?}"
    );
}
