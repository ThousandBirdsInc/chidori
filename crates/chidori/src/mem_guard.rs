//! Process-level heap accounting for the sandbox memory ceiling.
//!
//! [`CountingAllocator`] wraps the backing allocator and maintains a running
//! count of live (allocated-minus-freed) bytes. The binary installs it as the
//! `#[global_allocator]` (see `main.rs`); the rust-engine watchdog
//! (`runtime::rust_engine`) samples a per-run meter on a background thread and
//! trips the VM's cooperative-cancellation flag when a run's heap growth
//! exceeds its configured cap, so a runaway agent is stopped before it can
//! OOM the host.
//!
//! Accounting has two levels:
//!   * A **process-wide** counter ([`current_allocated_bytes`]) — a cheap
//!     diagnostic statistic.
//!   * A **per-run meter** ([`RunMeterGuard`]): a run registers a meter on its
//!     execution thread and every alloc/free performed *on that thread* is
//!     charged to it. Because a VM run is single-threaded, this attributes a
//!     run's allocations to that run even under concurrent multi-agent
//!     execution — concurrent runs no longer trip each other's caps.
//!
//! Notes / limitations:
//!   * The per-run meter charges by *thread*, not by ownership: bytes a host
//!     effect allocates on other threads (e.g. tokio workers buffering an
//!     HTTP response) are not charged until they reach the run thread, and a
//!     value allocated on the run thread but freed elsewhere stays charged.
//!     For a single-threaded VM run this drift is small; the meter is clamped
//!     at zero in the negative direction.
//!   * When the allocator is not installed (e.g. `cargo test` against the lib,
//!     which has no `#[global_allocator]`), both counters stay at 0 and the
//!     watchdog's memory check is simply inert.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::Arc;

/// The allocator that actually services requests. The counting wrapper below
/// is allocator-agnostic — swap this constant to change the backing allocator.
/// A mimalloc swap was measured (2026-07) and rejected: callgrind showed it
/// cutting total instructions 10-13% on allocation-heavy JS workloads, but
/// interleaved wall-clock ran ~9% *slower* geomean across the benchmark
/// suite — glibc's tcache handles the interpreter's LIFO same-size churn
/// better than the instruction counts suggest. See
/// crates/chidori-js/benchmarks/README.md ("Build variants").
const INNER: std::alloc::System = std::alloc::System;

/// Live bytes currently allocated through [`CountingAllocator`]. Relaxed
/// ordering is sufficient: this is a monotonically-maintained statistic, not a
/// synchronization primitive, and the watchdog only needs an approximate sample.
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The meter of the run currently executing on this thread, if any.
    /// `const`-initialized so the allocator's fast path never allocates.
    static RUN_METER: RefCell<Option<Arc<AtomicIsize>>> = const { RefCell::new(None) };
}

/// Adjust both the process-wide counter and (when registered) the current
/// thread's run meter by `delta` bytes.
fn charge(delta: isize) {
    if delta >= 0 {
        ALLOCATED.fetch_add(delta as usize, Ordering::Relaxed);
    } else {
        ALLOCATED.fetch_sub(delta.unsigned_abs(), Ordering::Relaxed);
    }
    // `try_with` so an allocation during TLS teardown is counted process-wide
    // but never panics. `fetch_add` cannot allocate, so the borrow can never
    // be re-entered.
    let _ = RUN_METER.try_with(|slot| {
        if let Some(meter) = slot.borrow().as_ref() {
            meter.fetch_add(delta, Ordering::Relaxed);
        }
    });
}

/// RAII registration of a per-run allocation meter on the current thread.
///
/// Install it on the thread that will execute the run *before* agent code
/// runs; every alloc/free on this thread is then charged to [`handle`]
/// (`RunMeterGuard::handle`), which a watchdog on another thread can sample.
/// Dropping the guard (on the same thread) restores the previously registered
/// meter, so nested runs on one thread — a `callAgent` child executing inline
/// — meter themselves while registered and hand accounting back to the parent
/// afterwards.
pub struct RunMeterGuard {
    meter: Arc<AtomicIsize>,
    previous: Option<Arc<AtomicIsize>>,
}

impl RunMeterGuard {
    pub fn install() -> Self {
        let meter = Arc::new(AtomicIsize::new(0));
        let previous = RUN_METER.with(|slot| slot.borrow_mut().replace(meter.clone()));
        RunMeterGuard { meter, previous }
    }

