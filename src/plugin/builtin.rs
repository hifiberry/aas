//! The plugins that ship with the daemon, by name.

use super::balance::Balance;
use super::detect::Detect;
use super::{Failure, Plugin};

/// Builds the plugin called `kind` for a stream at `rate` and blocks of
/// `channels` channels, or `None` if there is no such plugin. The built-ins
/// take every rate. `balance` and `detect` take two channels whatever
/// `channels` is, and a chain refuses them at another count.
pub fn create(kind: &str, rate: u32, _channels: usize) -> Option<Result<Box<dyn Plugin>, Failure>> {
    match kind {
        "detect" => Some(Ok(Box::new(Detect::new(rate)))),
        "balance" => Some(Ok(Box::new(Balance::new()))),
        // Stand in for library plugins that do not support, or fail at,
        // the higher rates.
        #[cfg(test)]
        "only-up-to-48k" if rate <= 48_000 => Some(Ok(Box::new(Balance::new()))),
        #[cfg(test)]
        "only-up-to-48k" => Some(Err(Failure::Unsupported)),
        #[cfg(test)]
        "fails-above-48k" => {
            FAILS_TRIED.with(|n| n.set(n.get() + 1));
            if rate <= 48_000 {
                Some(Ok(Box::new(Balance::new())))
            } else {
                Some(Err(Failure::Failed))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
thread_local! {
    /// How often `fails-above-48k` was asked for on this thread. Per
    /// thread, so that tests running at the same time do not count each
    /// other's builds.
    pub static FAILS_TRIED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_kind_gives_none() {
        assert!(create("reverb", 48_000, 2).is_none());
        assert!(create("", 48_000, 2).is_none());
    }

    #[test]
    fn the_known_kinds_are_built() {
        let name = |kind| create(kind, 48_000, 2).unwrap().unwrap().name().to_string();
        assert_eq!(name("detect"), "detect");
        assert_eq!(name("balance"), "balance");
    }
}
