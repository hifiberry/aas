//! The plugins that ship with the daemon, by name.

use super::Plugin;
use super::balance::Balance;
use super::detect::Detect;

/// Builds the plugin called `kind` for a stream at `rate`, or `None` if
/// there is no such plugin.
pub fn create(kind: &str, rate: u32) -> Option<Box<dyn Plugin>> {
    match kind {
        "detect" => Some(Box::new(Detect::new(rate))),
        "balance" => Some(Box::new(Balance::new())),
        // Stands in for a library plugin that refuses the higher rates.
        #[cfg(test)]
        "only-up-to-48k" if rate <= 48_000 => Some(Box::new(Balance::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_kind_gives_none() {
        assert!(create("reverb", 48_000).is_none());
        assert!(create("", 48_000).is_none());
    }

    #[test]
    fn the_known_kinds_are_built() {
        assert_eq!(create("detect", 48_000).unwrap().name(), "detect");
        assert_eq!(create("balance", 48_000).unwrap().name(), "balance");
    }
}
