//! Cross-format metadata e2e: the SAME ~1.5 s track, muxed by ffmpeg into
//! every container this crate reads, must yield consistent tags and a
//! plausible duration via `read_owned`.
//!
//! These `testdata/meta.*` fixtures encode the player's dogfooding bugs: WAV
//! showing "untitled" (RIFF `LIST`/`INFO` unread), AAC/ALAC-in-M4A with `moov`
//! after `mdat` and Ogg-FLAC failing, AIFF "untitled". This is the regression
//! gate that keeps every container reporting the same title/artist/album and a
//! ~1.5 s length.
//!
//! `std`-only (file I/O via `embedded-io-adapters`), same gating as
//! `differential` (see `Cargo.toml` + the `#![cfg(feature = "std")]` below).
#![cfg(feature = "std")]

use std::fs::File;
use std::path::{Path, PathBuf};

use embedded_io_adapters::std::FromStd;
use soul_audio_tags::read_owned;

const TITLE: &str = "Aria";
const ARTIST: &str = "Kimiko Ishizaka";
const ALBUM: &str = "Open Goldberg Variations";

/// Expected (title, artist, album); `None` where the container/encoder doesn't
/// carry that field — AIFF gets only a `NAME` (title) IFF chunk from ffmpeg,
/// and raw ADTS has no tag structure at all.
struct Case {
    file: &'static str,
    title: Option<&'static str>,
    artist: Option<&'static str>,
    album: Option<&'static str>,
}

fn full(file: &'static str) -> Case {
    Case {
        file,
        title: Some(TITLE),
        artist: Some(ARTIST),
        album: Some(ALBUM),
    }
}

fn cases() -> Vec<Case> {
    vec![
        full("meta.wav"),
        full("meta.flac"),
        full("meta.mp3"),
        full("meta.ogg"),
        full("meta.opus"),
        full("meta-aac.m4a"),  // moov after mdat (ffmpeg default muxing)
        full("meta-alac.m4a"), // moov after mdat
        full("meta-oggflac.oga"),
        full("meta.mka"),     // Matroska (EBML tags)
        full("meta.webm"),    // WebM (EBML tags)
        full("meta.caf"),     // CAF (`info` chunk)
        full("meta.wv"),      // WavPack (APEv2)
        full("meta-id3.aac"), // raw ADTS fronted by an ID3v2 tag (iTunes/FAAC)
        Case {
            file: "meta.aiff",
            title: Some(TITLE),
            artist: None,
            album: None,
        },
        Case {
            file: "meta.aac",
            title: None,
            artist: None,
            album: None,
        },
    ]
}

/// `crates/soul-audio-tags` -> workspace root -> `testdata`.
fn testdata_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/soul-audio-tags -> workspace root")
        .join("testdata")
}

#[test]
fn every_format_reports_consistent_tags_and_duration() {
    let dir = testdata_dir();
    let mut failures = Vec::new();

    for c in cases() {
        let path = dir.join(c.file);
        assert!(path.is_file(), "missing fixture: {}", path.display());
        let file = File::open(&path).expect("open fixture");
        let mut src = FromStd::new(file);
        let (tags, facts) = match read_owned(&mut src) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}: read_owned errored: {e:?}", c.file));
                continue;
            }
        };

        let want = (
            c.title.map(str::to_owned),
            c.artist.map(str::to_owned),
            c.album.map(str::to_owned),
        );
        let got = (tags.title.clone(), tags.artist.clone(), tags.album.clone());
        if got != want {
            failures.push(format!("{}: tags {got:?} != {want:?}", c.file));
        }

        // Duration = total_frames / sample_rate. MP4 reports movie-timescale
        // units and the rest PCM samples, but either ratio is seconds. Every
        // fixture is the same ~1.5 s clip, so a near-zero result (the Ogg-FLAC
        // `total_samples = 0` bug) or one far from 1.5 s (a wrong-length bug)
        // fails.
        match (facts.total_frames, facts.sample_rate) {
            (Some(tf), Some(sr)) if sr > 0 => {
                let secs = tf as f64 / f64::from(sr);
                // ~1.5 s; a little slack for ALAC/Opus frame-padding at the tail.
                if !(1.45..=1.65).contains(&secs) {
                    failures.push(format!("{}: duration {secs:.3}s not ~1.5s", c.file));
                }
            }
            other => failures.push(format!("{}: incomplete facts {other:?}", c.file)),
        }
    }

    assert!(
        failures.is_empty(),
        "cross-format metadata mismatches:\n{}",
        failures.join("\n")
    );
}
