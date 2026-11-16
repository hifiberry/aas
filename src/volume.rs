//! The output volume, as a percentage. On a card with a hardware volume it
//! sets the card's mixer. Otherwise it is a gain the engine applies.
//!
//! The mixer is set on the control thread, never on the audio thread.

use crate::config::BitPerfect;
use crate::format::db_to_gain;

/// The range of the volume below 100 percent, linear in dB.
const RANGE_DB: f32 = 60.0;

/// Controls tried on a card, in this order, unless the config names one.
const CONTROLS: [&str; 3] = ["Digital", "Master", "PCM"];

/// 100 percent is 0 dB and each percent below it is 0.6 dB less, so that
/// 50 is -30 dB and 1 is -59.4 dB. `None` mutes, at 0.
pub fn percent_to_db(percent: u8) -> Option<f32> {
    let percent = percent.min(100);
    if percent == 0 {
        return None;
    }
    // Divided last, so that whole dB values come out exact.
    Some((f32::from(percent) - 100.0) * RANGE_DB / 100.0)
}

/// What ALSA reports as the dB of a step that mutes, in millibel.
pub const MUTE_MB: i64 = -9_999_999;

/// How far a level may be from the one a percent sets and still count as it:
/// a control's steps are coarser than a percent, and its dB is rounded.
pub const SAME_DB: f32 = 1.0;

/// The percent a level in dB reads as, the inverse of `percent_to_db`: 1 for
/// anything quiet but not muted, 100 at 0 dB and above, 0 for a mute.
pub fn db_to_percent(db: f32) -> u8 {
    if db == f32::NEG_INFINITY {
        return 0;
    }
    if db >= 0.0 {
        return 100;
    }
    (100.0 + db * 100.0 / RANGE_DB).round().clamp(1.0, 100.0) as u8
}

/// The level of a control in dB from its raw `millibel` and whether its
/// switch is on: `f32::NEG_INFINITY` when switched off or at a mute step.
pub fn control_db(millibel: i64, switch_on: bool) -> f32 {
    if !switch_on || millibel <= MUTE_MB {
        f32::NEG_INFINITY
    } else {
        millibel as f32 / 100.0
    }
}

/// Tells a level someone else set from one `aas` set.
#[derive(Debug, Default)]
pub struct Watch {
    last: Option<f32>,
}

impl Watch {
    /// A level `aas` wrote and read back, or read at open.
    pub fn known(&mut self, db: f32) {
        self.last = Some(db);
    }

    /// The control reads `db` now: `Some(db)` if it differs from the last
    /// known or seen level, or if none was known yet.
    pub fn seen(&mut self, db: f32) -> Option<f32> {
        if self.last == Some(db) {
            return None;
        }
        self.last = Some(db);
        Some(db)
    }
}

/// A card's volume control.
pub trait HardwareVolume: Send {
    /// Sets the control to `db`, within `range_db`. `f32::NEG_INFINITY`
    /// mutes.
    fn set_db(&mut self, db: f32);
    /// The lowest and highest dB the control takes.
    fn range_db(&self) -> (f32, f32);
    /// Whether `set_db(f32::NEG_INFINITY)` silences the card. A control
    /// without a switch whose lowest step is not a mute cannot.
    fn can_mute(&self) -> bool;
    /// The level now, `f32::NEG_INFINITY` when muted. It counts as known.
    fn read_db(&mut self) -> f32;
    /// A level someone else set since the last write, read or change.
    fn changed(&mut self) -> Option<f32>;
    /// False for a control `aas` follows but applies itself.
    fn carries_volume(&self) -> bool {
        true
    }
}

/// Opens the card's volume control: on the control thread at the start,
/// and again each time the card comes back. None if the card has none.
pub type OpenVolume = Box<dyn FnMut() -> Option<Box<dyn HardwareVolume>> + Send>;

