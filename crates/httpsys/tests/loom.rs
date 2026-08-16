//! Loom model of the `OverlappedWrap` completion handshake.
//!
//! Run with:
//!     $env:RUSTFLAGS = "--cfg loom"
//!     cargo test -p httpsys --test loom --release -- --test-threads=1
//!     Remove-Item Env:\RUSTFLAGS
//!
//! `OverlappedWrap` itself is welded to Win32 types and an IOCP boundary that
//! loom cannot model. These tests reproduce the same protocol — Relaxed stores
//! of the payload, a Release store on `completed`, an Acquire load before
//! reading it back — so loom can check that no interleaving lets a reader
//! observe `completed` without the values that belong with it.

#![cfg(loom)]

use loom::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use loom::thread;

/// Stand-in for `OverlappedWrap`, minus the Win32 fields.
struct Wrap {
    completed: AtomicBool,
    err: AtomicU32,
    len: AtomicU32,
}

impl Wrap {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            err: AtomicU32::new(0),
            len: AtomicU32::new(0),
        }
    }

    /// Mirror of `OverlappedWrap::complete` — the completion-thread side.
    fn complete(&self, err: u32, len: u32) {
        self.err.store(err, Ordering::Relaxed);
        self.len.store(len, Ordering::Relaxed);
        self.completed.store(true, Ordering::Release);
    }

    /// Mirror of `OverlappedWrap::poll_complete` — the future side.
    fn try_take(&self) -> Option<(u32, u32)> {
        if self.completed.load(Ordering::Acquire) {
            Some((
                self.err.load(Ordering::Relaxed),
                self.len.load(Ordering::Relaxed),
            ))
        } else {
            None
        }
    }
}

/// A completion is eventually observed, carrying exactly the values written.
#[test]
fn completion_is_observed_with_correct_values() {
    loom::model(|| {
        let wrap = Arc::new(Wrap::new());

        let completer = {
            let wrap = wrap.clone();
            thread::spawn(move || wrap.complete(0x1234, 42))
        };

        let mut observed = None;
        for _ in 0..3 {
            if let Some(value) = wrap.try_take() {
                observed = Some(value);
                break;
            }
            thread::yield_now();
        }
        completer.join().unwrap();

        let result = observed.unwrap_or_else(|| wrap.try_take().expect("must be visible"));
        assert_eq!(result, (0x1234, 42));
    });
}

/// Racing a poll against the completion must yield either Pending or the whole
/// result — never a half-written one.
#[test]
fn poll_never_observes_partial_state() {
    loom::model(|| {
        let wrap = Arc::new(Wrap::new());

        let completer = {
            let wrap = wrap.clone();
            thread::spawn(move || wrap.complete(7, 11))
        };

        let early = wrap.try_take();
        completer.join().unwrap();

        assert_eq!(wrap.try_take(), Some((7, 11)));
        if let Some(observed) = early {
            assert_eq!(observed, (7, 11));
        }
    });
}

/// The cancellation path spins on `completed`; it must not be able to leave
/// the spin before the payload is visible.
#[test]
fn blocking_wait_sees_the_payload() {
    loom::model(|| {
        let wrap = Arc::new(Wrap::new());

        let completer = {
            let wrap = wrap.clone();
            // ERROR_OPERATION_ABORTED, what CancelIoEx produces.
            thread::spawn(move || wrap.complete(995, 0))
        };

        let mut result = None;
        while result.is_none() {
            result = wrap.try_take();
            if result.is_none() {
                thread::yield_now();
            }
        }
        assert_eq!(result, Some((995, 0)));
        completer.join().unwrap();
    });
}
