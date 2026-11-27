//! ReplayGain acceptance test on a **real MP3 file**, read through the public
//! tag path (`read_owned`) — the end of the chain the unit tests in
//! `src/id3v2.rs` only exercise on synthetic in-memory tags.
//!
//! The fixture, `testdata/rva2-only.mp3`, is tagged with ID3v2.4 `RVA2`
//! relative-volume frames and **no** `TXXX replaygain_*` frames (see
//! `testdata/generate_rva2_mp3.py`), which is what tools that only write RVA2
//! (mp3gain's ID3v2 mode, taggers built on mutagen's `RVA2`) leave behind. A
//! player that only looks at `TXXX` re-analyzes such files for no reason.
//!
//! Every assertion is cross-checked against **lofty** (the mature reference
//! this crate is differentially tested against, see `tests/differential.rs`):
//! lofty parses the same fixture's RVA2 frames independently, so the expected
//! gains are not just our own reading of our own fixture.
//!
//! `std`-only, same two gates as `differential.rs`: `#![cfg(feature = "std")]`
//! plus `required-features = ["std"]` on the test target, so a plain
//! `cargo test` (what `cargo xtask verify` runs) never builds lofty.
#![cfg(feature = "std")]

use std::fs::File;
use std::path::{Path, PathBuf};

use embedded_io_adapters::std::FromStd;
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::id3::v2::{ChannelType, Frame, Id3v2Tag};
use lofty::mpeg::MpegFile;
use soul_audio_tags::Tags;

/// `crates/soul-audio-tags` -> workspace root.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/soul-audio-tags -> workspace root")
        .to_path_buf()
}

fn fixture() -> PathBuf {
    workspace_root().join("testdata").join("rva2-only.mp3")
}

/// Our own reading of the fixture, through the public `std` tag surface.
fn our_tags(path: &Path) -> Tags {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut src = FromStd::new(file);
    let (tags, _facts) = soul_audio_tags::read_owned(&mut src)
        .unwrap_or_else(|e| panic!("read_owned {}: {e:?}", path.display()));
    tags
}

/// lofty's ID3v2 tag for the fixture — the independent reference parse.
fn lofty_id3v2(path: &Path) -> Id3v2Tag {
    let mut file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mpeg = MpegFile::read_from(&mut file, ParseOptions::new())
        .unwrap_or_else(|e| panic!("lofty read {}: {e}", path.display()));
    mpeg.id3v2()
        .cloned()
        .expect("fixture carries an ID3v2 tag lofty can read")
}

/// The master-volume adjustment lofty reports for the RVA2 frame whose
/// identification is `ident`, in dB (the raw value is 1/512 dB).
fn lofty_rva2_master_db(tag: &Id3v2Tag, ident: &str) -> f32 {
    let frame = tag
        .into_iter()
        .filter_map(|f| match f {
            Frame::RelativeVolumeAdjustment(rva2) => Some(rva2),
            _ => None,
        })
        .find(|f| f.identification == ident)
        .unwrap_or_else(|| panic!("lofty found no RVA2 frame with identification {ident:?}"));
    let ch = frame
        .channels
        .get(&ChannelType::MasterVolume)
        .unwrap_or_else(|| panic!("RVA2 {ident:?} has no master-volume channel"));
    f32::from(ch.volume_adjustment) / 512.0
}

/// An RVA2-only MP3 yields track **and** album gain through `read_owned`,
/// matching lofty's independent parse of the same frames. The dB strings our
/// reader synthesizes are truncated to hundredths for display, so compare
/// within half a hundredth.
#[test]
fn rva2_only_mp3_yields_replaygain_through_read_owned() {
    let path = fixture();
    assert!(path.is_file(), "missing fixture: {}", path.display());

    let lofty_tag = lofty_id3v2(&path);
    // Guard the premise: this must be an RVA2-*only* file. If a TXXX
    // replaygain_* frame ever creeps into the fixture, the test would pass
    // through the pre-existing TXXX path and prove nothing about RVA2.
    assert!(
        !(&lofty_tag).into_iter().any(|f| match f {
            Frame::UserText(t) => t
                .description
                .to_ascii_lowercase()
                .starts_with("replaygain_"),
            _ => false,
        }),
        "fixture must carry no TXXX replaygain_* frames (it would bypass RVA2)"
    );

    let expected_track = lofty_rva2_master_db(&lofty_tag, "track");
    let expected_album = lofty_rva2_master_db(&lofty_tag, "album");

    let tags = our_tags(&path);
    let track = tags.track_gain_db.expect("RVA2 track gain");
    let album = tags.album_gain_db.expect("RVA2 album gain");

    assert!(
        (track - expected_track).abs() < 0.005,
        "track gain: ours {track} vs lofty {expected_track}"
    );
    assert!(
        (album - expected_album).abs() < 0.005,
        "album gain: ours {album} vs lofty {expected_album}"
    );

    // Peaks are deliberately not surfaced from RVA2 (the deck normalizes by
    // gain, not peak), even though the fixture's track frame carries one.
    assert_eq!(tags.track_peak, None, "RVA2 peak is not surfaced");
    assert_eq!(tags.album_peak, None, "RVA2 peak is not surfaced");
}

/// The same file decoded for playback still reports a sane stream — the tag
/// read is non-destructive to the MPEG stream behind it (the ID3v2 tag is
/// prepended, so a reader that mis-sizes it would corrupt the first frame).
#[test]
fn rva2_only_mp3_still_reads_stream_facts() {
    let path = fixture();
    let file = File::open(&path).expect("open fixture");
    let mut src = FromStd::new(file);
    let (_tags, facts) = soul_audio_tags::read_owned(&mut src).expect("read_owned");
    assert_eq!(facts.sample_rate, Some(44_100), "facts: {facts:?}");
    assert_eq!(facts.channels, Some(2), "facts: {facts:?}");
}
