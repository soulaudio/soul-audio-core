//! Cue-sheet playback, end to end: a **real** single-file album (the committed
//! `tone.flac`) plus a **real** cue sheet, played through the real `open()` →
//! `SubRange` → `Deck` path as separate tracks.
//!
//! The two claims that matter for a single-file rip, and they are the same
//! claim twice:
//!
//! 1. **Sample-accurate** — each cue track emits exactly the file's frames
//!    between its index points, no more and no less.
//! 2. **Gapless** — playing the tracks back to back through one deck
//!    reproduces the file's own continuous decode, sample for sample. A gap or
//!    an overlap at a boundary (the usual symptom of index-point arithmetic
//!    done in seconds rather than CD frames, or of a decoder that cannot stop
//!    mid-stream) shows up as a length mismatch or a shifted comparison.
//!
//! The cue sheet is written here rather than committed as a fixture: the
//! timestamps have to be derived from the fixture's real length for the test
//! to assert tiling, and a hand-written `.cue` with stale offsets would be a
//! worse fixture than the arithmetic that produces it.

use soul_audio_core::{
    AnyDecoder, Deck, Decoder, Progress, Quality, SampleFormat, SeekableDecoder, Sink, SinkError,
    SliceSource, StreamInfo, SubRange, decoder_scratch_len, open,
};
use soul_audio_playlist::cue::{self, CueTime};

const FIXTURE: &[u8] = include_bytes!("../../../testdata/tone.flac");

type Dec = AnyDecoder<SliceSource<'static>, Vec<i32>>;

fn open_fixture() -> Dec {
    open(SliceSource::new(FIXTURE), vec![0i32; decoder_scratch_len()]).expect("open tone.flac")
}

