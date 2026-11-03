//! Assert `read` allocates nothing on the read path.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use soul_audio_core::SliceSource;
use soul_audio_tags::{Field, Picture, StreamFacts, TagSink, read};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
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
    let before = ALLOCS.load(Ordering::SeqCst);
    let mut src = SliceSource::new(&data);
    let _ = read(&mut src, &mut scratch, &mut sink);
    let after = ALLOCS.load(Ordering::SeqCst);
    assert_eq!(before, after, "read allocated on the read path");
    let _ = StreamFacts::default();
}
