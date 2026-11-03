//! End-to-end tests: drive the real Engine + LinearSequencer through real
//! decoders (WAV/FLAC/MP3 fixtures) and the real decode->resample->normalize
//! pipeline into a capturing sink. Proves the Phase-1 seam works end-to-end,
//! not just against mocks.

use std::sync::{Arc, Mutex};
use std::thread;

use soul_audio_core::{
    AnyDecoder, Deck, DeckState, Decoder, Progress, Quality, SampleFormat, Sink, SinkError,
    SliceSource, StreamInfo, open,
};
use soul_audio_engine::{Engine, LinearSequencer, RepeatMode};

const OUT_RATE: u32 = 48_000;
const OUT_CH: u16 = 2;
/// One Deck output block (`BLOCK = 256` frames in `deck.rs`) worth of
/// interleaved samples at `OUT_CH` channels — the unit used as an
/// approximate-equality tolerance where resampling/drain bookkeeping makes
/// exact equality fragile across a fresh reload (e.g. a `RepeatOne` replay).
const BLOCK_TOL: usize = 256 * OUT_CH as usize;
/// Preload depth used by every engine in this suite.
const PRELOAD: usize = 1;

type Dec = AnyDecoder<SliceSource<'static>>;

/// A std sink that captures every written frame. Fixed 48 kHz / 2ch / 24-bit,
/// no backpressure (renders as fast as the pump decodes).
struct CaptureSink {
    out: Arc<Mutex<Vec<i32>>>,
}
impl Sink for CaptureSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: OUT_RATE,
            channels: OUT_CH,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        self.out.lock().unwrap().extend_from_slice(frames);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

fn fixture_path(name: &str) -> String {
    format!("{}/../../testdata/{}", env!("CARGO_MANIFEST_DIR"), name)
}

/// Open a fixture into an owned `'static` decoder (leaks the bytes — test only).
fn open_fixture(name: &str) -> Dec {
    let bytes: &'static [u8] = Box::leak(
        std::fs::read(fixture_path(name))
            .expect("read fixture")
            .into_boxed_slice(),
    );
    open(SliceSource::new(bytes)).expect("open fixture")
}

/// Run `f` on a 16 MiB stack (`AnyDecoder` is large by value: the `Engine`
/// holds `PRELOAD+1` of them, which overflows a default test-thread stack).
fn on_big_stack<F: FnOnce() + Send + 'static>(f: F) {
    thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("test thread panicked");
}

type E = Engine<CaptureSink, Dec, LinearSequencer, PRELOAD>;

/// A fresh engine + the `Arc<Mutex<Vec<i32>>>` its sink captures into.
fn new_engine() -> (E, Arc<Mutex<Vec<i32>>>) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let sink = CaptureSink { out: out.clone() };
    let e = Engine::new(Deck::new(sink, Quality::Hq), LinearSequencer::new());
    (e, out)
}

/// Service every open the engine wants, opening the i-th fixture from `names`
/// (acts as the driver's off-thread loader).
fn service(e: &mut E, names: &[&str]) {
    while let Some(i) = e.poll_open() {
        let d = open_fixture(names[i]);
        e.provide(i, d).unwrap();
    }
}

/// Drive to `Ended`, servicing every open along the way. Returns the number of
/// `TrackChanged` (gapless swap) events observed. Bounded loop — panics if
/// playback never ends.
fn run_to_end(e: &mut E, names: &[&str]) -> usize {
    let mut swaps = 0;
    for _ in 0..1_000_000 {
        service(e, names);
        match e.pump().unwrap() {
            Progress::TrackChanged => swaps += 1,
            Progress::Ended => return swaps,
            _ => {}
        }
    }
    panic!("did not end");
}

/// Drive until `target` `TrackChanged` swaps have occurred, recording
/// `e.current()` after each. A `Progress::Ended` before reaching `target` is a
/// hard failure (used to assert repeat modes never end).
fn run_swaps(e: &mut E, names: &[&str], target: usize) -> Vec<usize> {
    let mut cursors = Vec::new();
    for _ in 0..1_000_000 {
        service(e, names);
        match e.pump().unwrap() {
            Progress::TrackChanged => {
                cursors.push(e.current().unwrap());
                if cursors.len() == target {
                    return cursors;
                }
            }
            Progress::Ended => panic!("unexpected Ended while collecting swaps"),
            _ => {}
        }
    }
    panic!("did not reach {target} swaps");
}

/// Render `name` solo (a fresh 1-track engine, run to end) and return the
/// captured samples. Used to compute fixture-derived lengths rather than
/// hard-coding them.
fn render_solo(name: &str) -> Vec<i32> {
    let (mut e, out) = new_engine();
    e.sequencer_mut().set_queue(1, 0);
    e.rearm();
    let names = [name];
    let swaps = run_to_end(&mut e, &names);
    assert_eq!(swaps, 0, "a solo track must not swap");
    out.lock().unwrap().clone()
}

