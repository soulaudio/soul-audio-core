//! Integration: the Phase-1 reconcile `Engine` driven by a `TwoLevelQueue`.
//! Uses mock sink/decoders (behavior, not audio) to prove the queue policy and
//! the engine reconcile compose — especially that "Play Next" re-primes gaplessly.

use soul_audio_core::{
    Deck, DecodeError, Decoder, Progress, Quality, SampleFormat, SeekableDecoder, Sink, SinkError,
    StreamInfo,
};
use soul_audio_engine::Engine;
use soul_audio_queue::{Context, HeaplessQueue};

const RATE: u32 = 48_000;
const FRAMES: u64 = 400;

struct ConstDecoder {
    value: i32,
    frames: u64,
    pos: u64,
}
impl Decoder for ConstDecoder {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: RATE,
            channels: 1,
            format: SampleFormat::Int { bits: 16 },
        }
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let want = (out.len() as u64).min(self.frames - self.pos) as usize;
        out[..want].fill(self.value);
        self.pos += want as u64;
        Ok(want)
    }
}
impl SeekableDecoder for ConstDecoder {
    fn seek_to(&mut self, s: u64) -> Result<u64, DecodeError> {
        self.pos = s.min(self.frames);
        Ok(self.pos)
    }
    fn total_frames(&self) -> Option<u64> {
        Some(self.frames)
    }
}

struct MockSink {
    written: Vec<i32>,
}
impl Sink for MockSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: RATE,
            channels: 1,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, f: &[i32]) -> Result<(), SinkError> {
        self.written.extend_from_slice(f);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// The library: handle == track id; the decoder plays a constant = (id+1)*100.
struct Library {
    ids: Vec<u32>,
}
impl Context for Library {
    type Handle = u32;
    fn len(&self) -> usize {
        self.ids.len()
    }
    fn handle_at(&self, i: usize) -> Option<u32> {
        self.ids.get(i).copied()
    }
}
fn decoder_for(id: u32) -> ConstDecoder {
    ConstDecoder {
        value: (id as i32 + 1) * 100,
        frames: FRAMES,
        pos: 0,
    }
}
fn sample_of(id: u32) -> i32 {
    ((id as i32 + 1) * 100) << 8
}

type Q = HeaplessQueue<Library, 4, 8>;
type E = Engine<MockSink, ConstDecoder, Q, 1>;

fn engine(ids: &[u32]) -> E {
    let ctx = Library { ids: ids.to_vec() };
    let queue: Q = HeaplessQueue::new(ctx, heapless::Deque::new(), heapless::Deque::new());
    let mut e = Engine::new(
        Deck::new(
            MockSink {
                written: Vec::new(),
            },
            Quality::Fast,
        ),
        queue,
    );
    e.rearm();
    e
}
fn service(e: &mut E) {
    while let Some(id) = e.poll_open() {
        e.provide(id, decoder_for(id)).unwrap();
    }
}

#[test]
fn plays_library_in_order() {
    let mut e = engine(&[0, 1, 2]);
    let mut swaps = 0;
    for _ in 0..100_000 {
        service(&mut e);
        match e.pump().unwrap() {
            Progress::TrackChanged => swaps += 1,
            Progress::Ended => break,
            _ => {}
        }
    }
    assert_eq!(swaps, 2);
    let w = &e.deck().sink().written;
    assert_eq!(w[10], sample_of(0));
    assert_eq!(w[FRAMES as usize + 10], sample_of(1));
    assert_eq!(w[FRAMES as usize * 2 + 10], sample_of(2));
}

#[test]
fn play_next_reprimes_gaplessly_through_the_engine() {
    let mut e = engine(&[0, 1, 2]);
    service(&mut e); // current 0, preload 1
    e.pump().unwrap();
    // Play Next track 2: upcoming(1) changes 1 -> 2; the engine must discard the
    // stale preload and re-prime 2, then swap to 2 (not 1) at the boundary.
    e.sequencer_mut().play_next(2).unwrap();
    let mut swapped_to = None;
    for _ in 0..100_000 {
        service(&mut e);
        if let Progress::TrackChanged = e.pump().unwrap() {
            swapped_to = e.current();
            break;
        }
    }
    assert_eq!(
        swapped_to,
        Some(2),
        "Play Next winner played next, gaplessly"
    );
}

#[test]
fn repeat_all_cycles_through_the_engine() {
    let mut e = engine(&[0, 1]);
    e.sequencer_mut().set_repeat(soul_audio_queue::Repeat::All);
    let mut cursors = Vec::new();
    for _ in 0..100_000 {
        service(&mut e);
        if let Progress::TrackChanged = e.pump().unwrap() {
            cursors.push(e.current().unwrap());
            if cursors.len() == 4 {
                break;
            }
        }
    }
    assert_eq!(cursors, [1, 0, 1, 0]);
}

/// Drive the engine until the next track swap, returning the handle swapped to.
fn run_to_next_swap(e: &mut E) -> Option<u32> {
    for _ in 0..100_000 {
        service(e);
        if let Progress::TrackChanged = e.pump().unwrap() {
            return e.current();
        }
    }
    None
}

#[test]
fn reorder_reprimes_the_new_next_through_the_engine() {
    let mut e = engine(&[0, 1]);
    e.sequencer_mut().enqueue(90).unwrap();
    e.sequencer_mut().enqueue(91).unwrap(); // Up-Next [90, 91]; upcoming(1) = 90
    service(&mut e);
    e.pump().unwrap(); // current 0, preload 90
    // Drag 91 to the front of the queue: upcoming(1) becomes 91, so the engine
    // must discard the stale 90 preload and re-prime 91 — gaplessly, same path as
    // Play Next.
    assert!(e.sequencer_mut().move_upnext(0, 1)); // Up-Next [91, 90]
    assert_eq!(
        run_to_next_swap(&mut e),
        Some(91),
        "reorder re-primed the new front of the queue"
    );
}

#[test]
fn remove_from_queue_reprimes_through_the_engine() {
    let mut e = engine(&[0, 1]);
    e.sequencer_mut().enqueue(90).unwrap();
    e.sequencer_mut().enqueue(91).unwrap(); // upcoming(1) = 90
    service(&mut e);
    e.pump().unwrap();
    // Remove the front queued row (90): 91 is promoted; the engine re-primes it.
    assert_eq!(e.sequencer_mut().remove_upnext(0), Some(90));
    assert_eq!(
        run_to_next_swap(&mut e),
        Some(91),
        "removing the front queued row promoted the next, gaplessly"
    );
}

#[test]
fn click_a_queued_row_jumps_to_it_through_the_engine() {
    let mut e = engine(&[0, 1]);
    for h in [90, 91, 92] {
        e.sequencer_mut().enqueue(h).unwrap(); // Up-Next [90, 91, 92]
    }
    service(&mut e);
    e.pump().unwrap(); // current 0
    let mark = e.deck().sink().written.len();
    // Click the 2nd queued row (91): jump straight to it, dropping 90. This
    // changes `current` out of band; the engine's reconcile reloads the deck.
    assert_eq!(e.sequencer_mut().play_upnext(1), Some(91));
    assert_eq!(e.current(), Some(91), "sequencer jumped to the clicked row");
    let mut played_jump_target = false;
    for _ in 0..100_000 {
        service(&mut e);
        e.pump().unwrap();
        if e.deck().sink().written[mark..].contains(&sample_of(91)) {
            played_jump_target = true;
            break;
        }
    }
    assert!(
        played_jump_target,
        "the engine reloaded the deck and actually played the jumped-to track"
    );
}
