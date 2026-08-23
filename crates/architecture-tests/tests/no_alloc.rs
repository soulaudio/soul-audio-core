//! Prove `soul-audio-core` allocates nothing at runtime by installing a
//! counting global allocator and asserting the counter does not move while core
//! code runs.
//!
//! TEMPLATE: decoders are stubs, so this currently measures the `SliceReader`
//! plumbing. Once you implement a decoder, wrap a full `decode_into` loop in the
//! measured region — the assertion is what enforces the no-alloc contract at
//! runtime (the `thumbv7em` build enforces it at compile time).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use embedded_io::Read;
use soul_audio_core::SliceReader;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarding to the system allocator with the same ptr/layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GA: Counting = Counting;

#[test]
fn core_io_does_not_allocate() {
    // Buffers are set up BEFORE measuring — that setup may allocate.
    let data = [0u8; 256];
    let mut out = [0u8; 64];
    let mut reader = SliceReader::new(&data);

    let before = ALLOCS.load(Ordering::SeqCst);
    // Pure no-alloc plumbing: read the whole slice in chunks.
    while reader.read(&mut out).unwrap() != 0 {}
    let after = ALLOCS.load(Ordering::SeqCst);

    assert_eq!(before, after, "soul-audio-core io plumbing allocated");
}
