//! Loom tests for the `OverlappedWrap` completion handshake.
//!
//! Run with:
//!     RUSTFLAGS="--cfg loom" cargo test --test loom --release -- --test-threads=1
//!
//! `OverlappedWrap` itself depends on Win32 types and lives behind an IOCP
//! callback boundary that loom can't model directly. These tests reproduce
//! the *same synchronization protocol* (Release store on a flag, Acquire
//! load, then non-atomic read of the data) so loom can model-check that no
//! interleaving observes partial state.

#![cfg(loom)]

use loom::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use loom::thread;
use std::cell::UnsafeCell;

/// Stand-in for `OverlappedWrap`. Same shape, no Win32 deps.
struct Wrap {
    completed: AtomicBool,
    err: UnsafeCell<u32>,
    len: AtomicU32,
}

unsafe impl Send for Wrap {}
unsafe impl Sync for Wrap {}

impl Wrap {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            err: UnsafeCell::new(0),
            len: AtomicU32::new(0),
        }
    }

    /// Mirror of `OverlappedWrap::complete` — kernel callback side.
    fn complete(&self, err: u32, len: u32) {
        unsafe {
            *self.err.get() = err;
        }
        self.len.store(len, Ordering::Relaxed);
        self.completed.store(true, Ordering::Release);
    }

    /// Mirror of `OverlappedWrap::poll_complete` — future side. Returns
    /// `Some((err,len))` once `complete()` has been observed.
    fn try_take(&self) -> Option<(u32, u32)> {
        if self.completed.load(Ordering::Acquire) {
            // SAFETY: completed → callback finished writing err; only one
            // future-side reader.
            let err = unsafe { *self.err.get() };
            let len = self.len.load(Ordering::Relaxed);
            Some((err, len))
        } else {
            None
        }
    }
}

/// Single-completion path: kernel writes, future eventually observes.
#[test]
fn completion_is_observed_with_correct_values() {
    loom::model(|| {
        let w = Arc::new(Wrap::new());

        let cb = {
            let w = w.clone();
            thread::spawn(move || w.complete(0x1234, 42))
        };

        // Future side spins until visible (loom explores all interleavings).
        let mut observed = None;
        for _ in 0..3 {
            if let Some(v) = w.try_take() {
                observed = Some(v);
                break;
            }
            thread::yield_now();
        }
        cb.join().unwrap();

        // After joining, completion must be visible and carry the values
        // we wrote. Either we already observed them, or we observe now.
        let (err, len) = observed.unwrap_or_else(|| w.try_take().expect("must be visible"));
        assert_eq!(err, 0x1234);
        assert_eq!(len, 42);
    });
}

/// Race between completion and pre-completion poll: the poll must either
/// see Pending (then a later poll succeeds) or see the completed values —
/// never partial state.
#[test]
fn poll_never_observes_partial_state() {
    loom::model(|| {
        let w = Arc::new(Wrap::new());

        let cb = {
            let w = w.clone();
            thread::spawn(move || w.complete(7, 11))
        };

        let early = w.try_take();
        cb.join().unwrap();
        let late = w.try_take();

        // Once `late` succeeds, values must match what the callback wrote.
        let (err, len) = late.expect("must be complete after join");
        assert_eq!(err, 7);
        assert_eq!(len, 11);

        // If we did observe early, we must have observed the same thing.
        if let Some((eerr, elen)) = early {
            assert_eq!(eerr, 7);
            assert_eq!(elen, 11);
        }
    });
}