/// A slow loader that only opens when the deck is `Stopped`/`Ended` (never
/// preloads), forcing an underrun at every boundary. Mirrors
/// `drive_with_underruns` from `engine.rs`'s unit tests but opens real
/// fixtures. Returns the sequence of `current()` handles actually loaded.
fn drive_with_underruns(e: &mut E, names: &[&str], max: usize) -> Vec<usize> {
    let mut loaded = Vec::new();
    for _ in 0..1_000_000 {
        if matches!(e.state(), DeckState::Stopped | DeckState::Ended) {
            if let Some(i) = e.poll_open() {
                let d = open_fixture(names[i]);
                e.provide(i, d).unwrap();
                loaded.push(e.current().unwrap());
                if loaded.len() > max {
                    return loaded;
                }
            }
        }
        if e.pump().unwrap() == Progress::Ended {
            return loaded;
        }
    }
    panic!("did not terminate");
}

#[test]
fn multi_track_gapless_is_exact_concatenation() {
    on_big_stack(|| {
        let names = ["tone.wav", "tone.flac", "tone.mp3"];
        let sum: usize = names.iter().map(|n| render_solo(n).len()).sum();

        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 0);
        e.rearm();
        let swaps = run_to_end(&mut e, &names);

        assert_eq!(swaps, 2, "3-track queue swaps exactly twice");
        let captured = out.lock().unwrap();
        assert_eq!(
            captured.len(),
            sum,
            "gapless queue length must be the EXACT concatenation of the solo lengths"
        );
        assert!(
            captured.iter().any(|&s| s != 0),
            "output must be non-silent"
        );
    });
}

#[test]
fn plays_in_order_cursor_progresses() {
    on_big_stack(|| {
        let names = ["tone.wav", "tone.flac", "tone.mp3"];
        let (mut e, _out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 0);
        e.rearm();
        assert_eq!(e.current(), Some(0));

        let cursors = run_swaps(&mut e, &names, 2);
        assert_eq!(cursors, [1, 2], "cursor visits 0 -> 1 -> 2");
        assert_eq!(e.current(), Some(2));
    });
}

#[test]
fn repeat_all_cycles_forever() {
    on_big_stack(|| {
        let names = ["tone.wav", "tone.flac"];
        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 0);
        e.sequencer_mut().set_mode(RepeatMode::RepeatAll);
        e.rearm();

        // run_swaps panics on an unexpected Progress::Ended, so reaching 5
        // swaps here already proves no Ended occurred along the way.
        let cursors = run_swaps(&mut e, &names, 5);
        assert_eq!(cursors, [1, 0, 1, 0, 1]);
        assert!(out.lock().unwrap().iter().any(|&s| s != 0));
    });
}

#[test]
fn repeat_one_loops_current() {
    on_big_stack(|| {
        let names = ["tone.wav", "tone.flac", "tone.mp3"];
        let solo_len = render_solo("tone.flac").len(); // index 1

        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 1);
        e.sequencer_mut().set_mode(RepeatMode::RepeatOne);
        e.rearm();

        let cursors = run_swaps(&mut e, &names, 3);
        assert_eq!(cursors, [1, 1, 1], "only track 1 ever plays");

        // A TrackChanged swap fires exactly when a track FULLY finishes and
        // its (already-preloaded) successor is promoted, so N swaps means N
        // completed full plays captured (the just-promoted (N+1)-th instance
        // has produced no frames yet at the moment the Nth swap is observed).
        let captured_len = out.lock().unwrap().len();
        let expect = 3 * solo_len; // 3 completed plays by the time swap #3 fires
        assert!(
            captured_len.abs_diff(expect) <= BLOCK_TOL,
            "captured {captured_len} vs ~{expect} (3x solo length), tol {BLOCK_TOL}"
        );
    });
}

#[test]
fn next_skips_to_expected_tail() {
    on_big_stack(|| {
        let names = ["tone.wav", "tone.flac", "tone.mp3"];
        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 0);
        e.rearm();

        service(&mut e, &names);
        for _ in 0..3 {
            e.pump().unwrap();
        }
        e.next().unwrap();
        assert_eq!(e.current(), Some(1));

        let swaps = run_to_end(&mut e, &names);
        assert_eq!(swaps, 1, "exactly one further swap, 1 -> 2");
        assert!(out.lock().unwrap().iter().any(|&s| s != 0));
    });
}

#[test]
fn prev_goes_back() {
    on_big_stack(|| {
        let names = ["tone.wav", "tone.flac", "tone.mp3"];
        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 2);
        e.rearm();

        service(&mut e, &names);
        e.pump().unwrap();
        e.prev().unwrap();
        assert_eq!(e.current(), Some(1));

        let before = out.lock().unwrap().len();
        service(&mut e, &names);
        for _ in 0..5 {
            e.pump().unwrap();
        }
        let after = out.lock().unwrap().len();
        assert!(after > before, "playback continues after prev()");
    });
}

