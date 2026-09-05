//! Test-only, thread-local accounting; unrelated parallel tests do not affect the count.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

struct CountedAllocator;
#[global_allocator]
static ALLOCATOR: CountedAllocator = CountedAllocator;

fn record() {
    if TRACKING.try_with(Cell::get).unwrap_or(false) {
        let _ = COUNT.try_with(|count| count.set(count.get() + 1));
    }
}

unsafe impl GlobalAlloc for CountedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

pub(crate) fn count<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TRACKING.set(false);
        }
    }
    COUNT.set(0);
    TRACKING.set(true);
    let reset = Reset;
    let result = operation();
    drop(reset);
    (result, COUNT.get())
}
