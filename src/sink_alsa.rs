//! The sound card as a sink, through ALSA.
//!
//! Finding the card and converting samples need no ALSA, so they build and
//! are tested everywhere. `AlsaSink` and `AlsaVolume` need the
//! `alsa-output` feature.

use std::fs;

/// Where the kernel lists the sound cards.
const CARDS: &str = "/proc/asound/cards";

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

/// Whether a card named by id is among `ids`. A card named by index is
/// left for ALSA to find.
pub fn is_present(card: &str, ids: &[String]) -> bool {
    card.parse::<u32>().is_ok() || ids.iter().any(|id| id == card)
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

/// Writes S32 frames into `out` as `format`, little endian, keeping the top
/// bits. `out` holds exactly the frames.
pub fn encode(format: CardFormat, frames: &[[i32; 2]], out: &mut [u8]) {
    let samples = frames.iter().flatten();
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
pub use card::AlsaSink;
#[cfg(feature = "alsa-output")]
pub use mixer::AlsaVolume;

#[cfg(feature = "alsa-output")]
mod mixer {
    use super::card_device;
    use crate::volume::{HardwareVolume, find_control};
    use alsa::Round;
    use alsa::mixer::{MilliBel, Mixer, Selem, SelemId};

    /// A volume control of the card, set in dB. Used from the control
    /// thread only.
    pub struct AlsaVolume {
        mixer: Mixer,
        id: SelemId,
        range: (f32, f32),
        switch: bool,
        /// The lowest step of the control is a mute.
        floor_mutes: bool,
    }

    /// What ALSA reports as the dB of a step that mutes, in millibel.
    const MUTE_MB: i64 = -9_999_999;

    /// The control's dB range in millibel, if it has a playback volume
    /// with one.
    fn db_range(selem: &Selem) -> Option<(i64, i64)> {
        if !selem.has_playback_volume() {
            return None;
        }
        let (min, max) = selem.get_playback_db_range();
        (min < max).then_some((min.0, max.0))
    }

    impl AlsaVolume {
        /// The card's volume control: `configured` if the card has it, else
        /// the first of `Digital`, `Master` and `PCM`. None if the card is
        /// missing or has no such control with a range in dB.
        pub fn open(card: Option<&str>, configured: Option<&str>) -> Option<AlsaVolume> {
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
                "aas: hardware volume {device} {name}, {floor} to {} dB{}",
                range.1,
                if switch { ", with a switch" } else { "" }
            );
            Some(AlsaVolume {
                mixer,
                id,
                range,
                switch,
                floor_mutes,
            })
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
        }

        fn range_db(&self) -> (f32, f32) {
            self.range
        }

        fn can_mute(&self) -> bool {
            self.switch || self.floor_mutes
        }
    }
}

#[cfg(feature = "alsa-output")]
mod card {
    use super::{CardFormat, card_device, encode};
    use crate::latency::PERIODS;
    use crate::sink::{Sink, SinkError};
    use alsa::pcm::{Access, Format, Frames, HwParams, PCM};
    use alsa::{Direction, ValueOr};

    const CHANNELS: usize = 2;

    /// How long a write waits for room before the card counts as stuck.
    const WAIT_MS: u32 = 500;

    /// Rates offered to the rate rules, if the card takes them.
    const RATES: [u32; 14] = [
        8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000,
        352800, 384000,
    ];

    fn alsa_format(format: CardFormat) -> Format {
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

    /// The best format the card takes, and its rates in that format.
    fn choose(hw: &HwParams) -> alsa::Result<(CardFormat, Vec<u32>)> {
        hw.set_channels(CHANNELS as u32)?;
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
        /// Frames in the card's buffer.
        buffer: u32,
        /// Frames in the card's period, which is also its avail_min.
        period: usize,
        /// Room for one engine period in the card's format.
        bytes: Vec<u8>,
        /// The card's whole buffer of silence, to restart it after an xrun.
        zeros: Vec<u8>,
    }

    impl Open {
        fn frame_bytes(&self) -> usize {
            self.format.bytes() * CHANNELS
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
                    Err(e) if e.errno() == libc::EAGAIN => {
                        if !self.pcm.wait(Some(WAIT_MS))? {
                            return Err(alsa::Error::new("snd_pcm_wait", libc::ETIMEDOUT));
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        /// Waits until the card has room for one engine period, or for the
        /// whole buffer if that is smaller.
        fn wait_room(&self) -> alsa::Result<()> {
            let period = (self.bytes.len() / self.frame_bytes()).min(self.buffer as usize);
            while (self.pcm.avail_update()? as usize) < period {
                match self.pcm.wait(Some(WAIT_MS)) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(alsa::Error::new("snd_pcm_wait", libc::ETIMEDOUT));
                    }
                    Err(e) if e.errno() == libc::EINTR => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        /// Converts and writes the frames, a buffer's worth at a time.
        fn play(&mut self, frames: &[[i32; 2]]) -> alsa::Result<()> {
            let per_chunk = (self.bytes.len() / self.frame_bytes()).max(1);
            for chunk in frames.chunks(per_chunk) {
                let len = chunk.len() * self.frame_bytes();
                encode(self.format, chunk, &mut self.bytes[..len]);
                self.write_all(&self.bytes[..len])?;
            }
            Ok(())
        }

        /// Restarts the card after an xrun: prepare, then two periods of
        /// zeros, which fill the buffer and so start it.
        fn recover(&self) -> alsa::Result<()> {
            self.pcm.prepare()?;
            self.write_all(&self.zeros)
        }
    }

    /// Plays to `hw:<card>`, as S32 if the card takes it, else S24 in 32
    /// bits, else S16. Two periods are buffered and the card starts once
    /// both are written.
    pub struct AlsaSink {
        /// The card the config names. None finds one at each open.
        card: Option<String>,
        open: Option<Open>,
        rates: Vec<u32>,
        /// The last failure logged, so that a retry every second does not
        /// repeat it.
        last_failure: Option<String>,
    }

    impl AlsaSink {
        /// Looks at the card once for its rates. A card that is missing now
        /// is looked for again at each open.
        pub fn new(card: Option<String>) -> AlsaSink {
            let mut sink = AlsaSink {
                card,
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
            let (format, rates) = choose(&hw).map_err(e)?;
            eprintln!("aas: card {device} takes {}, {rates:?}", format.name());
            Ok(rates)
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

        fn try_open(&mut self, rate: u32, period: u32) -> Result<Open, String> {
            let device = self.device()?;
            let e = |what: &str, err: alsa::Error| format!("{device}: {what}: {err}");
            let pcm = open_pcm(&device).map_err(|x| e("open", x))?;
            let (format, buffer, card_period) = {
                let hw = HwParams::any(&pcm).map_err(|x| e("hw params", x))?;
                let (format, rates) = choose(&hw).map_err(|x| e("format", x))?;
                self.rates = rates;
                if !self.rates.contains(&rate) {
                    return Err(format!("{device}: does not take {rate} Hz"));
                }
                hw.set_rate(rate, ValueOr::Nearest)
                    .map_err(|x| e("rate", x))?;
                hw.set_period_size_near(Frames::from(period), ValueOr::Nearest)
                    .map_err(|x| e("period", x))?;
                hw.set_periods(PERIODS, ValueOr::Nearest)
                    .map_err(|x| e("periods", x))?;
                pcm.hw_params(&hw).map_err(|x| e("apply", x))?;
                let got_rate = hw.get_rate().map_err(|x| e("get rate", x))?;
                if got_rate != rate {
                    return Err(format!("{device}: asked for {rate} Hz, got {got_rate}"));
                }
                let got_period = hw.get_period_size().map_err(|x| e("get period", x))?;
                let got_buffer = hw.get_buffer_size().map_err(|x| e("get buffer", x))?;
                (format, got_buffer, got_period)
            };
            {
                let sw = pcm.sw_params_current().map_err(|x| e("sw params", x))?;
                sw.set_start_threshold(buffer)
                    .map_err(|x| e("start threshold", x))?;
                sw.set_avail_min(card_period)
                    .map_err(|x| e("avail min", x))?;
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
            let frame_bytes = format.bytes() * CHANNELS;
            Ok(Open {
                pcm,
                format,
                buffer: buffer as u32,
                period: card_period as usize,
                bytes: vec![0; period as usize * frame_bytes],
                zeros: vec![0; buffer as usize * frame_bytes],
            })
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
                Err(e) => {
                    self.failed(e);
                    Err(SinkError::Gone)
                }
            }
        }

        fn wait_room(&mut self) -> Result<(), SinkError> {
            let Some(open) = &mut self.open else {
                return Err(SinkError::Gone);
            };
            let result = match open.wait_room() {
                Ok(()) => Ok(()),
                Err(e) if e.errno() == libc::ENODEV => Err(SinkError::Gone),
                // The restart fills the buffer, so wait for room again.
                Err(_) => match open.recover().and_then(|()| open.wait_room()) {
                    Ok(()) => Err(SinkError::Xrun),
                    Err(_) => Err(SinkError::Gone),
                },
            };
            if result == Err(SinkError::Gone) {
                self.open = None;
            }
            result
        }

        fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
            let Some(open) = &mut self.open else {
                return Err(SinkError::Gone);
            };
            // alsa::Error holds the positive errno.
            let result = match open.play(frames) {
                Ok(()) => Ok(()),
                Err(e) if e.errno() == libc::ENODEV => Err(SinkError::Gone),
                // An underrun, a suspend, or anything else a restart may cure.
                Err(_) => match open.recover() {
                    Ok(()) => Err(SinkError::Xrun),
                    Err(_) => Err(SinkError::Gone),
                },
            };
            if result == Err(SinkError::Gone) {
                // Lets the driver go, so that the module can be unloaded.
                self.open = None;
            }
            result
        }

        fn close(&mut self) {
            self.open = None;
        }

        fn delay_frames(&self) -> u32 {
            self.open.as_ref().map_or(0, |open| open.buffer)
        }

        fn supported_rates(&self) -> Vec<u32> {
            self.rates.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
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
        encode(CardFormat::S32, &[[0x1234_5678, -2]], &mut out);
        assert_eq!(out, [0x78, 0x56, 0x34, 0x12, 0xfe, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn s24_keeps_the_top_24_bits_in_the_low_three_bytes() {
        let mut out = [0u8; 8];
        encode(CardFormat::S24, &[[0x1234_5678, i32::MIN]], &mut out);
        assert_eq!(out, [0x56, 0x34, 0x12, 0x00, 0x00, 0x00, 0x80, 0xff]);
    }

    #[test]
    fn s16_keeps_the_top_16_bits() {
        let mut out = [0u8; 4];
        encode(CardFormat::S16, &[[0x1234_5678, -65536]], &mut out);
        assert_eq!(out, [0x34, 0x12, 0xff, 0xff]);
    }
}