/// The control to use: the configured one if the card has it, else the
/// first of `Digital`, `Master` and `PCM` the card has. `names` are the
/// card's controls that have a playback volume in dB.
pub fn find_control(names: &[String], configured: Option<&str>) -> Option<String> {
    let has = |name: &str| names.iter().any(|n| n == name);
    match configured {
        Some(name) => has(name).then(|| name.to_string()),
        None => CONTROLS
            .into_iter()
            .find(|name| has(name))
            .map(String::from),
    }
}

/// The output volume. With a hardware control it is in hardware mode and
/// the engine gain stays at 0 dB, so bit-perfect is still possible.
/// Without one it is in software mode, and the engine gain carries it.
pub struct OutputVolume {
    hardware: Option<Box<dyn HardwareVolume>>,
    /// Forced bit-perfect: a gain the engine would apply has no effect.
    forced: bool,
    percent: u8,
    /// The level last followed, none after any `set`.
    outside: Option<f32>,
}

impl OutputVolume {
    /// Starts at 100 percent, without touching the hardware.
    pub fn new(hardware: Option<Box<dyn HardwareVolume>>, bit_perfect: BitPerfect) -> Self {
        OutputVolume {
            hardware,
            forced: bit_perfect == BitPerfect::Forced,
            percent: 100,
            outside: None,
        }
    }

    /// Takes the bit-perfect mode again, as `new` does: in forced
    /// bit-perfect a gain the engine would apply has no effect.
    pub fn set_mode(&mut self, bit_perfect: BitPerfect) {
        self.forced = bit_perfect == BitPerfect::Forced;
    }

    /// Sets the volume, from 0 to 100. More than 100 counts as 100. In
    /// hardware mode this writes to the card's mixer.
    pub fn set(&mut self, percent: u8) {
        self.percent = percent.min(100);
        self.outside = None;
        if let Some(hardware) = &mut self.hardware {
            let db = match percent_to_db(self.percent) {
                Some(db) => {
                    let (min, max) = hardware.range_db();
                    db.clamp(min, max)
                }
                None => f32::NEG_INFINITY,
            };
            hardware.set_db(db);
        }
    }

    /// Takes the level someone else set on the control as the volume,
    /// without writing the card.
    pub fn follow(&mut self, db: f32) {
        self.outside = Some(db);
        self.percent = db_to_percent(db);
    }

    /// Takes `percent` on a control that is already set: it stays if the
    /// control is at the level it would set, else the control's level is
    /// followed. Writes nothing. Without a control it is `set`.
    pub fn adopt(&mut self, percent: u8) {
        let percent = percent.min(100);
        let Some(hardware) = &mut self.hardware else {
            self.set(percent);
            return;
        };
        let read = hardware.read_db();
        let expected = match percent_to_db(percent) {
            Some(db) => {
                let (min, max) = hardware.range_db();
                db.clamp(min, max)
            }
            None => f32::NEG_INFINITY,
        };
        let same = if read.is_finite() && expected.is_finite() {
            (read - expected).abs() <= SAME_DB
        } else {
            read == expected
        };
        if same {
            self.percent = percent;
            self.outside = None;
        } else {
            self.follow(read);
        }
    }

    /// Takes a newly opened control, or none, and adopts the current volume
    /// on it. Without one the volume is carried in software from now on.
    pub fn replace_hardware(&mut self, hardware: Option<Box<dyn HardwareVolume>>) {
        self.hardware = hardware;
        self.adopt(self.percent);
    }

    pub fn percent(&self) -> u8 {
        self.percent
    }

    /// The level in dB, none when muted.
    pub fn level_db(&self) -> Option<f32> {
        match self.outside {
            Some(db) if db == f32::NEG_INFINITY => None,
            Some(db) => Some(db.min(0.0)),
            None => percent_to_db(self.percent),
        }
    }

    /// Whether a card's control carries the volume.
    pub fn is_hardware(&self) -> bool {
        self.hardware.as_ref().is_some_and(|h| h.carries_volume())
    }

    /// The gain of a further output `offset_db` from this volume. It is
    /// always applied in software, whatever carries the master's, and also
    /// in forced bit-perfect, which applies to the master only.
    pub fn further_gain(&self, offset_db: f32) -> f32 {
        self.level_db().map_or(0.0, |db| db_to_gain(db + offset_db))
    }

