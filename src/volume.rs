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
}

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
}

impl OutputVolume {
    /// Starts at 100 percent, without touching the hardware.
    pub fn new(hardware: Option<Box<dyn HardwareVolume>>, bit_perfect: BitPerfect) -> Self {
        OutputVolume {
            hardware,
            forced: bit_perfect == BitPerfect::Forced,
            percent: 100,
        }
    }

    /// Sets the volume, from 0 to 100. More than 100 counts as 100. In
    /// hardware mode this writes to the card's mixer.
    pub fn set(&mut self, percent: u8) {
        self.percent = percent.min(100);
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

    pub fn percent(&self) -> u8 {
        self.percent
    }

    /// Whether a card's control carries the volume.
    pub fn is_hardware(&self) -> bool {
        self.hardware.is_some()
    }

    /// The gain the engine applies to the output: 1 in hardware mode and
    /// with forced bit-perfect. At 0 percent on a control that cannot mute
    /// it is 0, since bit-perfect does not matter when nothing is heard.
    pub fn gain(&self) -> f32 {
        let db = percent_to_db(self.percent);
        match &self.hardware {
            Some(hardware) if db.is_none() && !hardware.can_mute() => 0.0,
            _ if self.hardware.is_some() || self.forced => 1.0,
            _ => db.map_or(0.0, db_to_gain),
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
    }

    impl HardwareVolume for FakeHardware {
        fn set_db(&mut self, db: f32) {
            self.set.lock().unwrap().push(db);
        }

        fn range_db(&self) -> (f32, f32) {
            self.range
        }

        fn can_mute(&self) -> bool {
            self.mutes
        }
    }

    type Recorded = Arc<Mutex<Vec<f32>>>;

    fn hardware(range: (f32, f32)) -> (Box<dyn HardwareVolume>, Recorded) {
        hardware_muting(range, true)
    }

    fn hardware_muting(range: (f32, f32), mutes: bool) -> (Box<dyn HardwareVolume>, Recorded) {
        let set = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeHardware {
            range,
            mutes,
            set: set.clone(),
        };
        (Box::new(fake), set)
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
        // Above 0 the hardware carries it alone again.
        volume.set(1);
        assert_eq!(volume.gain(), 1.0);
        assert_eq!(last(&set), Some(-51.5));
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
}
