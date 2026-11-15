//! The plugins that ship with the daemon, by name.

use super::balance::Balance;
use super::detect::Detect;
use super::{Channels, Failure, Formats, Param, Plugin};

/// Builds the plugin called `kind` for a stream at `rate` and blocks of
/// `channels` channels, or `None` if there is no such plugin. The built-ins
/// take every rate. `balance` and `detect` take two channels whatever
/// `channels` is, and a chain refuses them at another count.
pub fn create(kind: &str, rate: u32, _channels: usize) -> Option<Result<Box<dyn Plugin>, Failure>> {
    match kind {
        "detect" => Some(Ok(Box::new(Detect::new(rate)))),
        "balance" => Some(Ok(Box::new(Balance::new(rate)))),
        // Stand in for library plugins that do not support, or fail at,
        // the higher rates.
        #[cfg(test)]
        "only-up-to-48k" if rate <= 48_000 => Some(Ok(Box::new(Balance::new(rate)))),
        #[cfg(test)]
        "only-up-to-48k" => Some(Err(Failure::Unsupported)),
        #[cfg(test)]
        "widen" => Some(Ok(Box::new(test::Spread::widen()))),
        #[cfg(test)]
        "narrow" => Some(Ok(Box::new(test::Spread::narrow()))),
        #[cfg(test)]
        "gain" => Some(Ok(Box::new(test::Gain::new(rate)))),
        #[cfg(test)]
        "fails-above-48k" => {
            FAILS_TRIED.with(|n| n.set(n.get() + 1));
            if rate <= 48_000 {
                Some(Ok(Box::new(Balance::new(rate))))
            } else {
                Some(Err(Failure::Failed))
            }
        }
        _ => None,
    }
}

/// The channel counts of the plugin called `kind`, without building it, or
/// `None` if there is no such plugin.
pub fn channels(kind: &str) -> Option<Channels> {
    let stereo = Channels::Fixed {
        input: 2,
        output: 2,
    };
    match kind {
        "detect" | "balance" => Some(stereo),
        #[cfg(test)]
        "only-up-to-48k" | "fails-above-48k" | "gain" => Some(stereo),
        #[cfg(test)]
        "widen" => Some(test::Spread::widen().counts()),
        #[cfg(test)]
        "narrow" => Some(test::Spread::narrow().counts()),
        _ => None,
    }
}

/// The name and the parameters of the plugin called `kind`, without
/// building it for a rate, or `None` if there is no such plugin.
pub fn describe(kind: &str) -> Option<(String, Vec<Param>, Channels, Formats)> {
    let plugin: Box<dyn Plugin> = match kind {
        "detect" => Box::new(Detect::new(48_000)),
        "balance" => Box::new(Balance::new(48_000)),
        #[cfg(test)]
        "only-up-to-48k" | "fails-above-48k" => Box::new(Balance::new(48_000)),
        #[cfg(test)]
        "widen" => Box::new(test::Spread::widen()),
        #[cfg(test)]
        "narrow" => Box::new(test::Spread::narrow()),
        #[cfg(test)]
        "gain" => Box::new(test::Gain::new(48_000)),
        _ => return None,
    };
    Some((
        plugin.name().to_string(),
        plugin.params().to_vec(),
        plugin.channels(),
        plugin.formats(),
    ))
}

#[cfg(test)]
pub mod test {
    //! Plugins that change the channel count, for the tests.

    use super::super::{
        Block, Channels, Failure, Formats, Param, ParamKind, Plugin, Samples, Status,
    };
    use std::cell::RefCell;

    thread_local! {
        /// Every `set_param` of a `gain` plugin on this thread: the rate it
        /// was built for, the parameter and the value.
        pub static GAIN_SET: RefCell<Vec<(u32, usize, f64)>> = const { RefCell::new(Vec::new()) };
    }

    /// `gain`, a stand-in for the example library: a float, an integer
    /// and a text parameter, and it leaves the audio as it is. It records
    /// each `set_param` in `GAIN_SET`.
    pub struct Gain {
        rate: u32,
        params: Vec<Param>,
    }

    impl Gain {
        pub fn new(rate: u32) -> Gain {
            let param = |name: &str, unit: &str, kind, min, max| Param {
                name: name.into(),
                unit: unit.into(),
                kind,
                min,
                max,
                default: 0.0,
            };
            Gain {
                rate,
                params: vec![
                    param("gain", "dB", ParamKind::Float, -60.0, 20.0),
                    param("steps", "", ParamKind::Integer, 0.0, 8.0),
                    param("file", "", ParamKind::Text, 0.0, 0.0),
                ],
            }
        }
    }

    impl Plugin for Gain {
        fn name(&self) -> &str {
            "gain"
        }

        fn formats(&self) -> Formats {
            Formats {
                i32: true,
                f32: true,
                f64: true,
            }
        }

        fn params(&self) -> &[Param] {
            &self.params
        }

        fn set_param(&mut self, index: usize, value: f64) {
            GAIN_SET.with(|s| s.borrow_mut().push((self.rate, index, value)));
        }

        fn set_text(&mut self, _: usize, _: &str) -> Result<(), (Failure, String)> {
            Ok(())
        }

        fn neutral(&self) -> bool {
            true
        }

        fn latency_frames(&self) -> u32 {
            0
        }

        fn process(&mut self, _: Block<'_>) -> Status {
            Status::default()
        }
    }

    /// `widen` copies a stereo block onto channels 1 and 2 and 3 and 4.
    /// `narrow` takes four channels and keeps 1 and 2.
    pub struct Spread {
        input: usize,
        output: usize,
    }

    impl Spread {
        pub fn widen() -> Spread {
            Spread {
                input: 2,
                output: 4,
            }
        }

        pub fn narrow() -> Spread {
            Spread {
                input: 4,
                output: 2,
            }
        }

        pub fn counts(&self) -> Channels {
            Channels::Fixed {
                input: self.input,
                output: self.output,
            }
        }
    }

    fn spread<T: Copy>(input: &[T], cin: usize, output: &mut [T], cout: usize) {
        for (i, o) in input.chunks_exact(cin).zip(output.chunks_exact_mut(cout)) {
            for (c, s) in o.iter_mut().enumerate() {
                *s = i[c % cin];
            }
        }
    }

    impl Plugin for Spread {
        fn name(&self) -> &str {
            if self.output > self.input {
                "widen"
            } else {
                "narrow"
            }
        }

        fn channels(&self) -> Channels {
            self.counts()
        }

        fn formats(&self) -> Formats {
            Formats {
                i32: true,
                f32: true,
                f64: true,
            }
        }

        fn params(&self) -> &[Param] {
            &[]
        }

        fn set_param(&mut self, _: usize, _: f64) {}

        fn neutral(&self) -> bool {
            true
        }

        fn latency_frames(&self) -> u32 {
            0
        }

        fn process(&mut self, _: Block<'_>) -> Status {
            unreachable!("its counts differ")
        }

        fn process_into(&mut self, input: Block<'_>, output: Block<'_>) -> Status {
            let (cin, cout) = (input.channels, output.channels);
            match (input.samples, output.samples) {
                (Samples::I32(i), Samples::I32(o)) => spread(i, cin, o, cout),
                (Samples::F32(i), Samples::F32(o)) => spread(i, cin, o, cout),
                (Samples::F64(i), Samples::F64(o)) => spread(i, cin, o, cout),
                _ => panic!("formats differ"),
            }
            Status::default()
        }
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
