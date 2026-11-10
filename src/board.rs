//! Board detection, used only to pick the default latency target.

use std::fs;

/// Boards grouped by how low a latency they hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    /// Pi 5 and Compute Module 5.
    Pi5Class,
    /// Everything else, including Pi 3, Pi 4 and unknown boards.
    Other,
}

/// Classifies a device tree model string.
pub fn parse_model(model: &str) -> Board {
    let model = model.trim_end_matches('\0').trim();
    if model.starts_with("Raspberry Pi 5") || model.starts_with("Raspberry Pi Compute Module 5") {
        Board::Pi5Class
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
            Board::Pi5Class => 4.0,
            Board::Other => 12.0,
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
    fn pi_3_and_pi_4_and_unknown_are_other() {
        for m in [
            "Raspberry Pi 3 Model B Plus Rev 1.3",
            "Raspberry Pi 4 Model B Rev 1.5",
            "Raspberry Pi Compute Module 4 Rev 1.0",
            "Something Else",
            "",
        ] {
            assert_eq!(parse_model(m), Board::Other, "{m}");
        }
    }

    #[test]
    fn defaults_follow_the_board() {
        assert_eq!(Board::Pi5Class.default_latency_ms(), 4.0);
        assert_eq!(Board::Other.default_latency_ms(), 12.0);
    }
}
