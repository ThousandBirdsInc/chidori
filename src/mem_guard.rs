//! Process-level heap accounting for the sandbox memory ceiling.
//!
//! [`CountingAllocator`] wraps the system allocator and maintains a running
//! count of live (allocated-minus-freed) bytes. The binary installs it as the
//! `#[global_allocator]` (see `main.rs`); the rust-engine watchdog
//! (`runtime::rust_engine`) samples [`current_allocated_bytes`] on a background
//! thread and trips the VM's cooperative-cancellation flag when a run's heap
//! growth exceeds its configured cap, so a runaway agent is stopped before it
//! can OOM the host.
//!
//! Notes / limitations:
//!   * The counter is **process-wide**, not per-VM. The watchdog therefore caps
//!     *baseline-relative* growth (`current - baseline_at_run_start`); under
//!     heavy concurrency one run's allocations can be attributed to another, so
//!     the cap is a coarse safety backstop tuned generously, not a precise
//!     per-agent quota. Precise per-VM accounting is a documented follow-up.
//!   * When the allocator is not installed (e.g. `cargo test` against the lib,
//!     which has no `#[global_allocator]`), the counter stays at 0 and the
//!     watchdog's memory check is simply inert.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Live bytes currently allocated through [`CountingAllocator`]. Relaxed
/// ordering is sufficient: this is a monotonically-maintained statistic, not a
/// synchronization primitive, and the watchdog only needs an approximate sample.
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// A `#[global_allocator]`-compatible wrapper over [`System`] that tracks live
/// byte usage. Per-call overhead is a single relaxed atomic add/sub — the same
/// pattern used by crates like `cap`/`stats_alloc`.
pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            // Adjust by the net delta of the resize.
            let old = layout.size();
            if new_size >= old {
                ALLOCATED.fetch_add(new_size - old, Ordering::Relaxed);
            } else {
                ALLOCATED.fetch_sub(old - new_size, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

/// Current live bytes tracked by [`CountingAllocator`]. Returns 0 when the
/// allocator is not installed.
pub fn current_allocated_bytes() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}
