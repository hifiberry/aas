//! The sound card as a sink, through ALSA.
//!
//! Finding the card and converting samples need no ALSA, so they build and
//! are tested everywhere. `AlsaSink`, `AlsaCapture` and `AlsaVolume` need
//! the `alsa-output` feature.

use crate::capture::OpenError;
use crate::config::{Clock, Config};
use std::fs;

/// Where the kernel lists the sound cards.
const CARDS: &str = "/proc/asound/cards";
const PCMS: &str = "/proc/asound/pcm";

/// The card to play to: the first one that is not a loopback, the Pi's
/// headphone jack or its HDMI output. A card named in the config is used
/// instead, without this.
pub fn find_card(ids: &[String]) -> Option<String> {
    ids.iter()
        .find(|id| !matches!(id.as_str(), "Loopback" | "Headphones") && !id.starts_with("vc4hdmi"))
        .cloned()
}

/// The card ids in `/proc/asound/cards` text, in card order.
pub fn parse_card_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        .filter_map(|line| {
            let start = line.find('[')? + 1;
            let end = start + line[start..].find(']')?;
            Some(line[start..end].trim().to_string())
        })
        .collect()
}

/// The ids of the cards present now.
pub fn card_ids() -> Vec<String> {
    fs::read_to_string(CARDS)
        .map(|text| parse_card_ids(&text))
        .unwrap_or_default()
}

/// The id and driver name of the card to play to, in `/proc/asound/cards`
/// text: the one the config names, by id or index, or else the one
/// `find_card` takes. None if it is not there.
pub fn card_driver(text: &str, card: Option<&str>) -> Option<(String, String)> {
    let ids = parse_card_ids(text);
    let id = match card {
        Some(card) => match card.parse::<usize>() {
            Ok(index) => text
                .lines()
                .find(|line| line.split_whitespace().next() == Some(&index.to_string()))
                .and_then(|line| parse_card_ids(line).pop())?,
            Err(_) => ids.into_iter().find(|id| id == card)?,
        },
        None => find_card(&ids)?,
    };
    let line = text
        .lines()
        .find(|line| parse_card_ids(line).first() == Some(&id))?;
    let driver = crate::cards::driver_of(line)?;
    Some((id, driver.to_string()))
}

/// The index of a card in `/proc/asound/cards` text: one named by index or
/// by id, or for `None` the one `find_card` takes. None if it is not there.
pub fn card_index(text: &str, card: Option<&str>) -> Option<u32> {
    let id = match card {
        Some(card) => card.to_string(),
        None => find_card(&parse_card_ids(text))?,
    };
    text.lines().find_map(|line| {
        let index = line.split_whitespace().next()?.parse::<u32>().ok()?;
        let named = parse_card_ids(line).first() == Some(&id) || index.to_string() == id;
        named.then_some(index)
    })
}

/// Refuses two outputs on the same card, which names compared as text let
/// through: one by index and one by id, or the master's card found and
/// named by an output. `cards` are the outputs' cards in order, the master
/// first, `None` for one to find. A card not there now cannot be told, and
/// passes.
pub fn distinct_cards(text: &str, cards: &[Option<String>]) -> Result<(), String> {
    let name = |card: &Option<String>| match card {
        Some(card) => format!("card {card}"),
        None => "the card found".to_string(),
    };
    let mut seen: Vec<(usize, u32)> = Vec::new();
    for (i, card) in cards.iter().enumerate() {
        let Some(index) = card_index(text, card.as_deref()) else {
            continue;
        };
        if let Some(&(j, _)) = seen.iter().find(|&&(_, other)| other == index) {
            return Err(format!(
                "output {} ({}) and output {} ({}) are the same card, {index}",
                j + 1,
                name(&cards[j]),
                i + 1,
                name(card)
            ));
        }
        seen.push((i, index));
    }
    Ok(())
}

/// The kernel's list of cards now, empty if it cannot be read.
pub fn pcm_text() -> String {
    fs::read_to_string(PCMS).unwrap_or_default()
}

/// Whether card `card` has a device that plays and one that captures, from
/// the kernel's list of PCM devices in `text`, lines such as
/// `02-00: HiFiBerry ADC HiFi … : capture 1`.
pub fn card_directions(text: &str, card: u32) -> (bool, bool) {
    let mut found = (false, false);
    for line in text.lines() {
        let Some((id, rest)) = line.split_once(':') else {
            continue;
        };
        let index = id
            .split('-')
            .next()
            .and_then(|c| c.trim().parse::<u32>().ok());
        if index != Some(card) {
            continue;
        }
        for part in rest.split(" : ") {
            let part = part.trim();
            found.0 |= part.starts_with("playback ");
            found.1 |= part.starts_with("capture ");
        }
    }
    found
}

/// Whether the master runs on its card's capture clock: as configured, or
/// under `auto` for a card that captures and does not play. `directions` is
/// `None` for a card that is not there, which is played to as before.
pub fn capture_clocked(clock: Clock, directions: Option<(bool, bool)>) -> bool {
    match clock {
        Clock::Capture => true,
        Clock::Playback => false,
        Clock::Auto => directions == Some((false, true)),
    }
}

pub fn cards_text() -> String {
    fs::read_to_string(CARDS).unwrap_or_default()
}

/// Where the analog input's card is decided, at the start.
#[derive(Debug, PartialEq)]
pub enum AnalogCard {
    /// The config does not enable the input.
    Off,
    /// The card to play to is there: decided now, by its driver.
    Present { card: String, driver: String },
    /// The card is not there yet. It is looked for, and its driver
    /// checked, at each open, so a card whose driver probes after the
    /// start is picked up.
    Absent,
}

/// The analog input's card, from the kernel's list of cards in `text`.
pub fn analog_card(text: &str, config: &Config) -> AnalogCard {
    if !config.analog.enabled {
        return AnalogCard::Off;
    }
    match card_driver(text, config.card.as_deref()) {
        Some((card, driver)) => AnalogCard::Present { card, driver },
        None => AnalogCard::Absent,
    }
}

/// The id of the card to capture from, for a card that was not there at
/// the start: the card to play to, if it is there now and has an ADC aas
/// knows. A card that is there without one never will have one.
pub fn capture_card(text: &str, card: Option<&str>) -> Result<String, OpenError> {
    let Some((id, driver)) = card_driver(text, card) else {
        return Err(OpenError::Again(match card {
            Some(card) => format!("card {card} not found"),
            None => "no sound card found".to_string(),
        }));
    };
    if !crate::cards::has_adc(&driver) {
        return Err(OpenError::Never(format!(
            "the card {driver} has no analog input aas knows"
        )));
    }
    Ok(id)
}

/// Whether a card named by id is among `ids`. A card named by index is
/// left for ALSA to find.
pub fn is_present(card: &str, ids: &[String]) -> bool {
    card.parse::<u32>().is_ok() || ids.iter().any(|id| id == card)
}

/// Whether the card to play to is among `ids`: the one the config names,
/// or else one that `find_card` takes. The same choice as at an open.
pub fn wanted_card_is_among(card: Option<&str>, ids: &[String]) -> bool {
    match card {
        Some(card) => is_present(card, ids),
        None => find_card(ids).is_some(),
    }
}

/// Whether the card to play to is present now. Reads the kernel's list of
/// cards, so it belongs on the control thread.
pub fn card_is_present(card: Option<&str>) -> bool {
    wanted_card_is_among(card, &card_ids())
}

/// Whether a card's error, `errno`, means its device is gone, rather than
/// an xrun that a restart cures. `disconnected` is the stream's state;
/// `recovering` says the error came from the restart after an xrun. Only
/// a device that has gone (ENODEV, or EBADFD on a disconnected stream), or
/// a card still stuck after a restart, is gone: an underrun or a suspend,
/// also one during the restart, stays an xrun, which is counted and
/// recovered in place.
pub fn card_is_gone(errno: i32, disconnected: bool, recovering: bool) -> bool {
    match errno {
        libc::ENODEV => true,
        libc::EBADFD => disconnected,
        libc::ETIMEDOUT => recovering,
        _ => false,
    }
}

