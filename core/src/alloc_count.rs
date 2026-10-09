//! A counting allocator for tests that pin "this does not copy": it counts the bytes each thread
//! asks the allocator for, so a test can measure one call on its own thread while other threads
//! (an ingestion worker, say) allocate as they please.
//!
//! A test binary opts in with `#[global_allocator] static A: Counting = Counting;` -- this crate's
//! own unit tests do (`lib.rs`), and another crate's tests can through `test-support`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

/// The system allocator, counting what each thread allocates.
pub struct Counting;

fn count(bytes: usize) {
    // `try_with`: a thread being torn down may still allocate after its counter is gone.
    let _ = BYTES.try_with(|counter| counter.set(counter.get().saturating_add(bytes)));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Runs `f` and returns what it returned with the bytes this thread allocated meanwhile. Zero
/// unless the test binary installed [`Counting`].
pub fn bytes_allocated_by<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before = BYTES.with(Cell::get);
    let result = f();
    let after = BYTES.with(Cell::get);
    (result, after - before)
}
