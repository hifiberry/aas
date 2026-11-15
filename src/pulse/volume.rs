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

pub fn db_millis(cvolume: &[u32]) -> i32 {
    let level = average(cvolume);
    if level == 0 {
        return -60_000;
    }
    let linear = f64::from(level) / f64::from(wire::VOLUME_NORM);
    ((20.0 * linear.log10() * 1000.0) as i32).clamp(-60_000, 0)
}

pub fn cvolume_from_db(db_millis: i32) -> [u32; 2] {
    let db = f64::from(db_millis.clamp(-60_000, 0)) / 1000.0;
    let linear = 10f64.powf(db / 20.0);
    let level = (linear * f64::from(wire::VOLUME_NORM)) as u32;
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
    fn a_stream_volume_becomes_db() {
        assert_eq!(db_millis(&[65_536, 65_536]), 0);
        assert_eq!(db_millis(&[32_768, 32_768]), -6020);
        assert_eq!(db_millis(&[0, 0]), -60_000);
    }

    #[test]
    fn db_becomes_a_cvolume() {
        assert_eq!(cvolume_from_db(0), [wire::VOLUME_NORM; 2]);
        assert_eq!(cvolume_from_db(-60_000), [65; 2]);
    }
}
