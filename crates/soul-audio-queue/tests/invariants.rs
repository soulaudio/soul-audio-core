//! Randomized / model-based invariant tests (dependency-free — a tiny xorshift
//! PRNG, no proptest). These are the fuzz harness behind the hand-picked unit
//! cases: they drive long random op-sequences and assert structural invariants,
//! so an off-by-one in the in-place rotation (the trickiest new code) or a
//! wrong-row jump under shuffle is caught by construction, not by luck.

use soul_audio_engine::{Advance, Sequencer};
use soul_audio_queue::{Repeat, SliceContext, TwoLevelQueue};

/// xorshift64 — deterministic, seeded, good enough to shuffle test op streams.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// A value in `0..n` (n > 0).
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

const CAP: usize = 32;
type Up = heapless::Deque<u32, CAP>;
type Hist = heapless::Deque<(u32, usize), 64>;
type Q = TwoLevelQueue<SliceContext<u32, Vec<u32>>, Up, Hist>;

fn queue(src_len: u32) -> Q {
    let ids: Vec<u32> = (0..src_len).collect();
    TwoLevelQueue::new(
        SliceContext::new(ids),
        heapless::Deque::new(),
        heapless::Deque::new(),
    )
}

/// The manual Up-Next band must behave *exactly* like `Vec` push_back / insert(0)
/// / remove(i) / move / clear — the simplest possible reference model. Any
/// rotation bug in `remove_upnext` / `move_upnext` desyncs them immediately.
#[test]
fn manual_band_matches_a_vec_reference_model() {
    for seed in 1..=400u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut s = queue(8);
        let mut model: Vec<u32> = Vec::new();
        let mut next_h = 1000u32;

        for _ in 0..300 {
            match rng.below(6) {
                // enqueue (push_back) — mirror only if the bounded backing accepted it.
                0 => {
                    let h = next_h;
                    next_h += 1;
                    if s.enqueue(h).is_ok() {
                        model.push(h);
                    } else {
                        assert_eq!(model.len(), CAP, "enqueue only fails when full");
                    }
                }
                // play_next (push_front / insert(0)).
                1 => {
                    let h = next_h;
                    next_h += 1;
                    if s.play_next(h).is_ok() {
                        model.insert(0, h);
                    } else {
                        assert_eq!(model.len(), CAP, "play_next only fails when full");
                    }
                }
                // remove_upnext(i): sometimes in-range, sometimes not.
                2 => {
                    let i = rng.below(model.len() + 2);
                    let got = s.remove_upnext(i);
                    let want = (i < model.len()).then(|| model.remove(i));
                    assert_eq!(got, want, "remove_upnext({i}) [seed {seed}]");
                }
                // move_upnext(a, b).
                3 => {
                    let len = model.len();
                    let a = rng.below(len + 2);
                    let b = rng.below(len + 2);
                    let ok = s.move_upnext(a, b);
                    if a < len && b < len {
                        assert!(ok, "move in range");
                        let v = model.remove(a);
                        model.insert(b, v);
                    } else {
                        assert!(!ok, "move out of range is false");
                    }
                }
                // clear.
                4 => {
                    s.clear_upnext();
                    model.clear();
                }
                // a transport op that pops the queue front when it's non-empty,
                // kept in sync with the model (Up-Next wins on a normal advance).
                _ => {
                    let had = !model.is_empty();
                    s.advance(Advance::Natural);
                    if had {
                        model.remove(0);
                    }
                }
            }
            assert_eq!(
                s.upnext_iter().collect::<Vec<_>>(),
                model,
                "band desynced from the reference model [seed {seed}]"
            );
            assert_eq!(s.upnext_len(), model.len());
        }
    }
}

/// `play_upcoming(k)` must always play exactly the track the projection shows at
/// row `k` — across random queues, shuffle seeds, and `Repeat::Off`/`All`.
#[test]
fn play_upcoming_plays_the_projected_row_fuzz() {
    for seed in 1..=500u64 {
        let mut rng = Rng(seed ^ 0xA5A5_1234_DEAD_0001);
        let src_len = 4 + rng.below(20) as u32;
        let mut s = queue(src_len);

        // Random manual queue.
        for _ in 0..rng.below(6) {
            let _ = s.enqueue(5000 + rng.below(50) as u32);
        }
        // Random shuffle + repeat (exclude One: its projection is the frozen
        // current, a documented special case not addressed by play_upcoming).
        if rng.below(2) == 1 {
            s.set_shuffle(true, rng.next_u64());
        }
        s.set_repeat(if rng.below(2) == 1 {
            Repeat::All
        } else {
            Repeat::Off
        });
        // Advance a random amount so `pos`/history vary.
        for _ in 0..rng.below(src_len as usize) {
            if s.advance(Advance::Natural).is_none() {
                break;
            }
        }

        let k = 1 + rng.below(10);
        let want = s.upcoming_iter().nth(k - 1); // what the UI renders at row k
        let got = s.play_upcoming(k);
        assert_eq!(
            got, want,
            "play_upcoming({k}) != projected row [seed {seed}]"
        );
        if got.is_some() {
            assert_eq!(s.current(), got, "current follows the click");
        }
    }
}

/// `prev` must retrace the exact sequence of played tracks — even through manual
/// Up-Next interjections, shuffle, and a reshuffle mid-history.
#[test]
fn prev_retraces_playback_history_fuzz() {
    for seed in 1..=400u64 {
        let mut rng = Rng(seed.wrapping_mul(0x1000_0001) ^ 0xFEED);
        let mut s = queue(6 + rng.below(16) as u32);
        if rng.below(2) == 1 {
            s.set_shuffle(true, rng.next_u64());
        }
        if rng.below(2) == 1 {
            s.set_repeat(Repeat::All);
        }
        // Occasionally seed the manual queue so history mixes queue + source items.
        for _ in 0..rng.below(4) {
            let _ = s.enqueue(7000 + rng.below(30) as u32);
        }

        // Walk forward, recording every current we land on. Stop BEFORE the end
        // of a non-repeating queue: an advance that runs off the end still pushes
        // the last track to history (so prev returns to it) but leaves current at
        // None — a valid state, just not what this retrace invariant models.
        let mut visited = vec![s.current().expect("non-empty start")];
        for _ in 0..rng.below(10) {
            if s.upcoming(1).is_none() {
                break;
            }
            let trigger = if rng.below(2) == 1 {
                Advance::Natural
            } else {
                Advance::Skip
            };
            visited.push(s.advance(trigger).expect("upcoming(1) was Some"));
        }

        // Prev back to the start must replay the recorded currents in reverse.
        for expected in visited.iter().rev().skip(1) {
            assert_eq!(
                s.prev(),
                Some(*expected),
                "prev must retrace history [seed {seed}]"
            );
        }
    }
}
