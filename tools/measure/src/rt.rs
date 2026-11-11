//! Real-time setup shared by the probes.

use std::io::Error;

/// Lock all current and future pages, so page faults cannot distort a run.
pub fn lock_memory() -> Result<(), String> {
    // SAFETY: mlockall takes no pointers and only affects this process.
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("mlockall: {}", Error::last_os_error()))
    }
}

/// Move the calling thread to SCHED_FIFO at `priority`.
pub fn set_fifo(priority: i32) -> Result<(), String> {
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: param is a valid sched_param for the duration of the call.
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("sched_setscheduler: {}", Error::last_os_error()))
    }
}

/// Lock memory and move the calling thread to SCHED_FIFO at `priority`.
///
/// Returns whether the thread now runs with real-time rights. With
/// `may_fail`, a refusal is reported and the run goes on without them.
pub fn acquire(priority: i32, may_fail: bool) -> Result<bool, String> {
    match lock_memory().and_then(|()| set_fifo(priority)) {
        Ok(()) => Ok(true),
        Err(e) if may_fail => {
            eprintln!("continuing without real-time rights: {e}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Now on CLOCK_MONOTONIC, in seconds. The daemon logs its wake-up gaps on
/// this clock.
pub fn monotonic_s() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec for the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}
