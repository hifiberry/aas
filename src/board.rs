//! Board detection, used only to pick the default latency target.

use std::fs;

/// Boards grouped by how low a latency they hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    /// Pi 5 and Compute Module 5.
    Pi5Class,
    /// Pi 4, Pi 400 and Compute Module 4: the BCM2711.
    Pi4Class,
    /// Everything else, including Pi 3 and unknown boards.
    Other,
}

/// Classifies a device tree model string.
pub fn parse_model(model: &str) -> Board {
    // The device tree ends the string with a NUL, and a test or another
    // source may add white space on either side of it. One trim takes both.
    let model = model.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if model.starts_with("Raspberry Pi 5") || model.starts_with("Raspberry Pi Compute Module 5") {
        Board::Pi5Class
    } else if model.starts_with("Raspberry Pi 4")
        || model.starts_with("Raspberry Pi 400")
        || model.starts_with("Raspberry Pi Compute Module 4")
    {
        Board::Pi4Class
    } else {
        Board::Other
    }
}

/// Reads the board from the device tree. An unreadable model gives `Other`.
pub fn detect() -> Board {
    match fs::read_to_string("/proc/device-tree/model") {
        Ok(model) => parse_model(&model),
        Err(_) => Board::Other,
    }
}

impl Board {
    /// The latency target in milliseconds when the config sets none.
    pub fn default_latency_ms(self) -> f32 {
        match self {
            Board::Pi5Class => 5.0,
            Board::Pi4Class | Board::Other => 12.0,
        }
    }

    /// The lowest latency in milliseconds the board holds safely. A Pi 4 at
    /// 6 ms lost one period in 50 minutes under load while throttled for heat,
    /// so its floor is 8.
    pub fn min_latency_ms(self) -> f32 {
        match self {
            Board::Pi5Class => 3.0,
            Board::Pi4Class => 8.0,
            Board::Other => 6.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_module_5_is_pi5_class() {
        assert_eq!(
            parse_model("Raspberry Pi Compute Module 5 Rev 1.0"),
            Board::Pi5Class
        );
        assert_eq!(
            parse_model("Raspberry Pi 5 Model B Rev 1.0"),
            Board::Pi5Class
        );
    }

    #[test]
    fn pi_4_pi_400_and_compute_module_4_are_pi4_class() {
        for m in [
            "Raspberry Pi 4 Model B Rev 1.5",
            "Raspberry Pi 400 Rev 1.0",
            "Raspberry Pi Compute Module 4 Rev 1.0",
        ] {
            assert_eq!(parse_model(m), Board::Pi4Class, "{m}");
        }
    }

    #[test]
    fn pi_3_and_unknown_are_other() {
        for m in ["Raspberry Pi 3 Model B Plus Rev 1.3", "Something Else", ""] {
            assert_eq!(parse_model(m), Board::Other, "{m}");
        }
    }

    #[test]
    fn a_trailing_nul_and_white_space_in_any_order_are_ignored() {
        for m in [
            "Raspberry Pi 5 Model B Rev 1.0\0",
            "Raspberry Pi 5 Model B Rev 1.0\n\0",
            "Raspberry Pi 5 Model B Rev 1.0\0\n",
            " \0Raspberry Pi 5 Model B Rev 1.0 \0 \n",
        ] {
            assert_eq!(parse_model(m), Board::Pi5Class, "{m:?}");
        }
    }

    #[test]
    fn defaults_follow_the_board() {
        assert_eq!(Board::Pi5Class.default_latency_ms(), 5.0);
        assert_eq!(Board::Other.default_latency_ms(), 12.0);
    }

    #[test]
    fn the_latency_figures_by_board() {
        assert_eq!(Board::Pi5Class.default_latency_ms(), 5.0);
        assert_eq!(Board::Pi5Class.min_latency_ms(), 3.0);
        assert_eq!(Board::Pi4Class.default_latency_ms(), 12.0);
        assert_eq!(Board::Pi4Class.min_latency_ms(), 8.0);
        assert_eq!(Board::Other.default_latency_ms(), 12.0);
        assert_eq!(Board::Other.min_latency_ms(), 6.0);
    }
}