    /// A sampling handle for the watchdog thread.
    pub fn handle(&self) -> Arc<AtomicIsize> {
        self.meter.clone()
    }
}

impl Drop for RunMeterGuard {
    fn drop(&mut self) {
        // Move the replaced Arc out of the borrow before it is released; the
        // guard and watchdog still hold references, so nothing deallocates
        // (and thus re-enters the allocator) while the slot is borrowed.
        let _replaced = RUN_METER
            .try_with(|slot| std::mem::replace(&mut *slot.borrow_mut(), self.previous.take()));
    }
}

/// Net live bytes charged to a run meter, clamped at zero (cross-thread frees
/// can push the raw counter negative).
pub fn run_meter_bytes(meter: &AtomicIsize) -> usize {
    meter.load(Ordering::Relaxed).max(0) as usize
}

/// A `#[global_allocator]`-compatible wrapper over [`INNER`] that tracks live
/// byte usage. Per-call overhead is a relaxed atomic add/sub plus a
/// thread-local check for the per-run meter — the same pattern used by crates
/// like `cap`/`stats_alloc`.
pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = INNER.alloc(layout);
        if !ptr.is_null() {
            charge(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        INNER.dealloc(ptr, layout);
        charge(-(layout.size() as isize));
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = INNER.alloc_zeroed(layout);
        if !ptr.is_null() {
            charge(layout.size() as isize);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = INNER.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            // Adjust by the net delta of the resize.
            charge(new_size as isize - layout.size() as isize);
        }
        new_ptr
    }
}

/// Current live bytes tracked by [`CountingAllocator`]. Returns 0 when the
/// allocator is not installed.
#[allow(dead_code)] // Diagnostic accessor; no production call sites yet.
pub fn current_allocated_bytes() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests drive `charge` directly and compare meter *deltas* rather
    // than absolute values: in the binary's test build the counting allocator
    // is live, so incidental allocations (Arc::new, thread bookkeeping) are
    // also charged to a registered meter.

    #[test]
    fn meter_charges_only_while_registered_on_this_thread() {
        charge(64); // no meter registered — process counter only
        let guard = RunMeterGuard::install();
        let meter = guard.handle();
        let base = run_meter_bytes(&meter);
        charge(128);
        charge(-28);
        assert_eq!(run_meter_bytes(&meter), base + 100);
        let at_drop = run_meter_bytes(&meter);
        drop(guard);
        charge(512); // after unregistration nothing is charged to the meter
        assert_eq!(run_meter_bytes(&meter), at_drop);
    }

    #[test]
    fn meter_is_clamped_at_zero_for_cross_thread_frees() {
        let guard = RunMeterGuard::install();
        let meter = guard.handle();
        // A buffer allocated elsewhere but freed on the run thread drives the
        // raw counter negative; the sampled value clamps at zero.
        charge(-(1 << 30));
        assert_eq!(run_meter_bytes(&meter), 0);
    }

    #[test]
    fn nested_guards_restore_the_parent_meter() {
        let parent = RunMeterGuard::install();
        let parent_meter = parent.handle();
        charge(10);

        let child = RunMeterGuard::install();
        let child_meter = child.handle();
        let parent_paused_at = run_meter_bytes(&parent_meter);
        let child_base = run_meter_bytes(&child_meter);
        charge(7);
        // The child meters itself; the parent is paused.
        assert_eq!(run_meter_bytes(&child_meter), child_base + 7);
        assert_eq!(run_meter_bytes(&parent_meter), parent_paused_at);
        drop(child);

        // Accounting hands back to the parent after the child completes.
        charge(5);
        assert_eq!(run_meter_bytes(&parent_meter), parent_paused_at + 5);
        assert_eq!(run_meter_bytes(&child_meter), child_base + 7);
    }

    #[test]
    fn other_threads_do_not_charge_this_runs_meter() {
        let guard = RunMeterGuard::install();
        let meter = guard.handle();
        let base = run_meter_bytes(&meter) as isize;
        std::thread::spawn(|| charge(1 << 20)).join().unwrap();
        // Thread spawn/join bookkeeping allocates a little on this thread;
        // the megabyte charged on the other thread must not land here.
        let after = run_meter_bytes(&meter) as isize;
        assert!(
            (after - base).abs() < (1 << 19),
            "cross-thread charge leaked into the run meter: {base} -> {after}"
        );
    }
}
