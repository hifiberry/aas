//! The mixing matrix.

use crate::config::Route;
use crate::format::db_to_gain;

/// The most channels a block, a chain or a card has.
pub const MAX_CHANNELS: usize = 8;

/// The inputs of the matrix: `players.left`, `players.right`, `analog.left`
/// and `analog.right`.
pub const INPUTS: usize = 4;

/// The names of the inputs, by index.
pub const INPUT_NAMES: [&str; INPUTS] = [
    "players.left",
    "players.right",
    "analog.left",
    "analog.right",
];

/// A source of audio that enters the matrix as a pair of channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bus {
    Players,
    Analog,
}

impl Bus {
    /// The input indices of the bus's left and right channel.
    pub fn inputs(self) -> [usize; 2] {
        match self {
            Bus::Players => [0, 1],
            Bus::Analog => [2, 3],
        }
    }
}

/// The gains from the inputs to the channels of the output.
#[derive(Debug, Clone, PartialEq)]
pub struct Matrix {
    /// Linear gains, indexed by input and then by output channel. 0.0 is off.
    gains: [[f32; MAX_CHANNELS]; INPUTS],
    width: usize,
}

impl Matrix {
    /// Left to channel 1 and right to channel 2 for both buses, as far as
    /// `width` goes.
    pub fn default_for(width: usize) -> Matrix {
        let mut gains = [[0.0; MAX_CHANNELS]; INPUTS];
        for bus in [Bus::Players, Bus::Analog] {
            for (side, input) in bus.inputs().into_iter().enumerate() {
                if side < width {
                    gains[input][side] = 1.0;
                }
            }
        }
        Matrix { gains, width }
    }

    /// Builds the matrix from the routes of the config. Fails for a channel
    /// above `width`, or the same `from` and `to` twice. Errors name the
    /// route by its position in `routes`, from 1.
    pub fn from_routes(routes: &[Route], width: usize) -> Result<Matrix, String> {
        let mut gains = [[0.0; MAX_CHANNELS]; INPUTS];
        let mut seen = [[0usize; MAX_CHANNELS]; INPUTS];
        for (i, route) in routes.iter().enumerate() {
            let n = i + 1;
            if route.from >= INPUTS {
                return Err(format!("matrix {n}: from {} is not an input", route.from));
            }
            if route.to == 0 || route.to > width {
                return Err(format!(
                    "matrix {n}: channel {} is outside the matrix, which has {width} channel{}",
                    route.to,
                    if width == 1 { "" } else { "s" }
                ));
            }
            let channel = route.to - 1;
            if seen[route.from][channel] != 0 {
                return Err(format!(
                    "matrix {n}: {} to channel {} is already routed by matrix {}",
                    INPUT_NAMES[route.from], route.to, seen[route.from][channel]
                ));
            }
            seen[route.from][channel] = n;
            gains[route.from][channel] = db_to_gain(route.gain_db);
        }
        Ok(Matrix { gains, width })
    }

    /// The number of output channels the matrix feeds.
    pub fn width(&self) -> usize {
        self.width
    }

    /// The linear gain from an input to an output channel. 0.0 is off.
    pub fn gain(&self, input: usize, channel: usize) -> f32 {
        self.gains[input][channel]
    }