/// Decode a whole decoder to exhaustion.
fn drain(d: &mut impl Decoder) -> Vec<i32> {
    let mut out = Vec::new();
    let mut buf = vec![0i32; 4096];
    loop {
        let n = d.decode_into(&mut buf).expect("decode");
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// A sink that captures every frame, at the fixture's own rate and channel
/// count so the deck neither resamples nor remaps. What it receives is then the
/// decoded samples on the deck's canonical 24-bit scale (see [`canonical`]) —
/// an exact power-of-two shift, which is what lets the comparisons be exact.
struct CaptureSink {
    rate: u32,
    channels: u16,
    written: Vec<i32>,
}

impl Sink for CaptureSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        self.written.extend_from_slice(frames);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// A cue sheet splitting one file into three tracks at `MM:SS:FF` index
/// points — written the way a ripper does, CRLF and all.
fn cue_text(splits: [(u64, u64, u64); 2]) -> String {
    let [(m1, s1, f1), (m2, s2, f2)] = splits;
    format!(
        "REM GENRE Test\r\n\
         PERFORMER \"Soul Audio\"\r\n\
         TITLE \"Tone Album\"\r\n\
         FILE \"tone.flac\" WAVE\r\n\
         \x20 TRACK 01 AUDIO\r\n\
         \x20   TITLE \"First\"\r\n\
         \x20   INDEX 01 00:00:00\r\n\
         \x20 TRACK 02 AUDIO\r\n\
         \x20   TITLE \"Second\"\r\n\
         \x20   INDEX 01 {m1:02}:{s1:02}:{f1:02}\r\n\
         \x20 TRACK 03 AUDIO\r\n\
         \x20   TITLE \"Third\"\r\n\
         \x20   INDEX 01 {m2:02}:{s2:02}:{f2:02}\r\n"
    )
}

/// The fixture's decoded samples and its stream facts, straight from the
/// decoder — the ground truth every assertion below is against.
fn whole_file() -> (Vec<i32>, u32, u16, u64, u8) {
    let mut d = open_fixture();
    let si = d.stream_info();
    let total = d.total_frames().expect("tone.flac declares its length");
    let bits = match si.format {
        SampleFormat::Int { bits } | SampleFormat::Float { bits } => bits,
        _ => unreachable!("the fixture is integer PCM"),
    };
    (drain(&mut d), si.sample_rate, si.channels, total, bits)
}

/// Decoded samples as the **deck** emits them: it normalizes every source to a
/// canonical signed-24-bit scale (`2^(24 − bits)`), so a 16-bit fixture's
/// samples arrive at the sink shifted left by 8. Exact — a power of two — so
/// comparing deck output against a decoder's samples stays bit-exact.
fn canonical(samples: &[i32], bits: u8) -> Vec<i32> {
    let shift = 24u32.saturating_sub(u32::from(bits));
    samples.iter().map(|&s| s << shift).collect()
}

#[test]
fn cue_tracks_are_sample_accurate_slices_of_the_file() {
    let (whole, rate, channels, total, _bits) = whole_file();
    assert_eq!(rate, 44_100, "the fixture is CD rate");
    assert!(CueTime::divides_evenly(rate), "index points are exact here");

    // Split at 0.4 s (30 CD frames) and 1.2 s (90 CD frames) — both inside the
    // 2-second fixture, and neither on a decoder block boundary.
    let sheet = cue::parse(&cue_text([(0, 0, 30), (0, 1, 15)])).expect("parse cue");
    let file = &sheet.files[0];
    assert_eq!(file.name, "tone.flac");
    assert_eq!(sheet.audio_track_count(), 3);

    let ranges = file.ranges(rate, Some(total));
    assert_eq!(ranges.len(), 3);
    // The ranges tile the file: this is the cue arithmetic, before any audio.
    assert_eq!(
        ranges.iter().map(|r| r.frames.unwrap()).sum::<u64>(),
        total,
        "the ranges must account for every frame of the file"
    );

    let ch = usize::from(channels);
    let mut offset = 0usize;
    for (n, range) in ranges.iter().enumerate() {
        let mut track = SubRange::new(open_fixture(), range.start_frame, range.frames)
            .expect("sub-range decoder");
        // The track reports its own length, not the album file's.
        assert_eq!(track.total_frames(), range.frames, "track {n} length");
        let got = drain(&mut track);
        let want_len = range.frames.unwrap() as usize * ch;
        assert_eq!(got.len(), want_len, "track {n} emitted the wrong length");
        // And exactly the file's samples for that range — a one-frame error in
        // the index arithmetic would shift this comparison.
        assert_eq!(
            got,
            whole[offset..offset + want_len],
            "track {n} is not the file's frames {}..{}",
            range.start_frame,
            range.start_frame + range.frames.unwrap()
        );
        offset += want_len;
    }
    assert_eq!(offset, whole.len(), "the tracks must cover the whole file");
}

#[test]
fn playing_the_cue_tracks_gaplessly_reproduces_the_file() {
    let (whole, rate, channels, total, bits) = whole_file();
    let sheet = cue::parse(&cue_text([(0, 0, 30), (0, 1, 15)])).expect("parse cue");
    let ranges = sheet.files[0].ranges(rate, Some(total));

    // One deck, at the file's own rate and channel count: no resampling, no
    // channel mapping, 24-bit sink — so what the sink receives is what the
    // decoder produced, and the comparison can be exact.
    let mut deck: Deck<CaptureSink, SubRange<Dec>, 1> = Deck::new(
        CaptureSink {
            rate,
            channels,
            written: Vec::new(),
        },
        Quality::Hq,
    );

    let track = |i: usize| {
        SubRange::new(open_fixture(), ranges[i].start_frame, ranges[i].frames).expect("sub-range")
    };

    deck.load(track(0)).expect("load first cue track");
    deck.preload(track(1)).expect("preload second");
    let mut next = 2usize;
    let mut swaps = 0usize;
    for _ in 0..100_000 {
        match deck.pump().expect("pump") {
            Progress::TrackChanged => {
                swaps += 1;
                // Keep the preload slot full, exactly as the engine's driver
                // does — this is the gapless path, not a reload.
                if next < ranges.len() {
                    deck.preload(track(next)).expect("preload");
                    next += 1;
                }
            }
            Progress::Ended | Progress::Idle => break,
            Progress::Played => {}
        }
    }
    assert_eq!(swaps, 2, "three cue tracks swap twice");

    let played = deck.sink().written.clone();
    assert_eq!(
        played.len(),
        whole.len(),
        "gapless cue playback must be exactly as long as the file"
    );
    assert_eq!(
        played,
        canonical(&whole, bits),
        "gapless cue playback must reproduce the file sample for sample — a gap \
         or overlap at an index point shows up here"
    );
}

#[test]
fn seeking_inside_a_cue_track_stays_inside_it() {
    let (whole, rate, channels, total, bits) = whole_file();
    let sheet = cue::parse(&cue_text([(0, 0, 30), (0, 1, 15)])).expect("parse cue");
    let ranges = sheet.files[0].ranges(rate, Some(total));
    let ch = usize::from(channels);

    // The middle track, so there is audio on both sides of it to spill into.
    let range = ranges[1];
    let mut deck: Deck<CaptureSink, SubRange<Dec>, 0> = Deck::new(
        CaptureSink {
            rate,
            channels,
            written: Vec::new(),
        },
        Quality::Hq,
    );
    deck.load(SubRange::new(open_fixture(), range.start_frame, range.frames).expect("sub-range"))
        .expect("load");
    // Positions and duration are the track's, not the album's.
    assert_eq!(deck.duration_frames(), range.frames);

    // Seek a third of the way in: the deck's position is relative to the
    // track, and the audio that follows is the file's audio from there.
    let third = range.frames.unwrap() / 3;
    assert_eq!(deck.seek(third).expect("seek"), third);
    assert_eq!(deck.position_frames(), third);

    for _ in 0..100_000 {
        match deck.pump().expect("pump") {
            Progress::Ended | Progress::Idle => break,
            _ => {}
        }
    }
    let played = deck.sink().written.clone();
    assert_eq!(
        played.len(),
        (range.frames.unwrap() - third) as usize * ch,
        "a seek must play out the rest of the TRACK, not the rest of the file"
    );

    // The tail is the file's own samples from the seek point — except for the
    // deck's 8 ms de-click fade-in, so compare past it.
    let from = (range.start_frame + third) as usize * ch;
    let fade = (rate as usize / 100) * ch; // 10 ms of fade headroom
    assert_eq!(
        played[fade..],
        canonical(&whole[from + fade..from + played.len()], bits),
        "the post-seek audio must be the file's audio from that point"
    );
}
