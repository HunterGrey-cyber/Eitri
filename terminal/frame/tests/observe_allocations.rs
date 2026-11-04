//! What re-aligning a pinned view costs in allocations. `RawViewport::observe` runs on every PTY
//! read while the user is scrolled back, over every retained line, so anything it allocates per
//! line is allocated ten thousand times a read.
//!
//! Its own test binary, because it installs a counting global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use terminal_frame::viewport::RawViewport;

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` above, which is the system allocator's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct Dims;

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        50
    }
    fn screen_lines(&self) -> usize {
        50
    }
    fn columns(&self) -> usize {
        160
    }
}

#[test]
fn observing_a_pinned_view_does_not_allocate_per_retained_line() {
    let config = Config {
        scrolling_history: 10_000,
        ..Config::default()
    };
    let mut term = Term::new(config, &Dims, VoidListener);
    let mut parser = Processor::<StdSyncHandler>::new();
    for i in 0..10_100 {
        parser.advance(&mut term, format!("{i:06} {}\r\n", "abcdefghij".repeat(15)).as_bytes());
    }
    let mut view = RawViewport::new();
    view.pin(&term, -500);
    parser.advance(&mut term, b"one more line\r\n");
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    view.observe(&term);
    let allocations = ALLOCATIONS.load(Ordering::SeqCst) - before;
    eprintln!("{allocations} allocations to observe 10,050 lines");
    assert!(view.top_line(&term).is_some(), "still pinned: the alignment held");
    // A handful of vectors per observation (the spans, the fingerprints, the matcher's table), and
    // their growth: never one `String` per retained line, which would cost 10,000+ allocations.
    assert!(allocations < 200, "{allocations} allocations to observe 10,050 lines");
}
