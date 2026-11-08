//! The playback capstone: prove a *located* track is the *right* file **and**
//! that it decodes to audio — the last link between the data layer
//! (scan → bake → read → locate) and the playback engine
//! (`soul_audio_core::open` → PCM).
//!
//! Host-side, no board. We bake a real "card" of tagged audio in nested folders,
//! read the index back over a real `std::fs::File` `SeekSource` (the on-disk path
//! the DAP's SD read mirrors), reconstruct each track's physical location from
//! the folder table + file name, then open that audio file through the **same**
//! decode entry point the CLI and firmware use (`open` over a `SliceSource`) and
//! assert:
//!
//! - decoding yields **non-empty PCM** and terminates cleanly at EOF, and
//! - the decoder's own `sample_rate` / `channels` **equal the catalog record's**
//!   — so the file we located is provably the file the catalog described.
//!
//! Two formats (FLAC + WAV) run through the real dispatch, so this proves the
//! locate→decode seam, not a single codec.
//!
//! `soul-audio-core` is already a normal dependency of `soul-audio-import`, so
//! this test adds no dependency and cannot shift any crate's tier.

use std::path::{Path, PathBuf};

use soul_audio_catalog::{Catalog, StrRef};
use soul_audio_core::{Decoder, SliceSource, open};
use soul_audio_index::IndexReader;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// A unique temp "card": a tagged FLAC and a WAV in **nested** album folders.
/// Sorted walk ("Classical" < "Signals") → FLAC = track 0, WAV = track 1.
fn card_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("soul_playback_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bach = root.join("Classical").join("Bach");
    let signals = root.join("Signals");
    std::fs::create_dir_all(&bach).unwrap();
    std::fs::create_dir_all(&signals).unwrap();
    std::fs::copy(testdata("tagged.flac"), bach.join("01 - Aria.flac")).unwrap();
    std::fs::copy(testdata("chirp.wav"), signals.join("chirp.wav")).unwrap();
    root
}

/// Open the exported index over a real on-disk `File` as a `SeekSource` — the
/// transport the DAP uses (SD), proven here on a host file.
fn open_on_disk(card: &Path) -> IndexReader<embedded_io_adapters::std::FromStd<std::fs::File>> {
    let path = card.join(".soulidx").join("index.sidx");
    let file = std::fs::File::open(&path).expect("open exported index.sidx");
    IndexReader::new(embedded_io_adapters::std::FromStd::new(file))
}

fn owned(rdr: &impl Catalog, s: StrRef) -> String {
    let mut buf = [0u8; 256];
    let n = rdr.resolve_str(s, &mut buf).expect("resolve_str");
    core::str::from_utf8(&buf[..n]).expect("utf8").to_owned()
}

/// Reconstruct a track's physical path the way the DAP does:
/// `card_root / folder(track.folder).path / track.file_name`.
fn physical_path(rdr: &impl Catalog, card: &Path, track_id: u32) -> PathBuf {
    let t = rdr.track(track_id).expect("track");
    let folder = rdr.folder(t.folder).expect("folder");
    let rel = owned(rdr, folder.path);
    let name = owned(rdr, t.file_name);
    let mut p = card.to_path_buf();
    if !rel.is_empty() {
        p.push(&rel);
    }
    p.push(&name);
    p
}

#[test]
fn located_tracks_decode_to_pcm_matching_the_catalog() {
    let card = card_root();

    // Desktop bakes the index onto the card (v1 model = index-the-card).
    soul_audio_import::export(&card, &card).expect("export");
    let rdr = open_on_disk(&card);

    let n = rdr.track_count();
    assert_eq!(n, 2, "the FLAC and the WAV were both scanned");

    let mut decoded_flac = false;
    let mut decoded_wav = false;

    for id in 0..n {
        let rec = rdr.track(id).expect("track record");
        let path = physical_path(&rdr, &card, id);
        assert!(path.is_file(), "located path must exist: {path:?}");

        // Open the located file through the SAME entry point the CLI/firmware use
        // (`open` over an in-memory `SliceSource`, the DAP's no-alloc source).
        let bytes = std::fs::read(&path).expect("read located audio file");
        let mut decoder =
            open(SliceSource::new(&bytes)).unwrap_or_else(|e| panic!("open {path:?}: {e:?}"));

        // The located file *is* the file the catalog described: the decoder's own
        // stream facts must match the baked record (not merely "some audio").
        let info = decoder.stream_info();
        assert_ne!(
            rec.sample_rate, 0,
            "scan captured stream facts for {path:?}"
        );
        assert_eq!(
            info.sample_rate, rec.sample_rate,
            "sample rate: decoder vs catalog for {path:?}"
        );
        assert_eq!(
            info.channels,
            u16::from(rec.channels),
            "channels: decoder vs catalog for {path:?}"
        );

        // It actually plays: drain to EOF, proving non-empty PCM that terminates.
        let mut buf = [0i32; 1024];
        let mut total: u64 = 0;
        let mut reached_eof = false;
        for _ in 0..5_000_000u32 {
            let got = decoder.decode_into(&mut buf).expect("decode_into");
            if got == 0 {
                reached_eof = true;
                break;
            }
            total += got as u64;
        }
        assert!(reached_eof, "decode terminated cleanly at EOF for {path:?}");
        assert!(total > 0, "decoded non-empty PCM for {path:?}");
        let frames = total / u64::from(info.channels.max(1));
        assert!(frames > 0, "at least one PCM frame for {path:?}");

        match path.extension().and_then(|e| e.to_str()) {
            Some("flac") => {
                decoded_flac = true;
                // Sanity: the tagged FLAC we laid down is the Aria fixture.
                assert_eq!(owned(&rdr, rec.title), "Soul Test Track");
            }
            Some("wav") => decoded_wav = true,
            other => panic!("unexpected fixture extension: {other:?}"),
        }
    }

    assert!(decoded_flac, "the FLAC track was located and decoded");
    assert!(decoded_wav, "the WAV track was located and decoded");

    let _ = std::fs::remove_dir_all(&card);
}
