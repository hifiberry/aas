//! Which sound cards have an analog input.

/// A sound card `aas` knows, by the driver name in `/proc/asound/cards`.
pub struct Card {
    pub driver: &'static str,
    /// Whether the card has an ADC that capture can open.
    pub adc: bool,
}

/// The known cards. The table grows as cards are verified on hardware. An
/// unknown card is treated as having no ADC, because opening capture on a
/// Digi breaks playback.
const CARDS: &[Card] = &[
    Card {
        driver: "snd_rpi_hifiberry_dacplusadc",
        adc: true,
    },
    Card {
        driver: "snd_rpi_hifiberry_dacplusadcpro",
        adc: true,
    },
    Card {
        driver: "snd_rpi_hifiberry_digi",
        adc: false,
    },
];

/// The table row for a driver name.
pub fn lookup(driver: &str) -> Option<&'static Card> {
    CARDS.iter().find(|c| c.driver == driver)
}

/// Whether the card has an ADC. False for an unknown card.
pub fn has_adc(driver: &str) -> bool {
    lookup(driver).is_some_and(|c| c.adc)
}

/// The driver name in the first line of a card in `/proc/asound/cards`, the
/// text after ` - `. None for a line without it, such as the second line.
pub fn driver_of(card_line: &str) -> Option<&str> {
    let (_, driver) = card_line.split_once(" - ")?;
    Some(driver.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_boards_cards_have_an_adc() {
        assert!(has_adc("snd_rpi_hifiberry_dacplusadc"));
        assert!(has_adc("snd_rpi_hifiberry_dacplusadcpro"));
        assert!(lookup("snd_rpi_hifiberry_dacplusadc").unwrap().adc);
    }

    #[test]
    fn a_digi_and_an_unknown_card_have_none() {
        assert!(!has_adc("snd_rpi_hifiberry_digi"));
        assert!(lookup("snd_rpi_hifiberry_digi").is_some());
        assert!(!has_adc("snd_some_other_card"));
        assert!(lookup("snd_some_other_card").is_none());
    }

    #[test]
    fn the_driver_is_read_from_a_cards_line() {
        let first = " 0 [sndrpihifiberry]: HifiberryDacpAd - snd_rpi_hifiberry_dacplusadc";
        assert_eq!(driver_of(first), Some("snd_rpi_hifiberry_dacplusadc"));
        let second = "                      snd_rpi_hifiberry_dacplusadc";
        assert_eq!(driver_of(second), None);
        assert_eq!(driver_of(""), None);
    }
}
