//! Scale guards: the model's behaviour must be **correct at a million tracks**,
//! and no operation may secretly scan the library. The assertions that matter are
//! the deterministic correctness ones (they pin the virtual-context property); the
//! wall-clock check is a single, deliberately huge ceiling that only trips on a
//! catastrophic O(library) regression (which would otherwise hang the suite), not
//! a tight budget that could flake. Real throughput numbers live in
//! `benches/queue_ops.rs`.
//!
//! The headline property: the source context is *virtual* (position → handle,
//! O(1) memory and time at any library size), so transport and the Up-Next
//! projection cost the same over a million tracks as over ten.

use std::time::{Duration, Instant};

use soul_audio_engine::{Advance, Sequencer};
use soul_audio_queue::{SliceContext, TwoLevelQueue};

const LIBRARY: usize = 1_000_000;
/// Catastrophic-regression ceiling: anything near this means an op went
/// O(library). Real work here is sub-millisecond; the headroom is intentional so
/// a loaded/emulated CI box never flakes.
const CEILING: Duration = Duration::from_secs(30);

type Up = heapless::Deque<u32, 64>;
type Hist = heapless::Deque<(u32, usize), 64>;
type Q = TwoLevelQueue<SliceContext<u32, Vec<u32>>, Up, Hist>;

fn million_track_queue() -> Q {
    #[allow(clippy::cast_possible_truncation)]
    let ids: Vec<u32> = (0..LIBRARY as u32).collect();
    TwoLevelQueue::new(
        SliceContext::new(ids),
        heapless::Deque::new(),
        heapless::Deque::new(),
    )
}

#[test]
fn upcoming_projection_is_independent_of_library_size() {
    let s = million_track_queue();
    let t = Instant::now();
    // A bounded projection over a million-track context must be instant and
    // correct — it resolves 50 positions, never scans the library.
    let up: Vec<u32> = s.upcoming_iter().take(50).collect();
    let elapsed = t.elapsed();

    assert_eq!(up.len(), 50);
    assert_eq!(up.first(), Some(&1));
    assert_eq!(up.last(), Some(&50));
    assert!(
        elapsed < CEILING,
        "a 50-item projection over {LIBRARY} tracks took {elapsed:?} — went O(library)?"
    );
}

#[test]
fn a_hundred_thousand_advances_stay_cheap() {
    let mut s = million_track_queue();
    let steps = 100_000;
    let t = Instant::now();
    for _ in 0..steps {
        // Each natural advance is O(1): resolve one successor position.
        assert!(s.advance(Advance::Natural).is_some());
    }
    let elapsed = t.elapsed();
    assert_eq!(s.current(), Some(steps as u32));
    assert!(
        elapsed < CEILING,
        "{steps} advances over {LIBRARY} tracks took {elapsed:?} — per-advance went O(library)?"
    );
}

#[test]
fn goto_and_prev_are_o1_at_the_far_end_of_a_huge_library() {
    let mut s = million_track_queue();
    let t = Instant::now();
    // Jump straight to the last track, then step back — both O(1), no scan.
    assert_eq!(s.goto(LIBRARY - 1), Some((LIBRARY - 1) as u32));
    assert_eq!(s.prev(), Some(0), "prev restores the pre-goto anchor");
    assert!(t.elapsed() < CEILING, "goto to the far end + prev scanned?");
}

#[test]
fn reorder_and_remove_scale_with_the_queue_not_the_library() {
    // A large manual queue (bounded by the backing capacity). move/remove are
    // O(queue_len) via in-place rotation — linear in the small band, never the
    // library. Worst case: move the front all the way to the back and back again.
    const CAP: usize = 64;
    type BigUp = heapless::Deque<u32, CAP>;
    let ids: Vec<u32> = (0..10u32).collect();
    let mut s: TwoLevelQueue<SliceContext<u32, Vec<u32>>, BigUp, Hist> = TwoLevelQueue::new(
        SliceContext::new(ids),
        heapless::Deque::new(),
        heapless::Deque::new(),
    );
    for h in 0..CAP as u32 {
        s.enqueue(1000 + h).unwrap();
    }
    assert_eq!(s.upnext_len(), CAP);

    let t = Instant::now();
    for _ in 0..50_000 {
        // Net-zero churn: move front→back then back→front, so the queue contents
        // are invariant and the op is repeatable.
        assert!(s.move_upnext(0, CAP - 1));
        assert!(s.move_upnext(CAP - 1, 0));
    }
    // Correctness after 100k worst-case reorders: order fully preserved.
    assert_eq!(s.upnext_iter().next(), Some(1000), "queue order preserved");
    assert_eq!(s.upnext_len(), CAP);
    assert!(t.elapsed() < CEILING, "reorders scaled with the library?");

    // Remove-all drains in queue-length time, dropping exactly one per call.
    let t = Instant::now();
    let mut drained = 0;
    while s.remove_upnext(0).is_some() {
        drained += 1;
    }
    assert_eq!(drained, CAP);
    assert_eq!(s.upnext_len(), 0);
    assert!(t.elapsed() < CEILING, "drain scaled with the library?");
}
