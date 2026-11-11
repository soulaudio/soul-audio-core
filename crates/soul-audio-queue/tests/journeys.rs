//! End-to-end *journeys*: realistic multi-step listening sessions driven through
//! the public `TwoLevelQueue` API, asserting the whole play order — not single
//! operations in isolation. Covers the transport (next/prev/skip), queue
//! management (add/play-next/reorder/remove/clear), click-to-jump, and the
//! shuffle/repeat interplay. Uses the bounded `heapless` backings the DAP ships,
//! so every journey is exercised on the embedded profile too.

use soul_audio_engine::{Advance, Sequencer};
use soul_audio_queue::{Repeat, SliceContext, TwoLevelQueue};

type Up = heapless::Deque<u32, 16>;
type Hist = heapless::Deque<(u32, usize), 32>;
type Q = TwoLevelQueue<SliceContext<u32, Vec<u32>>, Up, Hist>;

/// A queue over `items` as the source context, empty Up-Next/history.
fn q(items: &[u32]) -> Q {
    TwoLevelQueue::new(
        SliceContext::new(items.to_vec()),
        heapless::Deque::new(),
        heapless::Deque::new(),
    )
}

/// The upcoming projection as a `Vec`, bounded to `n` (works without the `alloc`
/// feature — the test binary has std).
fn upcoming(s: &Q, n: usize) -> Vec<u32> {
    s.upcoming_iter().take(n).collect()
}

#[test]
fn transport_journey_walks_forward_and_back() {
    let mut s = q(&[10, 20, 30, 40]);
    assert_eq!(s.current(), Some(10));
    // Natural ends walk the album forward.
    assert_eq!(s.advance(Advance::Natural), Some(20));
    assert_eq!(s.advance(Advance::Natural), Some(30));
    // Prev walks back through history, exactly.
    assert_eq!(s.prev(), Some(20));
    assert_eq!(s.prev(), Some(10));
    assert_eq!(s.prev(), None, "prev at the start of history is inert");
    assert_eq!(s.current(), Some(10));
    // Forward again from the restored anchor.
    assert_eq!(s.advance(Advance::Natural), Some(20));
    assert_eq!(upcoming(&s, 3), [30, 40]);
}

#[test]
fn skip_journey_matches_natural_advance_and_prev_restores() {
    let mut s = q(&[1, 2, 3, 4, 5]);
    // Explicit skips move forward just like natural ends (no Repeat::One here).
    assert_eq!(s.advance(Advance::Skip), Some(2));
    assert_eq!(s.advance(Advance::Skip), Some(3));
    assert_eq!(s.advance(Advance::Skip), Some(4));
    // Each skip recorded history, so prev retraces it.
    assert_eq!(s.prev(), Some(3));
    assert_eq!(s.prev(), Some(2));
    assert_eq!(s.current(), Some(2));
    assert_eq!(upcoming(&s, 4), [3, 4, 5]);
}

#[test]
fn queue_management_journey_add_reorder_remove_clear() {
    let mut s = q(&[10, 20, 30]); // current 10
    // "Add to Queue" three tracks, then "Play Next" one (jumps the line).
    s.enqueue(91).unwrap();
    s.enqueue(92).unwrap();
    s.enqueue(93).unwrap(); // Up-Next [91, 92, 93]
    s.play_next(99).unwrap(); // Up-Next [99, 91, 92, 93]
    assert_eq!(upcoming(&s, 6), [99, 91, 92, 93, 20, 30]);

    // Drag 93 up to second place.
    assert!(s.move_upnext(3, 1)); // Up-Next [99, 93, 91, 92]
    assert_eq!(s.upnext_iter().collect::<Vec<_>>(), [99, 93, 91, 92]);

    // Remove 99 (the row we Play-Next'd) with the ✕.
    assert_eq!(s.remove_upnext(0), Some(99)); // Up-Next [93, 91, 92]
    assert_eq!(upcoming(&s, 5), [93, 91, 92, 20, 30]);

    // The current track and the source resume point are untouched throughout.
    assert_eq!(s.current(), Some(10));

    // Clear the queue: only the manual band goes; the album remains.
    s.clear_upnext();
    assert_eq!(s.upnext_len(), 0);
    assert_eq!(upcoming(&s, 3), [20, 30]);
}

#[test]
fn click_to_jump_journey_drops_rows_above_and_prev_returns() {
    let mut s = q(&[10, 20, 30]); // current 10 at source pos 0
    for h in [91, 92, 93] {
        s.enqueue(h).unwrap(); // Up-Next [91, 92, 93]
    }
    // Click the 3rd queued row (93): play it now, dropping 91 and 92.
    assert_eq!(s.play_upnext(2), Some(93));
    assert_eq!(s.current(), Some(93));
    assert_eq!(
        s.upnext_len(),
        0,
        "the rows above the clicked one are consumed"
    );
    // The source still resumes where it was.
    assert_eq!(upcoming(&s, 3), [20, 30]);
    // Prev returns to the track we jumped away from.
    assert_eq!(s.prev(), Some(10));
    assert_eq!(s.current(), Some(10));
}

#[test]
fn shuffle_session_journey_keeps_current_and_projection_matches_playback() {
    let items = [10u32, 20, 30, 40, 50, 60, 70, 80];
    let seed = 0xDEAD_BEEF;
    let mut s = q(&items);
    s.set_repeat(Repeat::All);

    // Play a couple tracks, then flip shuffle on mid-session: the current track
    // must not change, only the upcoming order.
    assert_eq!(s.advance(Advance::Natural), Some(20));
    let now = s.current();
    s.set_shuffle(true, seed);
    assert_eq!(s.current(), now, "enabling shuffle keeps the current track");

    // The Up-Next projection must equal the real shuffled play order from here.
    let projected = upcoming(&s, 10);
    let mut played = Vec::new();
    for _ in 0..10 {
        played.push(s.advance(Advance::Natural).unwrap());
    }
    assert_eq!(projected, played, "Up Next == actual shuffled play order");
}

#[test]
fn repeat_one_then_off_journey() {
    let mut s = q(&[10, 20, 30]);
    s.set_repeat(Repeat::One);
    // A natural end loops the current track; the projection reflects the loop.
    assert_eq!(upcoming(&s, 3), [10, 10, 10]);
    assert_eq!(s.advance(Advance::Natural), Some(10));
    // A manual skip still escapes the loop.
    assert_eq!(s.advance(Advance::Skip), Some(20));
    // Back to normal: the album plays through and ends.
    s.set_repeat(Repeat::Off);
    assert_eq!(upcoming(&s, 3), [30]);
    assert_eq!(s.advance(Advance::Natural), Some(30));
    assert_eq!(
        s.advance(Advance::Natural),
        None,
        "a non-repeating album ends"
    );
}

#[test]
fn queue_survives_a_context_switch_journey() {
    let mut s = q(&[10, 20, 30]);
    s.enqueue(91).unwrap();
    s.enqueue(92).unwrap(); // Up-Next [91, 92]
    s.advance(Advance::Natural); // play 91 (queue wins); Up-Next [92]
    assert_eq!(s.current(), Some(91));

    // Start a different album: the manual queue survives, the source is replaced.
    s.set_source(SliceContext::new(vec![1, 2]), 0);
    assert_eq!(s.current(), Some(1), "new album starts at its first track");
    assert_eq!(s.source_len(), 2);
    assert_eq!(
        upcoming(&s, 4),
        [92, 2],
        "the surviving queued track plays before the new album resumes"
    );
}