/// The sample format the card is opened with, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardFormat {
    S32,
    /// 24 bits in the low three bytes of 32.
    S24,
    S16,
}

impl CardFormat {
    pub const PREFERRED: [CardFormat; 3] = [CardFormat::S32, CardFormat::S24, CardFormat::S16];

    /// Bytes per sample.
    pub fn bytes(self) -> usize {
        match self {
            CardFormat::S32 | CardFormat::S24 => 4,
            CardFormat::S16 => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CardFormat::S32 => "S32_LE",
            CardFormat::S24 => "S24_LE",
            CardFormat::S16 => "S16_LE",
        }
    }
}

/// Writes interleaved S32 samples into `out` as `format`, little endian,
/// keeping the top bits. `out` holds exactly the samples.
pub fn encode(format: CardFormat, samples: &[i32], out: &mut [u8]) {
    let samples = samples.iter();
    match format {
        CardFormat::S32 => {
            for (s, o) in samples.zip(out.chunks_exact_mut(4)) {
                o.copy_from_slice(&s.to_le_bytes());
            }
        }
        CardFormat::S24 => {
            for (s, o) in samples.zip(out.chunks_exact_mut(4)) {
                o.copy_from_slice(&(s >> 8).to_le_bytes());
            }
        }
        CardFormat::S16 => {
            for (s, o) in samples.zip(out.chunks_exact_mut(2)) {
                o.copy_from_slice(&((s >> 16) as i16).to_le_bytes());
            }
        }
    }
}