#[test]
fn goto_jumps() {
    on_big_stack(|| {
        let base = ["tone.wav", "tone.flac", "tone.mp3"];
        let names: Vec<&str> = (0..5).map(|i| base[i % 3]).collect();
        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 0);
        e.rearm();

        service(&mut e, &names);
        e.pump().unwrap();
        e.goto(3).unwrap();
        assert_eq!(e.current(), Some(3));

        let before = out.lock().unwrap().len();
        service(&mut e, &names);
        for _ in 0..5 {
            e.pump().unwrap();
        }
        assert!(
            out.lock().unwrap().len() > before,
            "produces output after goto"
        );
    });
}

#[test]
fn empty_queue_is_inert() {
    on_big_stack(|| {
        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(0, 0);
        e.rearm();

        assert_eq!(e.current(), None);
        assert_eq!(e.poll_open(), None);
        assert_eq!(e.pump().unwrap(), Progress::Idle);
        assert!(!e.is_active());
        assert!(out.lock().unwrap().is_empty());
    });
}

/// Decode `name` with the raw [`Decoder`] seam directly — no `Deck`/`Engine`
/// involved — and return the total per-channel input frame count plus its
/// `StreamInfo`. Used as an independent ground truth for
/// `single_track_plays_once_and_ends`: unlike `render_solo` (which runs
/// through the very `Deck`/`Engine` pipeline under test), this cannot be
/// fooled by a systemic single-track completion bug in that pipeline.
fn raw_decode_frame_count(name: &str) -> (u64, StreamInfo) {
    let mut dec = open_fixture(name);
    let info = dec.stream_info();
    let channels = info.channels as usize;
    let mut buf = [0i32; 4096];
    let mut in_frames: u64 = 0;
    loop {
        let n = dec.decode_into(&mut buf).expect("raw decode");
        if n == 0 {
            break;
        }
        assert_eq!(n % channels, 0, "decode_into must return whole frames");
        in_frames += (n / channels) as u64;
    }
    (in_frames, info)
}

#[test]
fn single_track_plays_once_and_ends() {
    on_big_stack(|| {
        // Ground truth from OUTSIDE the Deck/Engine: raw-decode `tone.wav`
        // (WAV = no codec delay, so the frame count is exact) directly through
        // the `Decoder` trait. A systemic single-track completion bug (the
        // Deck/Engine replaying the track once before `Ended`, or truncating
        // it) would reproduce identically across two `render_solo` calls, so
        // comparing solo renders to each other (the old check) cannot catch
        // it — this anchor can, because it never goes through Deck/Engine.
        let (in_frames, info) = raw_decode_frame_count("tone.wav");

        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(1, 0);
        e.rearm();
        let names = ["tone.wav"];
        let swaps = run_to_end(&mut e, &names);
        assert_eq!(swaps, 0, "a solo track must not swap");

        let captured = out.lock().unwrap();
        assert!(!captured.is_empty());
        assert!(
            captured.iter().any(|&s| s != 0),
            "output must be non-silent"
        );

        let out_frames = captured.len() / OUT_CH as usize;
        let expected_out = in_frames * OUT_RATE as u64 / info.sample_rate as u64;
        let diff = (out_frames as i64 - expected_out as i64).unsigned_abs();
        eprintln!(
            "single_track_plays_once_and_ends: out_frames={out_frames} expected_out={expected_out} diff={diff}"
        );
        assert!(
            diff <= 1024,
            "single track rendered {out_frames} frames vs raw-decode-derived {expected_out} (±1024) — a truncated or replayed single track fails this"
        );
    });
}

#[test]
fn out_of_range_goto_is_noop() {
    on_big_stack(|| {
        let names = ["tone.wav", "tone.flac", "tone.mp3"];
        let (mut e, _out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 0);
        e.rearm();

        service(&mut e, &names);
        e.goto(9).unwrap();
        assert_eq!(
            e.current(),
            Some(0),
            "out-of-range goto leaves the cursor unchanged"
        );
        assert!(e.is_active());
    });
}

#[test]
fn underrun_still_reaches_every_track() {
    on_big_stack(|| {
        let names = ["tone.wav", "tone.flac", "tone.mp3"];
        let (mut e, out) = new_engine();
        e.sequencer_mut().set_queue(names.len(), 0);
        e.rearm();

        let loaded = drive_with_underruns(&mut e, &names, 100);
        assert_eq!(
            loaded,
            [0, 1, 2],
            "every track reached in order despite underruns"
        );
        assert!(!e.is_active());
        assert!(out.lock().unwrap().iter().any(|&s| s != 0));
    });
}