    /// The gain the engine applies to the output: 1 in hardware mode and
    /// with forced bit-perfect. At 0 percent on a control that cannot mute
    /// it is 0, since bit-perfect does not matter when nothing is heard.
    /// Below the lowest level of the card's control it is what the control
    /// lacks, so every percent is a step down on any card. A level followed
    /// from the control is its own, so it needs nothing more. A control that
    /// does not carry the volume leaves all of it to the engine.
    pub fn gain(&self) -> f32 {
        match &self.hardware {
            Some(hardware) if hardware.carries_volume() => {
                if self.percent == 0 && !hardware.can_mute() {
                    0.0
                } else if self.forced || self.outside.is_some() {
                    1.0
                } else {
                    match percent_to_db(self.percent) {
                        Some(db) if db < hardware.range_db().0 => {
                            db_to_gain(db - hardware.range_db().0)
                        }
                        _ => 1.0,
                    }
                }
            }
            _ if self.forced => 1.0,
            _ => self.level_db().map_or(0.0, db_to_gain),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BitPerfect;
    use crate::format::db_to_gain;
    use std::sync::{Arc, Mutex};

    /// A card's mixer that records what it is set to.
    struct FakeHardware {
        range: (f32, f32),
        mutes: bool,
        set: Arc<Mutex<Vec<f32>>>,
        /// What the control reads: set by `set_db` and by the test.
        level: Arc<Mutex<f32>>,
        watch: Watch,
        carries: bool,
    }

    impl HardwareVolume for FakeHardware {
        fn set_db(&mut self, db: f32) {
            self.set.lock().unwrap().push(db);
            *self.level.lock().unwrap() = db;
            self.watch.known(db);
        }

        fn read_db(&mut self) -> f32 {
            let level = *self.level.lock().unwrap();
            self.watch.known(level);
            level
        }

        fn changed(&mut self) -> Option<f32> {
            let level = *self.level.lock().unwrap();
            self.watch.seen(level)
        }

        fn carries_volume(&self) -> bool {
            self.carries
        }

        fn range_db(&self) -> (f32, f32) {
            self.range
        }

        fn can_mute(&self) -> bool {
            self.mutes
        }
    }

    type Recorded = Arc<Mutex<Vec<f32>>>;
    type Level = Arc<Mutex<f32>>;

    fn hardware(range: (f32, f32)) -> (Box<dyn HardwareVolume>, Recorded) {
        let (hw, set, _) = fake(range, true, true);
        (hw, set)
    }

    fn hardware_muting(range: (f32, f32), mutes: bool) -> (Box<dyn HardwareVolume>, Recorded) {
        let (hw, set, _) = fake(range, mutes, true);
        (hw, set)
    }

    /// A fake whose level starts at 0 dB, with the handle to change it.
    fn fake(
        range: (f32, f32),
        mutes: bool,
        carries: bool,
    ) -> (Box<dyn HardwareVolume>, Recorded, Level) {
        let set = Arc::new(Mutex::new(Vec::new()));
        let level = Arc::new(Mutex::new(0.0));
        let fake = FakeHardware {
            range,
            mutes,
            set: set.clone(),
            level: level.clone(),
            watch: Watch::default(),
            carries,
        };
        (Box::new(fake), set, level)
    }

    /// A fake whose level is `db` already.
    fn reading(range: (f32, f32), db: f32) -> (Box<dyn HardwareVolume>, Recorded) {
        let (hw, set, level) = fake(range, true, true);
        *level.lock().unwrap() = db;
        (hw, set)
    }

    fn last(set: &Arc<Mutex<Vec<f32>>>) -> Option<f32> {
        set.lock().unwrap().last().copied()
    }

    #[test]
    fn hundred_percent_is_0_db_and_zero_mutes() {
        assert_eq!(percent_to_db(100), Some(0.0));
        assert_eq!(percent_to_db(0), None);
        assert_eq!(percent_to_db(1), Some(-59.4));
    }

    #[test]
    fn fifty_percent_is_minus_30_db() {
        assert_eq!(percent_to_db(50), Some(-30.0));
    }

    #[test]
    fn hardware_mode_leaves_the_software_gain_at_0_db() {
        let (hw, set) = hardware((-103.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.set(50);
        assert_eq!(volume.gain(), 1.0);
        assert_eq!(last(&set), Some(-30.0));
        assert_eq!(volume.percent(), 50);
        volume.set(0);
        assert_eq!(volume.gain(), 1.0);
        assert_eq!(last(&set), Some(f32::NEG_INFINITY));
    }

    #[test]
    fn zero_on_a_control_that_cannot_mute_mutes_in_software() {
        let (hw, set) = hardware_muting((-51.5, 0.0), false);
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.set(0);
        assert_eq!(volume.gain(), 0.0);
        assert_eq!(last(&set), Some(f32::NEG_INFINITY));
        // Within its range the hardware carries it alone again.
        volume.set(20);
        assert_eq!(volume.gain(), 1.0);
        assert_eq!(last(&set), Some(-48.0));
    }

    #[test]
    fn software_mode_sets_the_engine_gain() {
        let mut volume = OutputVolume::new(None, BitPerfect::Auto);
        assert_eq!(volume.gain(), 1.0);
        volume.set(50);
        assert_eq!(volume.gain(), db_to_gain(-30.0));
        volume.set(0);
        assert_eq!(volume.gain(), 0.0);
        volume.set(100);
        assert_eq!(volume.gain(), 1.0);
    }

    #[test]
    fn forced_bit_perfect_ignores_software_volume_but_not_hardware() {
        let mut software = OutputVolume::new(None, BitPerfect::Forced);
        software.set(50);
        assert_eq!(software.gain(), 1.0);
        assert_eq!(software.percent(), 50);
        let (hw, set) = hardware((-103.5, 0.0));
        let mut hardware = OutputVolume::new(Some(hw), BitPerfect::Forced);
        hardware.set(50);
        assert_eq!(hardware.gain(), 1.0);
        assert_eq!(last(&set), Some(-30.0));
    }

    #[test]
    fn a_value_below_the_cards_range_clamps_to_its_minimum() {
        let (hw, set) = hardware((-51.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.set(10);
        assert_eq!(last(&set), Some(-51.5));
        // And above it, to its maximum.
        let (hw, set) = hardware((-60.0, -6.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.set(100);
        assert_eq!(last(&set), Some(-6.0));
    }

    #[test]
    fn software_carries_what_the_cards_range_lacks() {
        let (hw, set) = hardware((-51.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        // 10 percent is -54 dB: the card at its lowest, and 2.5 dB more.
        volume.set(10);
        assert_eq!(last(&set), Some(-51.5));
        assert!((volume.gain() - db_to_gain(-2.5)).abs() < 1e-6);
        // Within the card's range the samples are left alone.
        volume.set(20);
        assert_eq!(last(&set), Some(-48.0));
        assert_eq!(volume.gain(), 1.0);
    }

    #[test]
    fn forced_bit_perfect_stops_at_the_cards_lowest_level() {
        let (hw, set) = hardware((-51.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Forced);
        volume.set(10);
        assert_eq!(last(&set), Some(-51.5));
        assert_eq!(volume.gain(), 1.0);
    }

    #[test]
    fn a_new_control_matching_the_volume_keeps_it() {
        let (old, old_set) = hardware((-103.5, 0.0));
        let mut volume = OutputVolume::new(Some(old), BitPerfect::Auto);
        volume.set(50);
        let (new, new_set) = reading((-103.5, 0.0), -30.0);
        volume.replace_hardware(Some(new));
        assert_eq!(volume.percent(), 50);
        assert!(new_set.lock().unwrap().is_empty());
        assert_eq!(old_set.lock().unwrap().len(), 1);
        assert_eq!(volume.gain(), 1.0);
    }

    #[test]
    fn a_new_control_at_another_level_is_followed() {
        let (old, _) = hardware((-103.5, 0.0));
        let mut volume = OutputVolume::new(Some(old), BitPerfect::Auto);
        volume.set(50);
        let (new, new_set) = reading((-103.5, 0.0), 0.0);
        volume.replace_hardware(Some(new));
        assert_eq!(volume.percent(), 100);
        assert!(new_set.lock().unwrap().is_empty());
    }

    #[test]
    fn a_card_without_a_control_moves_the_volume_to_software_and_back() {
        let (hw, _) = hardware((-103.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.set(50);
        volume.replace_hardware(None);
        assert!(!volume.is_hardware());
        assert_eq!(volume.gain(), db_to_gain(-30.0));
        let (hw, set) = reading((-103.5, 0.0), -30.0);
        volume.replace_hardware(Some(hw));
        assert!(volume.is_hardware());
        assert_eq!(volume.gain(), 1.0);
        assert_eq!(volume.percent(), 50);
        assert!(set.lock().unwrap().is_empty());
    }

    #[test]
    fn a_further_output_follows_in_software_with_its_offset() {
        let mut software = OutputVolume::new(None, BitPerfect::Auto);
        software.set(50);
        assert_eq!(software.further_gain(-6.0), db_to_gain(-36.0));
        assert_eq!(software.further_gain(0.0), software.gain());
        // The master's card carries its own volume, and forced bit-perfect
        // leaves the master alone: neither reaches a further output.
        let (hw, _) = hardware((-103.5, 0.0));
        let mut hardware = OutputVolume::new(Some(hw), BitPerfect::Forced);
        hardware.set(50);
        assert_eq!(hardware.gain(), 1.0);
        assert_eq!(hardware.further_gain(-6.0), db_to_gain(-36.0));
        hardware.set(0);
        assert_eq!(hardware.further_gain(0.0), 0.0);
    }

    #[test]
    fn a_configured_control_wins_over_detection() {
        let names: Vec<String> = ["PCM", "Master", "Digital"].map(String::from).to_vec();
        assert_eq!(find_control(&names, Some("PCM")).as_deref(), Some("PCM"));
        assert_eq!(find_control(&names, None).as_deref(), Some("Digital"));
        let names: Vec<String> = ["PCM", "Master"].map(String::from).to_vec();
        assert_eq!(find_control(&names, None).as_deref(), Some("Master"));
        // A configured control the card lacks is not used.
        assert_eq!(find_control(&names, Some("Digital")), None);
        assert_eq!(find_control(&["Mic".to_string()], None), None);
    }

    #[test]
    fn db_maps_back_to_percent() {
        assert_eq!(db_to_percent(0.0), 100);
        assert_eq!(db_to_percent(-30.0), 50);
        assert_eq!(db_to_percent(-20.0), 67);
        assert_eq!(db_to_percent(3.0), 100);
        assert_eq!(db_to_percent(-59.4), 1);
        assert_eq!(db_to_percent(-80.0), 1); // quiet, not muted
        assert_eq!(db_to_percent(f32::NEG_INFINITY), 0);
    }

    #[test]
    fn control_db_reads_switch_and_mute_step() {
        assert_eq!(control_db(-3000, true), -30.0);
        assert_eq!(control_db(-3000, false), f32::NEG_INFINITY);
        assert_eq!(control_db(MUTE_MB, true), f32::NEG_INFINITY);
    }

    #[test]
    fn watch_reports_only_levels_aas_did_not_set() {
        let mut watch = Watch::default();
        assert_eq!(watch.seen(-30.0), Some(-30.0)); // nothing known yet
        assert_eq!(watch.seen(-30.0), None);
        watch.known(-20.0);
        assert_eq!(watch.seen(-20.0), None); // aas's own write
        assert_eq!(watch.seen(f32::NEG_INFINITY), Some(f32::NEG_INFINITY));
        assert_eq!(watch.seen(f32::NEG_INFINITY), None);
    }

    #[test]
    fn watch_sees_a_level_equal_to_a_percent() {
        let mut watch = Watch::default();
        watch.known(-30.0);
        assert_eq!(watch.seen(-29.4), Some(-29.4)); // what percent 51 would set
    }

    #[test]
    fn following_takes_the_level_without_writing_the_card() {
        let (hw, set) = hardware((-103.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.set(50);
        volume.follow(-20.0);
        assert_eq!(volume.percent(), 67);
        assert_eq!(set.lock().unwrap().len(), 1);
        assert_eq!(volume.gain(), 1.0);
        assert_eq!(volume.level_db(), Some(-20.0));
    }

    #[test]
    fn following_below_minus_60_reads_one() {
        let (hw, _) = hardware((-103.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.follow(-80.0);
        assert_eq!(volume.percent(), 1);
        assert_eq!(volume.gain(), 1.0);
        assert_eq!(volume.further_gain(-6.0), db_to_gain(-86.0));
    }

    #[test]
    fn following_a_mute_reads_zero() {
        let (hw, _) = hardware((-103.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.follow(f32::NEG_INFINITY);
        assert_eq!(volume.percent(), 0);
        assert_eq!(volume.gain(), 1.0);
        assert_eq!(volume.further_gain(0.0), 0.0);
    }

    #[test]
    fn following_ends_the_floor() {
        let (hw, _) = hardware((-51.5, 0.0));
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.set(10);
        assert!((volume.gain() - db_to_gain(-2.5)).abs() < 1e-6);
        volume.follow(-51.5);
        assert_eq!(volume.percent(), 14);
        assert_eq!(volume.gain(), 1.0);
        volume.set(10);
        assert!((volume.gain() - db_to_gain(-2.5)).abs() < 1e-6);
    }

    #[test]
    fn adopt_takes_the_cards_level() {
        let (hw, set) = reading((-103.5, 0.0), -20.0);
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.adopt(50);
        assert_eq!(volume.percent(), 67);
        assert!(set.lock().unwrap().is_empty());
    }

    #[test]
    fn adopt_keeps_a_percent_the_card_matches() {
        let (hw, set) = reading((-103.5, 0.0), -30.5);
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.adopt(50);
        assert_eq!(volume.percent(), 50);
        assert!(set.lock().unwrap().is_empty());
        let (hw, set) = reading((-103.5, 0.0), -31.5);
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.adopt(50);
        assert_eq!(volume.percent(), 48);
        assert!(set.lock().unwrap().is_empty());
    }

    #[test]
    fn adopt_keeps_a_percent_in_the_floor() {
        let (hw, _) = reading((-51.5, 0.0), -51.5);
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.adopt(10);
        assert_eq!(volume.percent(), 10);
        assert!((volume.gain() - db_to_gain(-2.5)).abs() < 1e-6);
    }

    #[test]
    fn adopt_of_a_muted_card() {
        let (hw, _) = reading((-103.5, 0.0), f32::NEG_INFINITY);
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.adopt(0);
        assert_eq!(volume.percent(), 0);
        volume.adopt(50);
        assert_eq!(volume.percent(), 0);
    }

    #[test]
    fn adopt_without_a_card_sets_software() {
        let mut volume = OutputVolume::new(None, BitPerfect::Auto);
        volume.adopt(50);
        assert_eq!(volume.gain(), db_to_gain(-30.0));
    }

    #[test]
    fn a_control_aas_applies_itself() {
        let (hw, set, _) = fake((-51.0, 0.0), true, false);
        let mut volume = OutputVolume::new(Some(hw), BitPerfect::Auto);
        volume.set(50);
        assert_eq!(last(&set), Some(-30.0));
        assert_eq!(volume.gain(), db_to_gain(-30.0));
        assert!(!volume.is_hardware());
        volume.follow(-20.0);
        assert_eq!(volume.gain(), db_to_gain(-20.0));
        volume.follow(f32::NEG_INFINITY);
        assert_eq!(volume.gain(), 0.0);
        let (hw, _, _) = fake((-51.0, 0.0), true, false);
        let mut forced = OutputVolume::new(Some(hw), BitPerfect::Forced);
        forced.set(50);
        assert_eq!(forced.gain(), 1.0);
    }
}
