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
    /// A plugin that alters the audio is in a chain. Automatic mode only;
    /// forced mode bypasses plugins.
    Plugin,
    /// Nothing is playing.
    NoSource,
    /// The matrix scales the one source, or sums its channels.
    Routing,
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
    /// Every plugin in the chains the output passes through leaves the
    /// audio as it is. Only automatic mode asks: forced mode bypasses them.
    pub plugins_neutral: bool,
    /// The matrix routes the bus of the one active source purely: each
    /// route at 0 dB, at most one into an output channel. In forced mode
    /// every route counts as 0 dB. True with no source active.
    pub routing_pure: bool,
}

/// Returns `Ok(())` when samples can be copied to the card untouched.
///
/// `Off` is never bit-perfect, whatever plays. Otherwise it takes exactly
/// one source that is not resampled. Automatic mode also takes plugins that
/// leave the audio as it is and volumes at 0 dB. Forced mode sets aside the
/// volumes that aas calculates and bypasses the plugins, so neither counts
/// there. A client at another rate than the output is still resampled, so
/// it is not bit-perfect in either mode. Both modes take pure routing of
/// the source's bus through the matrix, which forced mode judges with
/// every route at 0 dB.
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
    if plugins_apply(s.mode) && !s.plugins_neutral {
        return Err(NotBitPerfect::Plugin);
    }
    if s.mode == BitPerfect::Auto && !s.software_volume_at_0db {
        return Err(NotBitPerfect::Volume);
    }
    if !s.routing_pure {
        return Err(NotBitPerfect::Routing);
    }
    Ok(())
}

/// Whether volumes that aas calculates take effect in this mode.
pub fn volumes_apply(mode: BitPerfect) -> bool {
    mode != BitPerfect::Forced
}

/// Whether plugins act on the audio in this mode. In forced mode they are
/// bypassed. The analog chain still runs there, for its detector and for
/// the recorders: its output is not played.
pub fn plugins_apply(mode: BitPerfect) -> bool {
    mode != BitPerfect::Forced
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sit(mode: BitPerfect, active: usize, sw: bool, resampled: bool) -> Situation {
        with_plugins(mode, active, sw, resampled, true)
    }

    fn with_plugins(
        mode: BitPerfect,
        active: usize,
        sw: bool,
        resampled: bool,
        plugins_neutral: bool,
    ) -> Situation {
        Situation {
            mode,
            active,
            software_volume_at_0db: sw,
            resampled,
            plugins_neutral,
            routing_pure: true,
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

    #[test]
    fn a_plugin_that_is_not_neutral_stops_automatic_bit_perfect() {
        let busy = |active, sw, resampled| {
            decide(&with_plugins(
                BitPerfect::Auto,
                active,
                sw,
                resampled,
                false,
            ))
        };
        assert_eq!(busy(1, true, false), Err(NotBitPerfect::Plugin));
        // After the resampler, before the volume.
        assert_eq!(busy(1, true, true), Err(NotBitPerfect::Resampled));
        assert_eq!(busy(1, false, false), Err(NotBitPerfect::Plugin));
        assert_eq!(busy(2, true, false), Err(NotBitPerfect::SeveralSources));
        assert_eq!(busy(0, true, false), Err(NotBitPerfect::NoSource));
        assert!(plugins_apply(BitPerfect::Auto));
        assert!(plugins_apply(BitPerfect::Off));
        assert_eq!(
            decide(&with_plugins(BitPerfect::Off, 1, true, false, false)),
            Err(NotBitPerfect::Disabled)
        );
    }

    #[test]
    fn impure_routing_is_reported_as_the_reason() {
        let routed = |mode, active, sw, resampled, plugins_neutral| {
            decide(&Situation {
                routing_pure: false,
                ..with_plugins(mode, active, sw, resampled, plugins_neutral)
            })
        };
        assert_eq!(
            routed(BitPerfect::Auto, 1, true, false, true),
            Err(NotBitPerfect::Routing)
        );
        assert_eq!(
            routed(BitPerfect::Forced, 1, false, false, false),
            Err(NotBitPerfect::Routing)
        );
        // The other reasons come first.
        assert_eq!(
            routed(BitPerfect::Off, 1, true, false, true),
            Err(NotBitPerfect::Disabled)
        );
        assert_eq!(
            routed(BitPerfect::Auto, 0, true, false, true),
            Err(NotBitPerfect::NoSource)
        );
        assert_eq!(
            routed(BitPerfect::Auto, 2, true, false, true),
            Err(NotBitPerfect::SeveralSources)
        );
        assert_eq!(
            routed(BitPerfect::Auto, 1, true, true, true),
            Err(NotBitPerfect::Resampled)
        );
        assert_eq!(
            routed(BitPerfect::Auto, 1, true, false, false),
            Err(NotBitPerfect::Plugin)
        );
        assert_eq!(
            routed(BitPerfect::Auto, 1, false, false, true),
            Err(NotBitPerfect::Volume)
        );
    }

    #[test]
    fn forced_bit_perfect_bypasses_plugins() {
        let busy = |active, sw, resampled| {
            decide(&with_plugins(
                BitPerfect::Forced,
                active,
                sw,
                resampled,
                false,
            ))
        };
        assert_eq!(busy(1, true, false), Ok(()));
        assert_eq!(busy(1, false, false), Ok(()));
        assert_eq!(busy(1, true, true), Err(NotBitPerfect::Resampled));
        assert_eq!(busy(2, true, false), Err(NotBitPerfect::SeveralSources));
        assert_eq!(busy(0, true, false), Err(NotBitPerfect::NoSource));
        assert!(!plugins_apply(BitPerfect::Forced));
    }
}
