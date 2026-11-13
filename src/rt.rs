//! Real-time setup for the daemon.

use std::io::Error;

/// The audio thread's SCHED_FIFO priority.
pub const AUDIO_PRIORITY: i32 = 70;

/// The SCHED_FIFO priority of a further output's writer thread, below the
/// audio thread, which never waits for it.
pub const WRITER_PRIORITY: i32 = 65;

/// Lock all current and future pages, so that a page fault cannot delay the
/// audio thread.
pub fn lock_memory() -> Result<(), String> {
    // SAFETY: mlockall takes no pointers and only affects this process.
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("mlockall: {}", Error::last_os_error()))
    }
}

/// The kernel setting that lets memory compaction move locked pages.
const COMPACT_UNEVICTABLE: &str = "/proc/sys/vm/compact_unevictable_allowed";

/// Whether the kernel may move locked pages when it compacts memory.
///
/// Locking keeps a page in memory but not in place. While compaction moves a
/// page, a thread that touches it waits for the move to end, which has stalled
/// the audio thread for 15 to 34 ms on a Pi 3 under load. `None` when the
/// setting cannot be read.
pub fn compaction_moves_locked_pages() -> Option<bool> {
    parse_compact_unevictable(&std::fs::read_to_string(COMPACT_UNEVICTABLE).ok()?)
}

fn parse_compact_unevictable(text: &str) -> Option<bool> {
    match text.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compaction_setting_is_read_as_the_kernel_writes_it() {
        assert_eq!(parse_compact_unevictable("1\n"), Some(true));
        assert_eq!(parse_compact_unevictable("0\n"), Some(false));
        assert_eq!(parse_compact_unevictable(""), None);
        assert_eq!(parse_compact_unevictable("2\n"), None);
    }
}
