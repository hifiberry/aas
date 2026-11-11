//! Decides whether the output is bit-perfect.

use crate::config::BitPerfect;

/// Why the output is not bit-perfect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotBitPerfect {
    /// The setting is off.
    Disabled,
    /// More than one source is mixed.
    SeveralSources,
    /// A volume that aas calculates is not at 0 dB.
    Volume,
    /// A client is being resampled.
    Resampled,
    /// Nothing is playing.
    NoSource,
}

/// What the engine knows about the current cycle.
#[derive(Debug, Clone, Copy)]
pub struct Situation {
    pub mode: BitPerfect,
    /// Number of active sources.
    pub active: usize,
    /// All volumes that aas calculates are at 0 dB.
    pub software_volume_at_0db: bool,
    pub resampled: bool,
}

/// Returns `Ok(())` when samples can be copied to the card untouched.
///
/// `Off` is never bit-perfect, whatever plays. Otherwise it takes exactly
/// one source that is not resampled. Forced mode does not change that: a
/// client at another rate than the output is still resampled, and is not
/// bit-perfect. Forced mode only sets aside the volumes that aas calculates.
pub fn decide(s: &Situation) -> Result<(), NotBitPerfect> {
    if s.mode == BitPerfect::Off {
        return Err(NotBitPerfect::Disabled);
    }
    match s.active {
        0 => return Err(NotBitPerfect::NoSource),
        1 => {}
        _ => return Err(NotBitPerfect::SeveralSources),
    }
    if s.resampled {
        return Err(NotBitPerfect::Resampled);
    }
    if s.mode == BitPerfect::Auto && !s.software_volume_at_0db {
        return Err(NotBitPerfect::Volume);
    }
    Ok(())
}

/// Whether volumes that aas calculates take effect in this mode.
pub fn volumes_apply(mode: BitPerfect) -> bool {
    mode != BitPerfect::Forced
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sit(mode: BitPerfect, active: usize, sw: bool, resampled: bool) -> Situation {
        Situation {
            mode,
            active,
            software_volume_at_0db: sw,
            resampled,
        }
    }

    #[test]
    fn one_source_at_0db_is_bit_perfect() {
        assert_eq!(decide(&sit(BitPerfect::Auto, 1, true, false)), Ok(()));
    }

    #[test]
    fn only_the_software_volume_decides_in_automatic_mode() {
        // The card's own volume is not part of the situation. A volume the
        // card carries leaves the software gain at 0 dB, and only that gain
        // is asked about.
        for resampled in [false, true] {
            for active in 0..3 {
                let at_0db = decide(&sit(BitPerfect::Auto, active, true, resampled));
                let lowered = decide(&sit(BitPerfect::Auto, active, false, resampled));
                let single = active == 1 && !resampled;
                assert_eq!(at_0db.is_ok(), single);
                assert_ne!(lowered, Ok(()));
                // The volume is the last reason given, after the others.
                assert_eq!(lowered == Err(NotBitPerfect::Volume), single);
            }
        }
        assert!(volumes_apply(BitPerfect::Auto));
    }

    #[test]
    fn software_volume_blocks_automatic_bit_perfect() {
        assert_eq!(
            decide(&sit(BitPerfect::Auto, 1, false, false)),
            Err(NotBitPerfect::Volume)
        );
    }

    #[test]
    fn forced_ignores_software_volume() {
        assert_eq!(decide(&sit(BitPerfect::Forced, 1, false, false)), Ok(()));
        assert!(!volumes_apply(BitPerfect::Forced));
    }

    #[test]
    fn forced_with_two_sources_mixes_them() {
        assert_eq!(
            decide(&sit(BitPerfect::Forced, 2, true, false)),
            Err(NotBitPerfect::SeveralSources)
        );
        assert_eq!(
            decide(&sit(BitPerfect::Auto, 2, true, false)),
            Err(NotBitPerfect::SeveralSources)
        );
    }

    #[test]
    fn off_is_never_bit_perfect() {
        assert_eq!(
            decide(&sit(BitPerfect::Off, 1, true, false)),
            Err(NotBitPerfect::Disabled)
        );
        assert!(volumes_apply(BitPerfect::Off));
    }

    #[test]
    fn off_gives_its_own_reason_with_no_source_or_several() {
        for active in [0, 2, 5] {
            for resampled in [false, true] {
                assert_eq!(
                    decide(&sit(BitPerfect::Off, active, false, resampled)),
                    Err(NotBitPerfect::Disabled)
                );
            }
        }
    }

    #[test]
    fn a_resampled_client_is_never_bit_perfect() {
        assert_eq!(
            decide(&sit(BitPerfect::Auto, 1, true, true)),
            Err(NotBitPerfect::Resampled)
        );
        assert_eq!(
            decide(&sit(BitPerfect::Forced, 1, true, true)),
            Err(NotBitPerfect::Resampled)
        );
    }

    #[test]
    fn no_source_is_not_bit_perfect() {
        assert_eq!(
            decide(&sit(BitPerfect::Auto, 0, true, false)),
            Err(NotBitPerfect::NoSource)
        );
    }
}
