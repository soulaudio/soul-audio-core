//! Assert `read` and `disc_folder::parse` allocate nothing.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use soul_audio_core::SliceSource;
use soul_audio_tags::{Field, Picture, StreamFacts, TagSink, disc_folder, read};

thread_local! {
    // Per-thread, const-initialised: libtest's other threads can't perturb it
    // and first access never allocates.
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get().wrapping_add(1)));
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: forwarding to the system allocator with the same ptr/layout.
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static GA: Counting = Counting;

struct Null;
impl TagSink for Null {
    fn text(&mut self, _f: Option<Field>, _k: &str, _v: &str) {}
    fn picture(&mut self, _p: &Picture) {}
}

#[test]
fn read_does_not_allocate() {
    let data = [0u8; 64]; // not a real container; read returns Unsupported without allocating
    let mut scratch = [0u8; 256];
    let mut sink = Null;
    let before = ALLOCS.with(Cell::get);
    let mut src = SliceSource::new(&data);
    let _ = read(&mut src, &mut scratch, &mut sink);
    let after = ALLOCS.with(Cell::get);
    assert_eq!(before, after, "read allocated on the read path");
    let _ = StreamFacts::default();
}

#[test]
fn disc_folder_parse_does_not_allocate() {
    let before = ALLOCS.with(Cell::get);
    let hit = disc_folder::parse("Disc 2 - Liyue");
    let miss = disc_folder::parse("Discography");
    let after = ALLOCS.with(Cell::get);
    assert_eq!(before, after, "disc_folder::parse allocated");
    assert_eq!(
        hit.map(|d| (d.number, d.subtitle)),
        Some((2, Some("Liyue")))
    );
    assert_eq!(miss, None);
}
