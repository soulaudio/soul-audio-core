//! Queue-operation **throughput** benchmark — the performance baseline for the
//! transport and Up-Next model. Dependency-free (no criterion, matching
//! `soul-audio-core/benches/throughput.rs`): a hand-rolled, auto-calibrating
//! timer reporting ns/op and Mops/s per operation.
//!
//! Run: `cargo bench -p soul-audio-queue --bench queue_ops`
//!   (add `--features alloc` for the desktop `VecDeque`-backed numbers too).
//!
//! The point it proves: transport and projection cost the same over a MILLION-
//! track context as over ten (the context is virtual), and reorder/remove cost
//! scales with the small manual queue, never the library.

use std::hint::black_box;
use std::time::{Duration, Instant};

use soul_audio_engine::{Advance, Sequencer};
use soul_audio_queue::{SliceContext, TwoLevelQueue};

/// Calibrate each benchmark to at least this much wall time for a stable number.
const MIN_WALL: Duration = Duration::from_millis(150);
/// Timed batches per benchmark; the fastest (least-contended) is reported.
const REPEAT: usize = 5;
const LIBRARY: u32 = 1_000_000;
const QUEUE_CAP: usize = 64;

type Up = heapless::Deque<u32, QUEUE_CAP>;
type Hist = heapless::Deque<(u32, usize), 64>;
type Q = TwoLevelQueue<SliceContext<u32, Vec<u32>>, Up, Hist>;

fn million_track_queue() -> Q {
    let ids: Vec<u32> = (0..LIBRARY).collect();
    TwoLevelQueue::new(
        SliceContext::new(ids),
        heapless::Deque::new(),
        heapless::Deque::new(),
    )
}

/// Run `f` (one net-state-neutral operation returning a value to keep live)
/// enough times to exceed [`MIN_WALL`], then print ns/op and Mops/s.
fn bench(label: &str, ops_per_call: u32, mut f: impl FnMut() -> u64) {
    let _ = f(); // warm up
    let mut iters = 1u64;
    loop {
        let t = Instant::now();
        for _ in 0..iters {
            black_box(f());
        }
        if t.elapsed() >= MIN_WALL {
            break;
        }
        iters = iters.saturating_mul(2);
    }
    // Best-of-N: system noise only ever adds wall time, so the minimum batch is
    // the most representative — robust to shared-CI jitter.
    let mut per_call = f64::INFINITY;
    for _ in 0..REPEAT {
        let t = Instant::now();
        for _ in 0..iters {
            black_box(f());
        }
        per_call = per_call.min(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    let per_op = per_call / f64::from(ops_per_call);
    println!(
        "  {label:<30} {per_op:>9.1} ns/op   {:>8.2} Mops/s",
        1e3 / per_op
    );
}

fn main() {
    println!("\nsoul-audio-queue — transport & projection over a {LIBRARY}-track context:");

    // Transport: advance then prev (net-neutral), 2 ops/call.
    {
        let mut q = million_track_queue();
        bench("next + prev", 2, move || {
            q.advance(Advance::Natural);
            q.prev();
            u64::from(q.current().unwrap_or(0))
        });
    }
    // Explicit skip then prev (net-neutral), 2 ops/call.
    {
        let mut q = million_track_queue();
        bench("skip + prev", 2, move || {
            q.advance(Advance::Skip);
            q.prev();
            u64::from(q.current().unwrap_or(0))
        });
    }
    // Goto a cycling position (1 op/call).
    {
        let mut q = million_track_queue();
        let mut i: u32 = 0;
        bench("goto (cycling)", 1, move || {
            i = i.wrapping_add(97_003) % LIBRARY;
            q.goto(i as usize);
            u64::from(q.current().unwrap_or(0))
        });
    }
    // Bounded Up-Next projection of 50 rows (1 op/call).
    {
        let q = million_track_queue();
        bench("upcoming projection (50)", 1, move || {
            q.upcoming_iter().take(50).count() as u64
        });
    }

    println!("\nUp-Next band mutation over a {QUEUE_CAP}-item manual queue:");

    // Reorder: move front→back then back→front (net-neutral), 2 ops/call.
    {
        let mut q = million_track_queue();
        for h in 0..QUEUE_CAP as u32 {
            q.enqueue(1_000_000 + h).unwrap();
        }
        bench("reorder move (heapless)", 2, move || {
            q.move_upnext(0, QUEUE_CAP - 1);
            q.move_upnext(QUEUE_CAP - 1, 0);
            q.upnext_len() as u64
        });
    }
    // Remove front then re-append (net size-stable), 2 ops/call.
    {
        let mut q = million_track_queue();
        for h in 0..QUEUE_CAP as u32 {
            q.enqueue(1_000_000 + h).unwrap();
        }
        bench("remove + enqueue (heapless)", 2, move || {
            if let Some(h) = q.remove_upnext(0) {
                let _ = q.enqueue(h);
            }
            q.upnext_len() as u64
        });
    }

    #[cfg(feature = "alloc")]
    alloc_benches();

    println!();
}

/// Desktop profile: the same mutations over the growable `VecDeque` backing.
#[cfg(feature = "alloc")]
fn alloc_benches() {
    use soul_audio_queue::AllocQueue;

    println!("\nUp-Next band mutation over a 4096-item VecDeque queue (desktop):");
    type AQ = AllocQueue<SliceContext<u32, Vec<u32>>>;
    const BIG: u32 = 4096;

    let build = || -> AQ {
        let ids: Vec<u32> = (0..LIBRARY).collect();
        let mut q = AllocQueue::new(
            SliceContext::new(ids),
            std::collections::VecDeque::new(),
            std::collections::VecDeque::new(),
        );
        for h in 0..BIG {
            q.enqueue(1_000_000 + h).unwrap();
        }
        q
    };

    {
        let mut q = build();
        bench("reorder move (vecdeque)", 2, move || {
            q.move_upnext(0, BIG as usize - 1);
            q.move_upnext(BIG as usize - 1, 0);
            q.upnext_len() as u64
        });
    }
    {
        let q = build();
        bench("upcoming_n(500) (vecdeque)", 1, move || {
            q.upcoming_n(500).len() as u64
        });
    }
}
