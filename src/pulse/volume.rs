//! PulseAudio volumes and the daemon's percent and dB.

use crate::pulse::wire;

fn average(cvolume: &[u32]) -> u32 {
    if cvolume.is_empty() {
        return 0;
    }
    let sum: u64 = cvolume.iter().map(|&value| u64::from(value)).sum();
    (sum / cvolume.len() as u64) as u32
}

pub fn percent(cvolume: &[u32]) -> u8 {
    let level = (u64::from(average(cvolume)) * 100 + u64::from(wire::VOLUME_NORM) / 2)
        / u64::from(wire::VOLUME_NORM);
    level.min(100) as u8
}

/// A stream volume in millidB, on PulseAudio's software curve: the gain is
/// the cube of `v / PA_VOLUME_NORM`, so `dB = 60 log10(v / NORM)`. This is
/// what `pa_sw_volume_from_dB` and `pactl` assume. Clamped to the client
/// gain's range, -60 to 0 dB; silence is `is_silent`, not a gain.
pub fn db_millis(cvolume: &[u32]) -> i32 {
    let level = average(cvolume);
    if level == 0 {
        return -60_000;
    }
    let ratio = f64::from(level) / f64::from(wire::VOLUME_NORM);
    ((60.0 * ratio.log10() * 1000.0).round() as i32).clamp(-60_000, 0)
}

/// Whether a stream volume is PulseAudio's silence, `PA_VOLUME_MUTED`.
pub fn is_silent(cvolume: &[u32]) -> bool {
    average(cvolume) == 0
}

/// The stream volume for a gain in millidB, the inverse of `db_millis`.
pub fn cvolume_from_db(db_millis: i32) -> [u32; 2] {
    let db = f64::from(db_millis.clamp(-60_000, 0)) / 1000.0;
    let level = (10f64.powf(db / 60.0) * f64::from(wire::VOLUME_NORM)).round() as u32;
    [level.min(wire::VOLUME_NORM); 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cvolume_reports_the_aas_percent() {
        assert_eq!(percent(&[65_536, 65_536]), 100);
        assert_eq!(percent(&[32_768, 32_768]), 50);
        assert_eq!(percent(&[0, 0]), 0);
    }

    #[test]
    fn a_stream_volume_follows_pulseaudios_cubic_curve() {
        assert_eq!(db_millis(&[65_536, 65_536]), 0);
        // pactl shows 50 % as -18.06 dB.
        assert_eq!(db_millis(&[32_768, 32_768]), -18_062);
        // 10 % is the -60 dB floor; below it the gain stays at the floor.
        assert_eq!(db_millis(&[6_553, 6_553]), -60_000);
        assert_eq!(db_millis(&[1_000, 1_000]), -60_000);
        assert!(is_silent(&[0, 0]));
        assert!(!is_silent(&[1, 1]));
    }

    #[test]
    fn db_becomes_a_cvolume_on_the_same_curve() {
        assert_eq!(cvolume_from_db(0), [wire::VOLUME_NORM; 2]);
        // pa_sw_volume_from_dB(-6) is 52057.
        assert_eq!(cvolume_from_db(-6_000), [52_057; 2]);
        assert_eq!(cvolume_from_db(-60_000), [6_554; 2]);
        for level in [6_554, 20_000, 32_768, 65_536] {
            assert!(cvolume_from_db(db_millis(&[level; 2]))[0].abs_diff(level) <= 2);
        }
    }
}
