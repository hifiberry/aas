//! The sound card as a sink, through ALSA.
//!
//! Finding the card and converting samples need no ALSA, so they build and
//! are tested everywhere. `AlsaSink`, `AlsaCapture` and `AlsaVolume` need
//! the `alsa-output` feature.

use crate::capture::OpenError;
use crate::config::Config;
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

/// The kernel's list of cards now, empty if it cannot be read.
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
pub use capture::AlsaCapture;
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

    /// How long a write waits for room before the card counts as stuck.
    const WAIT_MS: u32 = 500;

    /// Rates offered to the rate rules, if the card takes them.
    const RATES: [u32; 14] = [
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

        /// Restarts the card after an xrun: prepare, then two periods of
        /// zeros, which fill the buffer and so start it.
        fn recover(&self) -> alsa::Result<()> {
            self.pcm.prepare()?;
            self.write_all(&self.zeros)
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

    impl From<String> for OpenFailure {
        fn from(message: String) -> OpenFailure {
            OpenFailure::Failed(message)
        }
    }

    /// Plays to `hw:<card>` with a fixed number of channels, as S32 if the
    /// card takes it, else S24 in 32 bits, else S16. Two periods are
    /// buffered and the card starts once both are written.
    pub struct AlsaSink {
        /// The card the config names. None finds one at each open.
        card: Option<String>,
        channels: usize,
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
            let mut sink = AlsaSink {
                card,
                channels,
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
            let (format, rates) = self.choose(&device, &hw)?;
            eprintln!(
                "aas: card {device} takes {} channels, {}, {rates:?}",
                self.channels,
                format.name()
            );
            Ok(rates)
        }

        /// `choose` for this sink's channel count, with the failure for a
        /// card that does not take it.
        fn choose(&self, device: &str, hw: &HwParams) -> Result<(CardFormat, Vec<u32>), String> {
            if hw.test_channels(self.channels as u32).is_err() {
                return Err(format!(
                    "{device}: does not take {} channels",
                    self.channels
                ));
            }
            choose(hw, self.channels).map_err(|err| format!("{device}: format: {err}"))
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
                let channels = self.channels;
                let refused =
                    || OpenFailure::Refused(format!("{device}: does not take {channels} channels"));
                if hw.test_channels(self.channels as u32).is_err()
                    || hw.set_channels(self.channels as u32).is_err()
                {
                    return Err(refused());
                }
                let (format, rates) = self.choose(&device, &hw)?;
                self.rates = rates;
                if !self.rates.contains(&rate) {
                    return Err(format!("{device}: does not take {rate} Hz").into());
                }
                hw.set_rate(rate, ValueOr::Nearest)
                    .map_err(|x| e("rate", x))?;
                hw.set_period_size_near(Frames::from(period), ValueOr::Nearest)
                    .map_err(|x| e("period", x))?;
                hw.set_periods(PERIODS, ValueOr::Nearest)
                    .map_err(|x| e("periods", x))?;
                if let Err(x) = pcm.hw_params(&hw) {
                    // The probe took the channel count and the rate and
                    // period were granted within what the card takes, so a
                    // driver that refuses here refuses the channels, as
                    // some I2S controllers do for more than two.
                    if x.errno() == libc::EINVAL {
                        return Err(refused());
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

        fn write(&mut self, samples: &[i32]) -> Result<(), SinkError> {
            let Some(open) = &mut self.open else {
                return Err(SinkError::Gone);
            };
            // alsa::Error holds the positive errno.
            let result = match open.play(samples) {
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

        fn channels(&self) -> usize {
            self.channels
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

#[cfg(feature = "alsa-output")]
mod capture {
    use super::card::alsa_format;
    use super::{CardFormat, SETTLE_MS, decode, mute_settling};
    use crate::capture::Capture;
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
