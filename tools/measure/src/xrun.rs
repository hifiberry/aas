//! Counts xruns, so that recovering from one can never hide it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Continue,
    Recover,
    Fatal,
}

#[derive(Default)]
pub struct Counter {
    xruns: u64,
}

impl Counter {
    /// Classify the result of a write: frames written, or a negative errno.
    pub fn on_write_result(&mut self, result: i32) -> Action {
        if result >= 0 {
            Action::Continue
        } else if result == -libc::EPIPE {
            self.xruns += 1;
            Action::Recover
        } else {
            Action::Fatal
        }
    }

    pub fn count(&self) -> u64 {
        self.xruns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_epipe_is_counted_and_recovered() {
        let mut c = Counter::default();
        assert_eq!(c.on_write_result(-libc::EPIPE), Action::Recover);
        assert_eq!(c.on_write_result(-libc::EPIPE), Action::Recover);
        assert_eq!(c.count(), 2);
    }

    #[test]
    fn a_successful_write_is_not_an_xrun() {
        let mut c = Counter::default();
        assert_eq!(c.on_write_result(64), Action::Continue);
        assert_eq!(c.count(), 0);
    }

    #[test]
    fn a_suspended_or_unknown_error_is_fatal() {
        let mut c = Counter::default();
        assert_eq!(c.on_write_result(-libc::ENODEV), Action::Fatal);
    }
}
