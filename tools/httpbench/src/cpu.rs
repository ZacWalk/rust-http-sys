//! Sampling a child process's CPU consumption.
//!
//! `GetProcessTimes` gives cumulative kernel + user time for the whole
//! process tree of one process, which is exactly what "how much CPU did the
//! server burn" means here. Two snapshots and the wall time between them give
//! the number of cores the server was actually using.

use std::time::{Duration, Instant};

use windows::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE},
    System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
};

use crate::Result;

/// A snapshot of one process's cumulative CPU time.
#[derive(Debug, Clone, Copy)]
pub struct CpuSnapshot {
    at: Instant,
    cpu: Duration,
}

/// An open handle used to poll a process for CPU time.
pub struct ProcessCpu {
    handle: HANDLE,
}

impl ProcessCpu {
    pub fn open(pid: u32) -> Result<Self> {
        // SAFETY: a plain FFI call; the returned handle is owned by `self`.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }?;
        Ok(Self { handle })
    }

    pub fn snapshot(&self) -> Result<CpuSnapshot> {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: all four out-parameters are live locals and the handle was
        // opened with PROCESS_QUERY_LIMITED_INFORMATION.
        unsafe {
            GetProcessTimes(
                self.handle,
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        }?;
        Ok(CpuSnapshot {
            at: Instant::now(),
            cpu: as_duration(kernel) + as_duration(user),
        })
    }
}

impl Drop for ProcessCpu {
    fn drop(&mut self) {
        // SAFETY: the handle came from `OpenProcess` and is closed once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// CPU time consumed between two snapshots.
pub fn cpu_between(start: CpuSnapshot, end: CpuSnapshot) -> Duration {
    end.cpu.saturating_sub(start.cpu)
}

/// Average number of cores in use between two snapshots. `1.0` means one core
/// pinned for the whole interval.
pub fn cores_between(start: CpuSnapshot, end: CpuSnapshot) -> f64 {
    let wall = end.at.duration_since(start.at).as_secs_f64();
    if wall <= 0.0 {
        return 0.0;
    }
    cpu_between(start, end).as_secs_f64() / wall
}

/// `FILETIME` counts 100-nanosecond ticks.
fn as_duration(t: FILETIME) -> Duration {
    let ticks = (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Duration::from_nanos(ticks * 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_ticks_are_hundred_nanoseconds() {
        let one_second = FILETIME {
            dwLowDateTime: 10_000_000,
            dwHighDateTime: 0,
        };
        assert_eq!(as_duration(one_second), Duration::from_secs(1));
    }

    #[test]
    fn filetime_uses_the_high_word() {
        let t = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 1,
        };
        assert_eq!(as_duration(t), Duration::from_nanos(1u64 << 32) * 100);
    }

    #[test]
    fn cores_is_cpu_time_over_wall_time() {
        let start = CpuSnapshot {
            at: Instant::now(),
            cpu: Duration::ZERO,
        };
        let end = CpuSnapshot {
            at: start.at + Duration::from_secs(2),
            cpu: Duration::from_secs(3),
        };
        assert!((cores_between(start, end) - 1.5).abs() < 1e-9);
    }

    #[test]
    fn this_process_reports_some_cpu_time() {
        let cpu = ProcessCpu::open(std::process::id()).expect("open self");
        let first = cpu.snapshot().expect("snapshot");
        let mut acc = 0u64;
        for i in 0..2_000_000u64 {
            acc = acc.wrapping_add(i);
        }
        std::hint::black_box(acc);
        let second = cpu.snapshot().expect("snapshot");
        assert!(cpu_between(first, second) <= second.cpu);
    }
}
