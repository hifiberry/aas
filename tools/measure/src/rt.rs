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