/// Reads frames captured as `format`, little endian, into S32 with the
/// samples in the top bits: the reverse of `encode`. `bytes` holds exactly
/// the frames in `out`.
pub fn decode(format: CardFormat, bytes: &[u8], out: &mut [[i32; 2]]) {
    let samples = out.iter_mut().flatten();
    match format {
        CardFormat::S32 => {
            for (s, b) in samples.zip(bytes.chunks_exact(4)) {
                *s = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
        }
        CardFormat::S24 => {
            // The top byte, sign or not, is shifted out.
            for (s, b) in samples.zip(bytes.chunks_exact(4)) {
                *s = i32::from_le_bytes([0, b[0], b[1], b[2]]);
            }
        }
        CardFormat::S16 => {
            for (s, b) in samples.zip(bytes.chunks_exact(2)) {
                *s = i32::from_le_bytes([0, 0, b[0], b[1]]);
            }
        }
    }
}

/// How long a capture delivers zeros after it starts. The ADC of the DAC+
/// ADC Pro starts with a transient that its high-pass filter takes this
/// long to bring below -60 dBFS: measured on the CM5 test board, it
/// stays above the detector's -50 dBFS for up to 650 ms at 44.1 kHz,
/// less at higher rates. Played through, it would open the analog input
/// for its hold time at every open and every change of rate. A restart
/// after an overrun, which prepares and starts the stream without closing
/// it, brings no transient (measured: nothing above -98 dBFS in 24
/// restarts), so it is not muted.
pub const SETTLE_MS: u32 = 1000;

/// Zeros the front of `out` while `left` frames are still to settle, and
/// counts them off.
pub fn mute_settling(out: &mut [[i32; 2]], left: &mut usize) {
    let n = out.len().min(*left);
    out[..n].fill([0; 2]);
    *left -= n;
}

/// The device of the card named in the config, or else the one found.
#[cfg(feature = "alsa-output")]
fn card_device(card: Option<&str>) -> Result<String, String> {
    let ids = card_ids();
    let card = match card {
        Some(card) if is_present(card, &ids) => card.to_string(),
        Some(card) => return Err(format!("card {card} not found")),
        None => find_card(&ids).ok_or("no sound card found")?,
    };
    Ok(format!("hw:{card}"))
}

#[cfg(feature = "alsa-output")]
pub use capture::{AlsaCapture, AlsaClock};
#[cfg(feature = "alsa-output")]
pub use card::AlsaSink;
#[cfg(feature = "alsa-output")]
pub use mixer::AlsaVolume;

#[cfg(feature = "alsa-output")]
mod mixer {
    use super::card_device;
    use crate::volume::{HardwareVolume, MUTE_MB, Watch, control_db, find_control};
    use alsa::Round;
    use alsa::mixer::{MilliBel, Mixer, Selem, SelemChannelId, SelemId};

    /// A volume control of the card, set in dB. Used from the control
    /// thread only.
    pub struct AlsaVolume {
        mixer: Mixer,
        id: SelemId,
        range: (f32, f32),
        switch: bool,
        /// The lowest step of the control is a mute.
        floor_mutes: bool,
        /// The control carries the volume; false when aas follows it but
        /// applies the volume itself.
        carries: bool,
        /// Tells a level another program set from one aas set.
        watch: Watch,
        /// The last read failed and was logged, so the next failures are
        /// not, until a read succeeds again.
        read_failed: bool,
    }

    /// The control's dB range in millibel, if it has a playback volume
    /// with one.
    fn db_range(selem: &Selem) -> Option<(i64, i64)> {
        if !selem.has_playback_volume() {
            return None;
        }
        let (min, max) = selem.get_playback_db_range();
        (min < max).then_some((min.0, max.0))
    }

    /// The control's level in dB from its first channel, with its switch
    /// if it has one.
    fn read_level(selem: &Selem, switch: bool) -> alsa::Result<f32> {
        let millibel = selem.get_playback_vol_db(SelemChannelId::FrontLeft)?;
        let on = !switch || selem.get_playback_switch(SelemChannelId::FrontLeft)? != 0;
        Ok(control_db(millibel.0, on))
    }

    impl AlsaVolume {
        /// The card's volume control: `configured` if the card has it, else
        /// the first of `Digital`, `Master` and `PCM`. None if the card is
        /// missing or has no such control with a range in dB. `carries` is
        /// false for a control aas follows and writes but whose volume aas
        /// applies itself.
        pub fn open(
            card: Option<&str>,
            configured: Option<&str>,
            carries: bool,
        ) -> Option<AlsaVolume> {
            let device = match card_device(card) {
                Ok(device) => device,
                Err(e) => {
                    eprintln!("aas: no hardware volume: {e}");
                    return None;
                }
            };
            let mixer = match Mixer::new(&device, false) {
                Ok(mixer) => mixer,
                Err(e) => {
                    eprintln!("aas: no hardware volume: {device}: {e}");
                    return None;
                }
            };
            let names: Vec<String> = mixer
                .iter()
                .filter_map(Selem::new)
                .filter(|selem| db_range(selem).is_some())
                .filter_map(|selem| {
                    let id = selem.get_id();
                    let name = id.get_name().ok()?;
                    (id.get_index() == 0).then(|| name.to_string())
                })
                .collect();
            let Some(name) = find_control(&names, configured) else {
                match configured {
                    Some(c) => eprintln!("aas: {device} has no volume control {c}"),
                    None => eprintln!("aas: {device} has no hardware volume"),
                }
                return None;
            };
            let id = SelemId::new(&name, 0);
            let selem = mixer.find_selem(&id)?;
            let (min, max) = db_range(&selem)?;
            let switch = selem.has_playback_switch();
            let floor_mutes = min <= MUTE_MB;
            let range = (min as f32 / 100.0, max as f32 / 100.0);
            let floor = if floor_mutes {
                "mute".to_string()
            } else {
                format!("{} dB", range.0)
            };
            eprintln!(
                "aas: hardware volume {device} {name}, {floor} to {} dB{}{}",
                range.1,
                if switch { ", with a switch" } else { "" },
                if carries {
                    ""
                } else {
                    ", followed and applied in software"
                }
            );
            Some(AlsaVolume {
                mixer,
                id,
                range,
                switch,
                floor_mutes,
                carries,
                watch: Watch::default(),
                read_failed: false,
            })
        }

        /// The control's level now, `f32::NEG_INFINITY` when muted. None if
        /// the control has gone or cannot be read; that is logged once,
        /// not at every poll, until a read succeeds again.
        fn level(&mut self) -> Option<f32> {
            // Takes in changes others made, so the element is current.
            let _ = self.mixer.handle_events();
            let read = match self.mixer.find_selem(&self.id) {
                None => Err("the hardware volume control has gone".to_string()),
                Some(selem) => read_level(&selem, self.switch)
                    .map_err(|e| format!("could not read the hardware volume: {e}")),
            };
            match read {
                Ok(db) => {
                    self.read_failed = false;
                    Some(db)
                }
                Err(e) => {
                    if !self.read_failed {
                        eprintln!("aas: {e}");
                        self.read_failed = true;
                    }
                    None
                }
            }
        }
    }

    impl HardwareVolume for AlsaVolume {
        fn set_db(&mut self, db: f32) {
            // Takes in changes others made, so the element is current.
            let _ = self.mixer.handle_events();
            let Some(selem) = self.mixer.find_selem(&self.id) else {
                eprintln!("aas: the hardware volume control has gone");
                return;
            };
            let muted = db == f32::NEG_INFINITY;
            let db = if muted { self.range.0 } else { db };
            let value = MilliBel((db * 100.0).round() as i64);
            if let Err(e) = selem.set_playback_db_all(value, Round::Floor) {
                eprintln!("aas: could not set the hardware volume: {e}");
            }
            if self.switch {
                if let Err(e) = selem.set_playback_switch_all(i32::from(!muted)) {
                    eprintln!("aas: could not set the hardware mute: {e}");
                }
            }
            // What the control took, so it is not taken back in as a
            // change another program made. A change by another program
            // landing between the write and this read is taken as aas's own
            // and not followed; the window is one mixer call.
            if let Some(level) = self.level() {
                self.watch.known(level);
            }
        }

        fn range_db(&self) -> (f32, f32) {
            self.range
        }

        fn can_mute(&self) -> bool {
            self.switch || self.floor_mutes
        }

        fn read_db(&mut self) -> Option<f32> {
            let level = self.level();
            match level {
                Some(db) => self.watch.known(db),
                None => eprintln!("aas: the hardware volume could not be read"),
            }
            level
        }

        fn changed(&mut self) -> Option<f32> {
            self.level().and_then(|db| self.watch.seen(db))
        }

        fn carries_volume(&self) -> bool {
            self.carries
        }
    }
}

#[cfg(feature = "alsa-output")]
mod card {
    use super::{CardFormat, card_device, card_is_gone, encode};
    use crate::counters::monotonic_ns;
    use crate::latency::PERIODS;
    use crate::sink::{Sink, SinkError};
    use alsa::pcm::{Access, Format, Frames, HwParams, PCM, State, TstampType};
    use alsa::{Direction, ValueOr};

    /// How long a write waits for room before the card counts as stuck.
    const WAIT_MS: u32 = 500;

    /// Rates offered to the rate rules, if the card takes them.
    pub(super) const RATES: [u32; 14] = [
        8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000,
        352800, 384000,
    ];

    pub(super) fn alsa_format(format: CardFormat) -> Format {
        match format {
            CardFormat::S32 => Format::S32LE,
            CardFormat::S24 => Format::S24LE,
            CardFormat::S16 => Format::S16LE,
        }
    }

    /// Opens without waiting: a card that another program holds gives
    /// EBUSY at once, which counts as gone, so the null sink plays on and
    /// the open is retried. A blocking open would wait for the card to be
    /// free, holding up the start or the audio thread.
    fn open_pcm(device: &str) -> alsa::Result<PCM> {
        PCM::new(device, Direction::Playback, true)
    }

    /// The best format the card takes with `channels` channels, and its
    /// rates in that format.
    fn choose(hw: &HwParams, channels: usize) -> alsa::Result<(CardFormat, Vec<u32>)> {
        hw.set_channels(channels as u32)?;
        hw.set_access(Access::RWInterleaved)?;
        let format = CardFormat::PREFERRED
            .into_iter()
            .find(|&f| hw.test_format(alsa_format(f)).is_ok())
            .ok_or_else(|| alsa::Error::unsupported("snd_pcm_hw_params_test_format"))?;
        hw.set_format(alsa_format(format))?;
        let rates = RATES
            .into_iter()
            .filter(|&r| hw.test_rate(r).is_ok())
            .collect();
        Ok((format, rates))
    }

    struct Open {
        pcm: PCM,
        format: CardFormat,
        channels: usize,
        rate: u32,
        /// Frames in the card's buffer.
        buffer: u32,
        /// Frames in the card's period.
        period: usize,
        /// Room for one engine period in the card's format.
        bytes: Vec<u8>,
        /// The card's whole buffer of silence, to restart it after an xrun.
        zeros: Vec<u8>,
    }

    impl Open {
        fn frame_bytes(&self) -> usize {
            self.format.bytes() * self.channels
        }

        /// Writes all of `bytes`. The PCM is non-blocking, so this waits in
        /// `snd_pcm_wait` until there is room for the next period, or what
        /// is left of it, and writes it whole, as a blocking write would.
        /// Writing part of a period and then waiting for avail_min would
        /// wait up to a period too long. A card that gives no room in
        /// `WAIT_MS` fails.
        fn write_all(&self, bytes: &[u8]) -> alsa::Result<()> {
            let io = self.pcm.io_bytes();
            let mut done = 0;
            while done < bytes.len() {
                let want = ((bytes.len() - done) / self.frame_bytes()).min(self.period);
                let room = self.pcm.avail_update()?;
                let result = if room < want as Frames {
                    Err(alsa::Error::new("snd_pcm_avail_update", libc::EAGAIN))
                } else {
                    io.writei(&bytes[done..])
                };
                match result {
                    Ok(frames) => done += frames * self.frame_bytes(),
                    Err(e) if e.errno() == libc::EINTR => {}
                    Err(e) if e.errno() == libc::EAGAIN => self.wait()?,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        /// Waits for the card to have room, up to `WAIT_MS`. A signal that
        /// cuts the wait short is not a failure: the caller looks again.
        fn wait(&self) -> alsa::Result<()> {
            match self.pcm.wait(Some(WAIT_MS)) {
                Ok(true) => Ok(()),
                Ok(false) => Err(alsa::Error::new("snd_pcm_wait", libc::ETIMEDOUT)),
                Err(e) if e.errno() == libc::EINTR => Ok(()),
                Err(e) => Err(e),
            }
        }

        /// Waits until the card has room for one engine period, or for the
        /// whole buffer if that is smaller.
        fn wait_room(&self) -> alsa::Result<()> {
            let period = (self.bytes.len() / self.frame_bytes()).min(self.buffer as usize);
            while (self.pcm.avail_update()? as usize) < period {
                self.wait()?;
            }
            Ok(())
        }

        /// Converts and writes the frames, a buffer's worth at a time.
        fn play(&mut self, samples: &[i32]) -> alsa::Result<()> {
            if self.format == CardFormat::S32 && cfg!(target_endian = "little") {
                // The samples are the card's bytes already.
                // SAFETY: a sample is 4 bytes with no padding, and bytes
                // have no alignment to keep.
                let bytes = unsafe {
                    std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), size_of_val(samples))
                };
                return self.write_all(bytes);
            }
            let per_chunk = (self.bytes.len() / self.frame_bytes()).max(1) * self.channels;
            for chunk in samples.chunks(per_chunk) {
                let len = chunk.len() * self.format.bytes();
                encode(self.format, chunk, &mut self.bytes[..len]);
                self.write_all(&self.bytes[..len])?;
            }
            Ok(())
        }

        /// The frames queued in the card now. The status gives the delay
        /// as of its timestamp, the time of the card's last position
        /// update; while the card runs, the frames played since then are
        /// taken off, so the delay is read at the moment of the call and
        /// not at the card's last interrupt. A timestamp that is not on
        /// CLOCK_MONOTONIC, or older than a period, is not used.
        fn delay_now(&self) -> alsa::Result<u32> {
            let status = self.pcm.status()?;
            let delay = status.get_delay().max(0) as f64;
            let stamp = status.get_htstamp();
            // 32 bits on 32-bit ARM, 64 on 64-bit.
            #[allow(clippy::unnecessary_cast)]
            let stamp_ns = stamp.tv_sec as i64 * 1_000_000_000 + stamp.tv_nsec as i64;
            let since_ns = monotonic_ns() as i64 - stamp_ns;
            let period_ns = self.period as i64 * 1_000_000_000 / i64::from(self.rate);
            let delay = if status.get_state() == State::Running
                && stamp_ns > 0
                && (0..=period_ns).contains(&since_ns)
            {
                delay - since_ns as f64 * f64::from(self.rate) / 1e9
            } else {
                delay
            };
            Ok(delay.max(0.0) as u32)
        }

        /// Restarts the card after an xrun: prepare, then two periods of
        /// zeros, which fill the buffer and so start it.
        fn recover(&self) -> alsa::Result<()> {
            self.pcm.prepare()?;
            self.write_all(&self.zeros)
        }

        /// Whether the stream's device has been disconnected.
        fn disconnected(&self) -> bool {
            self.pcm.state() == State::Disconnected
        }

        /// Waits a period, after a restart that failed, so that a card that
        /// fails at every period cannot spin the thread that plays to it.
        fn rest(&self) {
            let ns = self.period as u64 * 1_000_000_000 / u64::from(self.rate);
            std::thread::sleep(std::time::Duration::from_nanos(ns));
        }
    }

    /// Why an open failed. A refusal is the same at every try until the
    /// configuration changes, so the fallback does not retry it by the clock.
    enum OpenFailure {
        /// The card is busy, gone, or failed in some way that may pass.
        Failed(String),
        /// The card is there and does not take this configuration.
        Refused(String),
    }

    impl OpenFailure {
        fn message(self) -> String {
            match self {
                OpenFailure::Failed(m) | OpenFailure::Refused(m) => m,
            }
        }
    }

    impl From<String> for OpenFailure {
        fn from(message: String) -> OpenFailure {
            OpenFailure::Failed(message)
        }
    }

    /// Plays to `hw:<card>` with a fixed number of channels, as S32 if the
    /// card takes it, else S24 in 32 bits, else S16. Two periods are
    /// buffered, or as many as it is made with, and the card starts once
    /// all are written.
    pub struct AlsaSink {
        /// The card the config names. None finds one at each open.
        card: Option<String>,
        channels: usize,
        periods: u32,
        open: Option<Open>,
        rates: Vec<u32>,
        /// The last failure logged, so that a retry every second does not
        /// repeat it.
        last_failure: Option<String>,
    }

    impl AlsaSink {
        /// Looks at the card once for its rates. A card that is missing now
        /// is looked for again at each open.
        pub fn new(card: Option<String>, channels: usize) -> AlsaSink {
            AlsaSink::buffering(card, channels, PERIODS)
        }

        /// The same with `periods` periods buffered, as a further output's
        /// card is.
        pub fn buffering(card: Option<String>, channels: usize, periods: u32) -> AlsaSink {
            let mut sink = AlsaSink {
                card,
                channels,
                periods,
                open: None,
                rates: Vec::new(),
                last_failure: None,
            };
            match sink.probe() {
                Ok(rates) => sink.rates = rates,
                Err(e) => sink.failed(e),
            }
            sink
        }

        fn probe(&self) -> Result<Vec<u32>, String> {
            let device = self.device()?;
            let e = |err: alsa::Error| format!("{device}: {err}");
            let pcm = open_pcm(&device).map_err(e)?;
            let hw = HwParams::any(&pcm).map_err(e)?;
            let (format, rates) = self.choose(&device, &hw).map_err(OpenFailure::message)?;
            eprintln!(
                "aas: card {device} takes {} channels, {}, {rates:?}",
                self.channels,
                format.name()
            );
            Ok(rates)
        }

        /// `choose` for this sink's channel count, with the failure for a
        /// card that does not take it.
        fn choose(
            &self,
            device: &str,
            hw: &HwParams,
        ) -> Result<(CardFormat, Vec<u32>), OpenFailure> {
            if hw.test_channels(self.channels as u32).is_err()
                || hw.set_channels(self.channels as u32).is_err()
            {
                return Err(self.refusal(device));
            }
            choose(hw, self.channels)
                .map_err(|err| OpenFailure::Failed(format!("{device}: format: {err}")))
        }

        /// The card is there and does not take the channel count.
        fn refusal(&self, device: &str) -> OpenFailure {
            OpenFailure::Refused(format!(
                "{device}: does not take {} channels",
                self.channels
            ))
        }

        /// The device to open. A named card is looked for first, so that a
        /// missing one does not make libasound log on every retry.
        fn device(&self) -> Result<String, String> {
            card_device(self.card.as_deref())
        }

        fn failed(&mut self, failure: String) {
            if self.last_failure.as_ref() != Some(&failure) {
                eprintln!("aas: {failure}");
                self.last_failure = Some(failure);
            }
        }

        fn try_open(&mut self, rate: u32, period: u32) -> Result<Open, OpenFailure> {
            let device = self.device()?;
            let e = |what: &str, err: alsa::Error| format!("{device}: {what}: {err}");
            let pcm = open_pcm(&device).map_err(|x| e("open", x))?;
            let (format, buffer, card_period) = {
                let hw = HwParams::any(&pcm).map_err(|x| e("hw params", x))?;
                let (format, rates) = self.choose(&device, &hw)?;
                self.rates = rates;
                if !self.rates.contains(&rate) {
                    return Err(format!("{device}: does not take {rate} Hz").into());
                }
                hw.set_rate(rate, ValueOr::Nearest)
                    .map_err(|x| e("rate", x))?;
                hw.set_period_size_near(Frames::from(period), ValueOr::Nearest)
                    .map_err(|x| e("period", x))?;
                hw.set_periods(self.periods, ValueOr::Nearest)
                    .map_err(|x| e("periods", x))?;
                if let Err(x) = pcm.hw_params(&hw) {
                    // Some I2S controllers take a channel count in the
                    // probe and refuse it here. Up to two channels every
                    // card takes it, so an EINVAL there is about something
                    // else, which may pass, and is retried.
                    if x.errno() == libc::EINVAL && self.channels > 2 {
                        return Err(self.refusal(&device));
                    }
                    return Err(e("apply", x).into());
                }
                let got_rate = hw.get_rate().map_err(|x| e("get rate", x))?;
                if got_rate != rate {
                    return Err(format!("{device}: asked for {rate} Hz, got {got_rate}").into());
                }
                let got_period = hw.get_period_size().map_err(|x| e("get period", x))?;
                let got_buffer = hw.get_buffer_size().map_err(|x| e("get buffer", x))?;
                (format, got_buffer, got_period)
            };
            {
                let sw = pcm.sw_params_current().map_err(|x| e("sw params", x))?;
                sw.set_start_threshold(buffer)
                    .map_err(|x| e("start threshold", x))?;
                // The engine waits for room for its own period, so ALSA
                // wakes it then, not at each of the card's periods. A card
                // that granted a smaller period would otherwise make
                // `wait_room` return at once until the difference had played.
                sw.set_avail_min(Frames::from(period).min(buffer))
                    .map_err(|x| e("avail min", x))?;
                // Timestamps on CLOCK_MONOTONIC for `delay_now`. A card
                // without them still plays, its delay read as of its last
                // position update.
                if let Err(x) = sw
                    .set_tstamp_mode(true)
                    .and_then(|()| sw.set_tstamp_type(TstampType::Monotonic))
                {
                    eprintln!("aas: {device}: no monotonic timestamps: {x}");
                }
                pcm.sw_params(&sw).map_err(|x| e("apply sw", x))?;
            }
            if card_period != Frames::from(period) {
                eprintln!(
                    "aas: warning: {device} gave a period of {card_period} frames, not {period}"
                );
            }
            eprintln!(
                "aas: card {device} open: {}, {rate} Hz, {card_period} frames, \
                 {buffer} frames buffered",
                format.name(),
            );
            let frame_bytes = format.bytes() * self.channels;
            Ok(Open {
                pcm,
                format,
                channels: self.channels,
                rate,
                buffer: buffer as u32,
                period: card_period as usize,
                bytes: vec![0; period as usize * frame_bytes],
                zeros: vec![0; buffer as usize * frame_bytes],
            })
        }
    }

    impl AlsaSink {
        /// What a failed wait or write comes to: a device that is gone, as
        /// `card_is_gone` says, is let go of, so that its module can be
        /// unloaded; anything else is an xrun, recovered here in place. A
        /// restart that fails, other than for a device gone, is an xrun too,
        /// and waits a period. alsa::Error holds the positive errno.
        fn io_failed(&mut self, error: alsa::Error) -> SinkError {
            let Some(open) = &self.open else {
                return SinkError::Gone;
            };
            let result = if card_is_gone(error.errno(), open.disconnected(), false) {
                SinkError::Gone
            } else {
                match open.recover() {
                    Ok(()) => SinkError::Xrun,
                    Err(e) if card_is_gone(e.errno(), open.disconnected(), true) => SinkError::Gone,
                    Err(_) => {
                        open.rest();
                        SinkError::Xrun
                    }
                }
            };
            if result == SinkError::Gone {
                self.open = None;
            }
            result
        }
    }

    impl Sink for AlsaSink {
        fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
            // The card takes one opener at a time.
            self.open = None;
            match self.try_open(rate, period) {
                Ok(open) => {
                    self.open = Some(open);
                    self.last_failure = None;
                    Ok(())
                }
                Err(OpenFailure::Failed(e)) => {
                    self.failed(e);
                    Err(SinkError::Gone)
                }
                Err(OpenFailure::Refused(e)) => {
                    self.failed(e);
                    Err(SinkError::Refused)
                }
            }
        }

        fn wait_room(&mut self) -> Result<(), SinkError> {
            let Some(open) = &mut self.open else {
                return Err(SinkError::Gone);
            };
            match open.wait_room() {
                Ok(()) => Ok(()),
                // The restart fills the buffer; the write that follows
                // waits for room itself.
                Err(e) => Err(self.io_failed(e)),
            }
        }

        fn write(&mut self, samples: &[i32]) -> Result<(), SinkError> {
            let Some(open) = &mut self.open else {
                return Err(SinkError::Gone);
            };
            match open.play(samples) {
                Ok(()) => Ok(()),
                Err(e) => Err(self.io_failed(e)),
            }
        }

        fn channels(&self) -> usize {
            self.channels
        }

        fn close(&mut self) {
            self.open = None;
        }

        fn delay_frames(&self) -> u32 {
            self.open.as_ref().map_or(0, |open| open.buffer)
        }

        fn delay_now(&mut self) -> Result<u32, SinkError> {
            let Some(open) = &self.open else {
                return Err(SinkError::Gone);
            };
            match open.delay_now() {
                Ok(frames) => Ok(frames),
                Err(e) if e.errno() == libc::ENODEV => {
                    // Lets the driver go, as a failed write does.
                    self.open = None;
                    Err(SinkError::Gone)
                }
                // The next write finds what is wrong and recovers.
                Err(_) => Err(SinkError::Xrun),
            }
        }

        fn supported_rates(&self) -> Vec<u32> {
            self.rates.clone()
        }

        fn granted(&self) -> Option<(u32, u32)> {
            let open = self.open.as_ref()?;
            Some((open.period as u32, open.buffer))
        }
    }
}

#[cfg(feature = "alsa-output")]
mod capture {
    use super::card::{RATES, alsa_format};
    use super::{CardFormat, SETTLE_MS, decode, mute_settling};
    use crate::capture::Capture;
    use crate::clock_sink::ClockDevice;
    use crate::sink::SinkError;
    use alsa::pcm::{Access, Frames, HwParams, PCM, State};
    use alsa::{Direction, ValueOr};

    const CHANNELS: usize = 2;
    /// Engine periods in the capture buffer. The engine reads one a cycle
    /// and keeps at most two, so four leave room for a late cycle.
    const PERIODS: u32 = 4;
    /// Fewer engine periods than this in the buffer are worth a warning.
    const FEWEST_PERIODS: u32 = 3;

    /// The card's ADC, read without waiting. Opened on the control thread
    /// and read on the audio thread.
    pub struct AlsaCapture {
        /// None once closed. The device is closed then, not kept until the
        /// capture is dropped: a capture PCM of these cards takes one
        /// opener, and the control thread opens the next one while the
        /// engine still holds this one.
        pcm: Option<PCM>,
        format: CardFormat,
        /// Frames in `bytes`, the most one read takes.
        chunk: usize,
        /// Room for one period in the card's format, made at the open so
        /// that reads do not allocate.
        bytes: Vec<u8>,
        /// Frames still to be muted while the ADC settles.
        settling: usize,
    }

    impl AlsaCapture {
        /// Opens `hw:<card>` for capture at `rate`, as S32 if the card
        /// takes it, else S24 in 32 bits, else S16, with periods of
        /// `period` frames and a buffer of four of them, and starts it. The
        /// first `SETTLE_MS` are read as zeros. The open does not wait: a
        /// device another program holds fails at once. A card that does
        /// not take `rate` fails too, since the ADC is never resampled.
        pub fn open(card: &str, rate: u32, period: u32) -> Result<AlsaCapture, String> {
            let device = format!("hw:{card}");
            let e = |what: &str, err: alsa::Error| format!("{device} capture: {what}: {err}");
            let pcm = PCM::new(&device, Direction::Capture, true).map_err(|x| e("open", x))?;
            let (format, card_period, buffer) = {
                let hw = HwParams::any(&pcm).map_err(|x| e("hw params", x))?;
                hw.set_channels(CHANNELS as u32)
                    .map_err(|x| e("channels", x))?;
                hw.set_access(Access::RWInterleaved)
                    .map_err(|x| e("access", x))?;
                let format = CardFormat::PREFERRED
                    .into_iter()
                    .find(|&f| hw.test_format(alsa_format(f)).is_ok())
                    .ok_or_else(|| format!("{device} capture: no format aas reads"))?;
                hw.set_format(alsa_format(format))
                    .map_err(|x| e("format", x))?;
                if hw.test_rate(rate).is_err() {
                    return Err(format!("{device} capture: does not take {rate} Hz"));
                }
                hw.set_rate(rate, ValueOr::Nearest)
                    .map_err(|x| e("rate", x))?;
                hw.set_period_size_near(Frames::from(period), ValueOr::Nearest)
                    .map_err(|x| e("period", x))?;
                // In engine periods, so that a card that grants a smaller
                // period still has room for what the engine keeps.
                hw.set_buffer_size_near(Frames::from(period * PERIODS))
                    .map_err(|x| e("buffer", x))?;
                pcm.hw_params(&hw).map_err(|x| e("apply", x))?;
                let got_rate = hw.get_rate().map_err(|x| e("get rate", x))?;
                if got_rate != rate {
                    return Err(format!(
                        "{device} capture: asked for {rate} Hz, got {got_rate}"
                    ));
                }
                let got_period = hw.get_period_size().map_err(|x| e("get period", x))?;
                let got_buffer = hw.get_buffer_size().map_err(|x| e("get buffer", x))?;
                (format, got_period, got_buffer)
            };
            if card_period != Frames::from(period) {
                eprintln!(
                    "aas: warning: {device} capture gave a period of {card_period} frames, \
                     not {period}"
                );
            }
            if buffer < Frames::from(period * FEWEST_PERIODS) {
                eprintln!(
                    "aas: warning: {device} capture gave a buffer of {buffer} frames, \
                     less than {FEWEST_PERIODS} periods of {period}"
                );
            }
            pcm.start().map_err(|x| e("start", x))?;
            eprintln!(
                "aas: capture {device} open: {}, {rate} Hz, {card_period} frames, \
                 {buffer} frames buffered",
                format.name(),
            );
            let chunk = period.max(1) as usize;
            Ok(AlsaCapture {
                pcm: Some(pcm),
                format,
                chunk,
                bytes: vec![0; chunk * format.bytes() * CHANNELS],
                settling: (u64::from(rate) * u64::from(SETTLE_MS) / 1000) as usize,
            })
        }

        fn frame_bytes(&self) -> usize {
            self.format.bytes() * CHANNELS
        }

        /// What an error leaves. Nothing queued is not an error. An overrun
        /// restarts the stream at once, and the frames lost are what the
        /// engine pads. Anything else, a card gone or suspended, fails the
        /// capture, and the control thread opens it again.
        fn recover(pcm: &PCM, err: alsa::Error) -> Result<(), SinkError> {
            // alsa::Error holds the positive errno.
            match err.errno() {
                libc::EAGAIN | libc::EINTR => Ok(()),
                libc::EPIPE => pcm
                    .prepare()
                    .and_then(|()| pcm.start())
                    .map_err(|_| SinkError::Gone),
                _ => Err(SinkError::Gone),
            }
        }

        /// Frames queued, 0 after an overrun it has recovered from. With
        /// none queued, a stream that is not running, such as one
        /// suspended or disconnected, for which `avail_update` gives 0
        /// rather than an error, fails here instead of being read as empty
        /// for ever. The state comes from the status ALSA maps, without a
        /// system call where the driver maps it.
        fn queued(pcm: &PCM) -> Result<usize, SinkError> {
            match pcm.avail_update() {
                Ok(n) if n > 0 => Ok(n as usize),
                Ok(_) => match pcm.state() {
                    State::Running => Ok(0),
                    State::XRun => {
                        Self::recover(pcm, alsa::Error::new("state", libc::EPIPE)).map(|()| 0)
                    }
                    _ => Err(SinkError::Gone),
                },
                Err(err) => Self::recover(pcm, err).map(|()| 0),
            }
        }

        /// Reads what is queued, up to `frames` and at most `chunk`, into
        /// `bytes`, and returns how many frames. Asks for no more than is
        /// queued, so the read neither waits nor fails for want of frames.
        fn read_bytes(&mut self, frames: usize) -> Result<usize, SinkError> {
            let frame_bytes = self.frame_bytes();
            let pcm = self.pcm.as_ref().ok_or(SinkError::Gone)?;
            let want = frames.min(self.chunk).min(Self::queued(pcm)?);
            if want == 0 {
                return Ok(0);
            }
            let len = want * frame_bytes;
            match pcm.io_bytes().readi(&mut self.bytes[..len]) {
                Ok(n) => Ok(n.min(want)),
                Err(err) => Self::recover(pcm, err).map(|()| 0),
            }
        }
    }

    impl Capture for AlsaCapture {
        fn available(&mut self) -> Result<usize, SinkError> {
            Self::queued(self.pcm.as_ref().ok_or(SinkError::Gone)?)
        }

        fn read(&mut self, out: &mut [[i32; 2]]) -> Result<usize, SinkError> {
            let mut done = 0;
            while done < out.len() {
                let n = self.read_bytes(out.len() - done)?;
                if n == 0 {
                    break;
                }
                let len = n * self.frame_bytes();
                let read = &mut out[done..done + n];
                decode(self.format, &self.bytes[..len], read);
                mute_settling(read, &mut self.settling);
                done += n;
            }
            Ok(done)
        }

        fn skip(&mut self, frames: usize) -> Result<(), SinkError> {
            let mut done = 0;
            while done < frames {
                let n = self.read_bytes(frames - done)?;
                if n == 0 {
                    break;
                }
                self.settling = self.settling.saturating_sub(n);
                done += n;
            }
            Ok(())
        }

        /// Stops the stream and closes the device, so that the next
        /// capture can open it, at the output's new rate, before this one
        /// comes back to the control thread. Closing frees the handle on
        /// the audio thread, as closing the output does. It happens there
        /// only before the output reopens; a capture that failed comes back
        /// to the control thread and is closed there.
        fn close(&mut self) {
            if let Some(pcm) = self.pcm.take() {
                let _ = pcm.drop();
            }
        }
    }

    /// How long a wait for the capture lasts before the card counts as
    /// stuck, as for a playback card.
    const WAIT_MS: u32 = 500;

    /// The master's device under the capture clock: an `AlsaCapture` the
    /// audio thread waits on. Its rates are read once, when it is made.
    pub struct AlsaClock {
        card: String,
        rates: Vec<u32>,
        capture: Option<AlsaCapture>,
        /// The last open failure, logged once until it changes.
        failed: Option<String>,
    }

    impl AlsaClock {
        pub fn new(card: String) -> AlsaClock {
            let rates = capture_rates(&card).unwrap_or_else(|e| {
                eprintln!("aas: {e}; its rates are read when it opens");
                Vec::new()
            });
            if !rates.is_empty() {
                eprintln!("aas: capture hw:{card} takes {rates:?}");
            }
            AlsaClock {
                card,
                rates,
                capture: None,
                failed: None,
            }
        }

        fn pcm(&self) -> Result<&PCM, SinkError> {
            let capture = self.capture.as_ref().ok_or(SinkError::Gone)?;
            capture.pcm.as_ref().ok_or(SinkError::Gone)
        }
    }

    /// The rates the card captures at, in the format the analog input
    /// reads, stereo.
    fn capture_rates(card: &str) -> Result<Vec<u32>, String> {
        let device = format!("hw:{card}");
        let e = |what: &str, err: alsa::Error| format!("{device} capture: {what}: {err}");
        let pcm = PCM::new(&device, Direction::Capture, true).map_err(|x| e("open", x))?;
        let hw = HwParams::any(&pcm).map_err(|x| e("hw params", x))?;
        hw.set_channels(CHANNELS as u32)
            .map_err(|x| e("channels", x))?;
        hw.set_access(Access::RWInterleaved)
            .map_err(|x| e("access", x))?;
        let format = CardFormat::PREFERRED
            .into_iter()
            .find(|&f| hw.test_format(alsa_format(f)).is_ok())
            .ok_or_else(|| format!("{device} capture: no format aas reads"))?;
        hw.set_format(alsa_format(format))
            .map_err(|x| e("format", x))?;
        Ok(RATES
            .into_iter()
            .filter(|&r| hw.test_rate(r).is_ok())
            .collect())
    }

    impl Capture for AlsaClock {
        fn available(&mut self) -> Result<usize, SinkError> {
            self.capture.as_mut().ok_or(SinkError::Gone)?.available()
        }

        fn read(&mut self, out: &mut [[i32; 2]]) -> Result<usize, SinkError> {
            self.capture.as_mut().ok_or(SinkError::Gone)?.read(out)
        }

        fn skip(&mut self, frames: usize) -> Result<(), SinkError> {
            self.capture.as_mut().ok_or(SinkError::Gone)?.skip(frames)
        }

        fn close(&mut self) {
            if let Some(mut capture) = self.capture.take() {
                capture.close();
            }
        }
    }

    impl ClockDevice for AlsaClock {
        fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
            self.close();
            // Rates not read at the start, the card absent or busy then,
            // are read before the open holds the device; the fallback
            // passes them on to the rate rules.
            if self.rates.is_empty()
                && let Ok(rates) = capture_rates(&self.card)
            {
                eprintln!("aas: capture hw:{} takes {rates:?}", self.card);
                self.rates = rates;
            }
            match AlsaCapture::open(&self.card, rate, period) {
                Ok(capture) => {
                    // The wait wakes when an engine period is there, also
                    // on a card that granted a smaller one, rather than
                    // spinning on each of the card's.
                    if let Some(pcm) = capture.pcm.as_ref()
                        && let Ok(sw) = pcm.sw_params_current()
                    {
                        let _ = sw.set_avail_min(Frames::from(period));
                        let _ = pcm.sw_params(&sw);
                    }
                    self.capture = Some(capture);
                    self.failed = None;
                    Ok(())
                }
                Err(why) => {
                    if self.failed.as_ref() != Some(&why) {
                        eprintln!("aas: {why}");
                        self.failed = Some(why);
                    }
                    Err(SinkError::Gone)
                }
            }
        }

        /// Waits until `frames` are queued. An overrun found here is
        /// recovered from and told as `Xrun`, so the engine counts it; the
        /// read in `queued` would have recovered from it without a word.
        fn wait(&mut self, frames: usize) -> Result<(), SinkError> {
            let pcm = self.pcm()?;
            loop {
                if pcm.state() == State::XRun {
                    AlsaCapture::recover(pcm, alsa::Error::new("state", libc::EPIPE))?;
                    return Err(SinkError::Xrun);
                }
                if AlsaCapture::queued(pcm)? >= frames {
                    return Ok(());
                }
                match pcm.wait(Some(WAIT_MS)) {
                    Ok(true) => {}
                    Ok(false) => return Err(SinkError::Gone),
                    Err(e) if e.errno() == libc::EINTR => {}
                    Err(e) => {
                        let overrun = e.errno() == libc::EPIPE;
                        AlsaCapture::recover(pcm, e)?;
                        if overrun {
                            return Err(SinkError::Xrun);
                        }
                    }
                }
            }
        }

        fn rates(&self) -> Vec<u32> {
            self.rates.clone()
        }

        fn is_open(&self) -> bool {
            self.capture.as_ref().is_some_and(|c| c.pcm.is_some())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// `/proc/asound/pcm` of a CM5 with a HiFiBerry ADC: it captures only.
    const ADC_ONLY_PCM: &str = "00-00: MAI PCM i2s-hifi-0 : MAI PCM i2s-hifi-0 : playback 1\n\
        01-00: MAI PCM i2s-hifi-0 : MAI PCM i2s-hifi-0 : playback 1\n\
        02-00: HiFiBerry ADC HiFi pcm1863-aif-0 : HiFiBerry ADC HiFi pcm1863-aif-0 : capture 1\n";

    /// A DAC+ ADC: one device that plays and captures.
    const DACADC_PCM: &str = "00-00: HiFiBerry DAC+ADC HiFi multicodec-0 : \
        HiFiBerry DAC+ADC HiFi multicodec-0 : playback 1 : capture 1\n";

    #[test]
    fn card_directions_reads_an_adc_only_board() {
        assert_eq!(card_directions(ADC_ONLY_PCM, 2), (false, true));
        assert_eq!(card_directions(ADC_ONLY_PCM, 0), (true, false));
        assert_eq!(card_directions(ADC_ONLY_PCM, 5), (false, false));
    }

    #[test]
    fn card_directions_reads_a_card_that_does_both() {
        assert_eq!(card_directions(DACADC_PCM, 0), (true, true));
        // The Pi 4's DAC+.
        let dacplus = "00-00: HiFiBerry DAC+ HiFi pcm512x-hifi-0 : \
            HiFiBerry DAC+ HiFi pcm512x-hifi-0 : playback 1\n";
        assert_eq!(card_directions(dacplus, 0), (true, false));
    }

    #[test]
    fn a_second_device_of_a_card_counts() {
        let text = "03-00: a : a : playback 1\n03-01: b : b : capture 1\n";
        assert_eq!(card_directions(text, 3), (true, true));
        // Card 13 is not card 3.
        assert_eq!(
            card_directions("13-00: a : a : capture 1\n", 3),
            (false, false)
        );
    }

    #[test]
    fn auto_is_the_capture_only_on_a_card_that_only_captures() {
        use crate::config::Clock;
        assert!(capture_clocked(Clock::Auto, Some((false, true))));
        assert!(!capture_clocked(Clock::Auto, Some((true, true))));
        assert!(!capture_clocked(Clock::Auto, Some((true, false))));
        // A card that is not there yet is played to, as before.
        assert!(!capture_clocked(Clock::Auto, None));
        assert!(capture_clocked(Clock::Capture, Some((true, true))));
        assert!(capture_clocked(Clock::Capture, None));
        assert!(!capture_clocked(Clock::Playback, Some((false, true))));
    }

    #[test]
    fn the_loopback_headphone_and_hdmi_cards_are_skipped() {
        let cards = ids(&[
            "Loopback",
            "Headphones",
            "vc4hdmi0",
            "vc4hdmi1",
            "sndrpihifiberry",
        ]);
        assert_eq!(find_card(&cards).as_deref(), Some("sndrpihifiberry"));
        let cards = ids(&["vc4hdmi", "DAC", "sndrpihifiberry"]);
        assert_eq!(find_card(&cards).as_deref(), Some("DAC"));
    }

    #[test]
    fn no_usable_card_gives_none() {
        assert_eq!(find_card(&[]), None);
        assert_eq!(
            find_card(&ids(&["Loopback", "Headphones", "vc4hdmi0"])),
            None
        );
    }

    #[test]
    fn a_named_card_is_present_by_id() {
        let cards = ids(&["sndrpihifiberry", "Loopback"]);
        assert!(is_present("Loopback", &cards));
        assert!(!is_present("Headphones", &cards));
        assert!(is_present("3", &cards));
    }

    #[test]
    fn the_card_to_play_to_is_looked_for_as_at_an_open() {
        let cards = ids(&["Headphones", "sndrpihifiberry"]);
        assert!(wanted_card_is_among(None, &cards));
        assert!(wanted_card_is_among(Some("sndrpihifiberry"), &cards));
        assert!(!wanted_card_is_among(Some("DAC"), &cards));
        // Only cards that are never chosen: nothing to open.
        let cards = ids(&["Headphones", "vc4hdmi0"]);
        assert!(!wanted_card_is_among(None, &cards));
        assert!(wanted_card_is_among(Some("Headphones"), &cards));
        assert!(!wanted_card_is_among(None, &[]));
        // A card named by index is left for ALSA to find.
        assert!(wanted_card_is_among(Some("1"), &[]));
    }

    #[test]
    fn card_ids_are_read_in_card_order() {
        let text = concat!(
            " 0 [Headphones     ]: bcm2835_headpho - bcm2835 Headphones\n",
            "                      bcm2835 Headphones\n",
            " 1 [sndrpihifiberry]: HifiberryDacpAd - snd_rpi_hifiberry_dacplusadc\n",
            "                      snd_rpi_hifiberry_dacplusadc\n",
            "10 [Loopback       ]: Loopback - Loopback\n",
            "                      Loopback 1\n",
        );
        assert_eq!(
            parse_card_ids(text),
            ["Headphones", "sndrpihifiberry", "Loopback"]
        );
        assert!(parse_card_ids("--- no soundcards ---\n").is_empty());
    }

    #[test]
    fn s32_is_written_as_it_is() {
        let mut out = [0u8; 8];
        encode(CardFormat::S32, &[0x1234_5678, -2], &mut out);
        assert_eq!(out, [0x78, 0x56, 0x34, 0x12, 0xfe, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn s24_keeps_the_top_24_bits_in_the_low_three_bytes() {
        let mut out = [0u8; 8];
        encode(CardFormat::S24, &[0x1234_5678, i32::MIN], &mut out);
        assert_eq!(out, [0x56, 0x34, 0x12, 0x00, 0x00, 0x00, 0x80, 0xff]);
    }

    const CARDS_TEXT: &str = concat!(
        " 0 [Headphones     ]: bcm2835_headpho - bcm2835 Headphones\n",
        "                      bcm2835 Headphones\n",
        " 1 [sndrpihifiberry]: HifiberryDacpAd - snd_rpi_hifiberry_dacplusadc\n",
        "                      snd_rpi_hifiberry_dacplusadc\n",
    );

    #[test]
    fn only_a_device_that_is_gone_counts_as_gone() {
        // An underrun or a suspend stays an xrun, also during or after the
        // restart that follows one.
        for recovering in [false, true] {
            for errno in [libc::EPIPE, libc::ESTRPIPE, libc::EINTR, libc::EIO] {
                assert!(!card_is_gone(errno, false, recovering), "{errno}");
            }
            assert!(card_is_gone(libc::ENODEV, false, recovering));
            // A stream left in a bad state is prepared again, unless its
            // device was disconnected.
            assert!(!card_is_gone(libc::EBADFD, false, recovering));
            assert!(card_is_gone(libc::EBADFD, true, recovering));
        }
        // A card that gives no room for half a second is stuck once a
        // restart has not helped.
        assert!(!card_is_gone(libc::ETIMEDOUT, false, false));
        assert!(card_is_gone(libc::ETIMEDOUT, false, true));
    }

    #[test]
    fn a_card_is_known_by_its_index_whatever_it_is_named_by() {
        assert_eq!(card_index(CARDS_TEXT, Some("1")), Some(1));
        assert_eq!(card_index(CARDS_TEXT, Some("sndrpihifiberry")), Some(1));
        assert_eq!(card_index(CARDS_TEXT, Some("Headphones")), Some(0));
        // The one found for the master when it names none.
        assert_eq!(card_index(CARDS_TEXT, None), Some(1));
        // A card not there now cannot be told.
        assert_eq!(card_index(CARDS_TEXT, Some("Loopback")), None);
        assert_eq!(card_index(CARDS_TEXT, Some("7")), None);
    }

    #[test]
    fn two_outputs_on_the_same_card_are_refused_with_both_names() {
        let named = |cards: &[Option<&str>]| -> Vec<Option<String>> {
            cards.iter().map(|c| c.map(String::from)).collect()
        };
        let both = named(&[Some("1"), Some("sndrpihifiberry")]);
        let e = distinct_cards(CARDS_TEXT, &both).unwrap_err();
        assert!(e.contains("output 1 (card 1)"), "{e}");
        assert!(e.contains("output 2 (card sndrpihifiberry)"), "{e}");
        // The master's card found, and the same card named by an output.
        let found = named(&[None, Some("Headphones"), Some("sndrpihifiberry")]);
        let e = distinct_cards(CARDS_TEXT, &found).unwrap_err();
        assert!(e.contains("output 1 (the card found)"), "{e}");
        assert!(e.contains("output 3 (card sndrpihifiberry)"), "{e}");
        assert_eq!(
            distinct_cards(CARDS_TEXT, &named(&[None, Some("0")])),
            Ok(())
        );
        // Cards not there now are let through.
        let absent = named(&[Some("Loopback"), Some("Loopback")]);
        assert_eq!(distinct_cards(CARDS_TEXT, &absent), Ok(()));
    }

    #[test]
    fn the_driver_is_that_of_the_card_played_to() {
        let found = Some((
            "sndrpihifiberry".to_string(),
            "snd_rpi_hifiberry_dacplusadc".to_string(),
        ));
        assert_eq!(card_driver(CARDS_TEXT, None), found);
        assert_eq!(card_driver(CARDS_TEXT, Some("sndrpihifiberry")), found);
        // A card named by index is found by it too.
        assert_eq!(card_driver(CARDS_TEXT, Some("1")), found);
        assert_eq!(
            card_driver(CARDS_TEXT, Some("Headphones")),
            Some(("Headphones".to_string(), "bcm2835 Headphones".to_string()))
        );
    }

    #[test]
    fn a_card_that_is_missing_has_no_driver() {
        assert_eq!(card_driver(CARDS_TEXT, Some("DAC")), None);
        assert_eq!(card_driver(CARDS_TEXT, Some("7")), None);
        assert_eq!(
            card_driver(" 0 [Headphones     ]: bcm2835 - x\n", None),
            None
        );
        assert_eq!(card_driver("--- no soundcards ---\n", None), None);
    }

    #[test]
    fn frames_are_muted_until_the_adc_has_settled() {
        let mut left = 3;
        let mut out = [[1, 1], [2, 2]];
        mute_settling(&mut out, &mut left);
        assert_eq!((out, left), ([[0, 0], [0, 0]], 1));
        let mut out = [[1, 1], [2, 2]];
        mute_settling(&mut out, &mut left);
        assert_eq!((out, left), ([[0, 0], [2, 2]], 0));
        let mut out = [[3, 3]];
        mute_settling(&mut out, &mut left);
        assert_eq!((out, left), ([[3, 3]], 0));
    }

    fn analog_config(enabled: bool, card: Option<&str>) -> Config {
        let mut config = Config::default();
        config.analog.enabled = enabled;
        config.card = card.map(str::to_string);
        config
    }

    #[test]
    fn a_card_present_at_the_start_is_decided_on_once() {
        let config = analog_config(true, None);
        assert_eq!(
            analog_card(CARDS_TEXT, &config),
            AnalogCard::Present {
                card: "sndrpihifiberry".to_string(),
                driver: "snd_rpi_hifiberry_dacplusadc".to_string()
            }
        );
    }

    #[test]
    fn a_card_missing_at_the_start_is_looked_for_at_each_open() {
        let config = analog_config(true, Some("DAC"));
        assert_eq!(analog_card(CARDS_TEXT, &config), AnalogCard::Absent);
        let config = analog_config(true, None);
        assert_eq!(
            analog_card("--- no soundcards ---\n", &config),
            AnalogCard::Absent
        );
    }

    #[test]
    fn a_disabled_input_needs_no_card() {
        let config = analog_config(false, None);
        assert_eq!(analog_card(CARDS_TEXT, &config), AnalogCard::Off);
        assert_eq!(analog_card("", &config), AnalogCard::Off);
    }

    #[test]
    fn a_late_card_opens_once_it_is_there_and_has_an_adc() {
        assert_eq!(
            capture_card("--- no soundcards ---\n", None),
            Err(OpenError::Again("no sound card found".to_string()))
        );
        assert_eq!(
            capture_card(CARDS_TEXT, Some("DAC")),
            Err(OpenError::Again("card DAC not found".to_string()))
        );
        assert_eq!(
            capture_card(CARDS_TEXT, None),
            Ok("sndrpihifiberry".to_string())
        );
        assert_eq!(
            capture_card(CARDS_TEXT, Some("Headphones")),
            Err(OpenError::Never(
                "the card bcm2835 Headphones has no analog input aas knows".to_string()
            ))
        );
    }

    #[test]
    fn captured_s32_is_read_as_it_is() {
        let mut out = [[0; 2]; 1];
        decode(
            CardFormat::S32,
            &[0x78, 0x56, 0x34, 0x12, 0xfe, 0xff, 0xff, 0xff],
            &mut out,
        );
        assert_eq!(out, [[0x1234_5678, -2]]);
    }

    #[test]
    fn captured_s24_moves_to_the_top_whatever_the_top_byte_holds() {
        let mut out = [[0; 2]; 2];
        let bytes = [
            0x56, 0x34, 0x12, 0x00, 0x00, 0x00, 0x80, 0xff, // sign-extended
            0x00, 0x00, 0x80, 0x00, 0xff, 0xff, 0x7f, 0xab, // top byte not
        ];
        decode(CardFormat::S24, &bytes, &mut out);
        assert_eq!(out, [[0x1234_5600, i32::MIN], [i32::MIN, 0x7fff_ff00]]);
    }

    #[test]
    fn captured_s16_is_left_justified() {
        let mut out = [[0; 2]; 1];
        decode(CardFormat::S16, &[0x34, 0x12, 0xff, 0xff], &mut out);
        assert_eq!(out, [[0x1234_0000, -65536]]);
    }

    #[test]
    fn decoding_reverses_encoding_in_the_top_bits() {
        let frames = [[0x1234_5678, -0x0765_4321], [i32::MAX, i32::MIN]];
        for format in CardFormat::PREFERRED {
            let mut bytes = vec![0u8; frames.len() * 2 * format.bytes()];
            encode(format, frames.as_flattened(), &mut bytes);
            let mut back = [[0; 2]; 2];
            decode(format, &bytes, &mut back);
            let bits = 8 * format.bytes() as u32 - if format == CardFormat::S24 { 8 } else { 0 };
            let mask = !0u32 << (32 - bits);
            for (b, f) in back.iter().flatten().zip(frames.iter().flatten()) {
                assert_eq!(*b as u32, *f as u32 & mask, "{}", format.name());
            }
        }
    }

    #[test]
    fn samples_are_encoded_in_the_order_given_whatever_the_channel_count() {
        let samples = [1 << 16, 2 << 16, 3 << 16, 4 << 16, 5 << 16, 6 << 16];
        let mut out = [0u8; 12];
        encode(CardFormat::S16, &samples, &mut out);
        assert_eq!(out, [1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
        let mut out = [0u8; 24];
        encode(CardFormat::S32, &samples, &mut out);
        for (i, o) in out.chunks_exact(4).enumerate() {
            assert_eq!(i32::from_le_bytes(o.try_into().unwrap()), samples[i]);
        }
    }

    #[test]
    fn s16_keeps_the_top_16_bits() {
        let mut out = [0u8; 4];
        encode(CardFormat::S16, &[0x1234_5678, -65536], &mut out);
        assert_eq!(out, [0x34, 0x12, 0xff, 0xff]);
    }
}