    /// For each output channel, which channel of `bus` it copies (0 left, 1
    /// right), if the routes from that bus are pure: each 0 dB, at most one
    /// per output channel. `forced` takes every route as 0 dB. Channels at or
    /// above the width are `None`, as are those the bus does not reach.
    pub fn routing(&self, bus: Bus, forced: bool) -> Option<[Option<u8>; MAX_CHANNELS]> {
        let mut out = [None; MAX_CHANNELS];
        for (channel, slot) in out.iter_mut().enumerate().take(self.width) {
            for (side, input) in bus.inputs().into_iter().enumerate() {
                let gain = self.gains[input][channel];
                if gain == 0.0 {
                    continue;
                }
                if !forced && gain != 1.0 {
                    return None;
                }
                if slot.is_some() {
                    return None;
                }
                *slot = Some(side as u8);
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(from: usize, to: usize, gain_db: f32) -> Route {
        Route { from, to, gain_db }
    }

    #[test]
    fn the_default_matrix_is_left_to_1_and_right_to_2_for_both_buses() {
        let m = Matrix::default_for(4);
        assert_eq!(m.width(), 4);
        for base in [0, 2] {
            assert_eq!(m.gain(base, 0), 1.0);
            assert_eq!(m.gain(base + 1, 1), 1.0);
            assert_eq!(m.gain(base, 1), 0.0);
            assert_eq!(m.gain(base + 1, 0), 0.0);
            for ch in 2..MAX_CHANNELS {
                assert_eq!(m.gain(base, ch), 0.0);
                assert_eq!(m.gain(base + 1, ch), 0.0);
            }
        }
    }

    #[test]
    fn the_default_matrix_for_one_channel_takes_left_only() {
        let m = Matrix::default_for(1);
        assert_eq!(m.width(), 1);
        assert_eq!(m.gain(0, 0), 1.0);
        assert_eq!(m.gain(2, 0), 1.0);
        assert_eq!(m.gain(1, 0), 0.0);
        assert_eq!(m.gain(3, 0), 0.0);
        assert_eq!(m.gain(1, 1), 0.0);
        assert_eq!(
            m.routing(Bus::Players, false),
            Some([Some(0), None, None, None, None, None, None, None])
        );
    }

    #[test]
    fn a_route_above_the_width_is_refused_with_the_channel_and_the_width() {
        let routes = [route(0, 1, 0.0), route(1, 2, 0.0), route(2, 7, 0.0)];
        let e = Matrix::from_routes(&routes, 4).unwrap_err();
        assert!(
            e.contains("matrix 3") && e.contains("channel 7") && e.contains("4 channels"),
            "{e}"
        );
        let e = Matrix::from_routes(&[route(0, 0, 0.0)], 2).unwrap_err();
        assert!(e.contains("matrix 1"), "{e}");
    }

    #[test]
    fn the_same_route_twice_is_refused() {
        let e = Matrix::from_routes(&[route(0, 1, 0.0), route(0, 1, -3.0)], 2).unwrap_err();
        assert!(e.contains("matrix 2") && e.contains("matrix 1"), "{e}");
        assert!(Matrix::from_routes(&[route(0, 1, 0.0), route(1, 1, 0.0)], 2).is_ok());
    }

    #[test]
    fn a_swap_is_pure_routing() {
        let m = Matrix::from_routes(&[route(0, 2, 0.0), route(1, 1, 0.0)], 2).unwrap();
        assert_eq!(
            m.routing(Bus::Players, false),
            Some([Some(1), Some(0), None, None, None, None, None, None])
        );
        assert_eq!(m.routing(Bus::Analog, false), Some([None; MAX_CHANNELS]));
        assert_eq!(m.gain(0, 1), 1.0);
    }

    #[test]
    fn a_sum_to_mono_is_not_pure() {
        let m = Matrix::from_routes(&[route(0, 1, 0.0), route(1, 1, 0.0)], 1).unwrap();
        assert_eq!(m.routing(Bus::Players, false), None);
        assert_eq!(m.routing(Bus::Players, true), None);
        assert!(m.routing(Bus::Analog, false).is_some());
    }

    #[test]
    fn a_route_at_minus_6_db_is_not_pure_unless_forced() {
        let m = Matrix::from_routes(&[route(0, 1, -6.0), route(1, 2, 0.0)], 2).unwrap();
        assert_eq!(m.routing(Bus::Players, false), None);
        assert_eq!(
            m.routing(Bus::Players, true),
            Some([Some(0), Some(1), None, None, None, None, None, None])
        );
        let other = Matrix::from_routes(&[route(2, 1, -6.0), route(0, 1, 0.0)], 2).unwrap();
        assert!(other.routing(Bus::Players, false).is_some());
    }

    #[test]
    fn one_source_channel_to_two_outputs_is_pure() {
        let m = Matrix::from_routes(&[route(0, 1, 0.0), route(0, 2, 0.0)], 2).unwrap();
        assert_eq!(
            m.routing(Bus::Players, false),
            Some([Some(0), Some(0), None, None, None, None, None, None])
        );
    }

    #[test]
    fn minus_6_db_is_a_gain_of_about_a_half() {
        let m = Matrix::from_routes(&[route(0, 1, -6.0)], 2).unwrap();
        assert!((m.gain(0, 0) - 0.5012).abs() < 1e-3);
    }

    #[test]
    fn swapping_the_analog_bus_leaves_the_players_bus_alone() {
        let m = Matrix::from_routes(
            &[
                route(0, 1, 0.0),
                route(1, 2, 0.0),
                route(2, 2, 0.0),
                route(3, 1, 0.0),
            ],
            2,
        )
        .unwrap();
        let analog = m.routing(Bus::Analog, false).unwrap();
        assert_eq!(&analog[..3], &[Some(1), Some(0), None]);
        let players = m.routing(Bus::Players, false).unwrap();
        assert_eq!(&players[..3], &[Some(0), Some(1), None]);
    }

    #[test]
    fn forced_takes_a_minus_100_db_route_as_pure() {
        let m = Matrix::from_routes(&[route(0, 1, -100.0), route(1, 2, 0.0)], 2).unwrap();
        assert!(m.gain(0, 0) > 0.0);
        assert_eq!(m.routing(Bus::Players, false), None);
        assert_eq!(
            m.routing(Bus::Players, true),
            Some([Some(0), Some(1), None, None, None, None, None, None])
        );
    }

    #[test]
    fn a_matrix_of_width_8_takes_routes_on_channels_7_and_8() {
        let m = Matrix::from_routes(&[route(0, 7, 0.0), route(1, 8, 0.0)], 8).unwrap();
        assert_eq!(m.width(), 8);
        assert_eq!(
            m.routing(Bus::Players, false),
            Some([None, None, None, None, None, None, Some(0), Some(1)])
        );
    }
}
