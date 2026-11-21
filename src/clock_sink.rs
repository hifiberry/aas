//! A master clocked by a capture card: [`ClockSink`] plays nowhere. Its
//! `wait_room` waits until the capture card has a period, so the card's
//! clock drives the audio thread, and its `write` discards. It owns the
//! capture device and lends it to the analog input as a [`ClockCapture`],
//! since a capture PCM of these cards takes one opener.
//!
//! The engine reads the analog input after the wait and writes after
//! that. Whatever the input took in that cycle, read or skipped, counts
//! towards the period; `write` takes the rest off the device. A cycle with
//! no input, or with the input disabled, therefore still consumes one
//! period, and the next wait lasts one period of the card's clock.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::capture::{Capture, OpenError};
use crate::control::OpenCapture;
use crate::sink::{Sink, SinkError};

/// A capture device that can be waited on.
pub trait ClockDevice: Capture {
    /// Opens and starts it at `rate` with periods of `period` frames.
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError>;
    /// Returns once `available()` is at least `frames`. After an overrun
    /// it recovers and returns `Err(Xrun)`, with nothing queued.
    fn wait(&mut self, frames: usize) -> Result<(), SinkError>;
    /// The rates it takes. Empty means unknown.
    fn rates(&self) -> Vec<u32>;
    fn is_open(&self) -> bool;
}

struct Shared {
    device: Box<dyn ClockDevice>,
    /// Frames the analog input took this cycle, read or skipped.
    taken: usize,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    // Only the audio thread uses it while the output runs; a panic there
    // ends the daemon, so a poisoned lock is never seen in use.
    shared.lock().unwrap_or_else(|p| p.into_inner())
}

/// The master's sink under the capture clock.
pub struct ClockSink {
    shared: Arc<Mutex<Shared>>,
    channels: usize,
    period: usize,
}

impl ClockSink {
    pub fn new(device: Box<dyn ClockDevice>, channels: usize) -> ClockSink {
        let shared = Shared { device, taken: 0 };
        ClockSink {
            shared: Arc::new(Mutex::new(shared)),
            channels,
            period: 0,
        }
    }

    /// The analog input's view of the device.
    pub fn capture(&self) -> ClockCapture {
        ClockCapture(self.shared.clone())
    }
}

impl Sink for ClockSink {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
        let mut s = lock(&self.shared);
        s.taken = 0;
        s.device.open(rate, period)?;
        self.period = period as usize;
        Ok(())
    }

    fn wait_room(&mut self) -> Result<(), SinkError> {
        lock(&self.shared).device.wait(self.period)
    }

    /// Plays nowhere. Takes off the device what the analog input left of
    /// this cycle's period, so that the next wait lasts a period.
    fn write(&mut self, _samples: &[i32]) -> Result<(), SinkError> {
        let mut s = lock(&self.shared);
        let left = self.period.saturating_sub(std::mem::take(&mut s.taken));
        if left > 0 {
            s.device.skip(left)?;
        }
        Ok(())
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn close(&mut self) {
        let mut s = lock(&self.shared);
        s.taken = 0;
        s.device.close();
    }

    fn delay_frames(&self) -> u32 {
        0
    }

    fn supported_rates(&self) -> Vec<u32> {
        lock(&self.shared).device.rates()
    }
}

/// The analog input reading the clock's device. Closing it leaves the
/// device to the sink.
#[derive(Clone)]
pub struct ClockCapture(Arc<Mutex<Shared>>);

impl Capture for ClockCapture {
    /// 0 while the sink has the device closed.
    fn available(&mut self) -> Result<usize, SinkError> {
        let mut s = lock(&self.0);
        if !s.device.is_open() {
            return Ok(0);
        }
        s.device.available()
    }

    fn read(&mut self, out: &mut [[i32; 2]]) -> Result<usize, SinkError> {
        let mut s = lock(&self.0);
        if !s.device.is_open() {
            return Ok(0);
        }
        let n = s.device.read(out)?;
        s.taken += n;
        Ok(n)
    }

    fn skip(&mut self, frames: usize) -> Result<(), SinkError> {
        let mut s = lock(&self.0);
        if !s.device.is_open() {
            return Ok(());
        }
        // Asking for more than is queued takes all of it, which is at
        // least the period the wait waited for: counting what was asked
        // leaves `write` nothing to take either way.
        s.device.skip(frames)?;
        s.taken += frames;
        Ok(())
    }

    /// The sink owns the device: it closes it when the output closes.
    fn close(&mut self) {}
}

/// The analog input's open under the capture clock: the clock's device,
/// once the card is there with an ADC `aas` knows. `driver` names the
/// card's driver, `None` while the card is not there yet, so that a card
/// that probes after the start is picked up when the control thread tries
/// again.
pub fn analog_open(
    capture: ClockCapture,
    mut driver: impl FnMut() -> Option<String> + 'static,
) -> OpenCapture {
    Box::new(move |_rate, _period| {
        let Some(driver) = driver() else {
            return Err(OpenError::Again("the card is not there yet".into()));
        };
        if !crate::cards::has_adc(&driver) {
            return Err(OpenError::Never(format!(
                "the card {driver} has no analog input aas knows"
            )));
        }
        Ok(Box::new(capture.clone()) as Box<dyn Capture>)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// What a test sees of the fake device.
    #[derive(Default)]
    struct State {
        queued: VecDeque<[i32; 2]>,
        open: Option<(u32, u32)>,
        rates: Vec<u32>,
        gone: bool,
        overrun: bool,
        /// Frames the last wait had the card deliver.
        delivered: usize,
        next: i32,
    }

    /// A device whose wait has the card deliver what is missing, numbered
    /// frames, so that a test sees how long each wait lasted.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<State>>);

    impl Fake {
        fn state(&self) -> MutexGuard<'_, State> {
            self.0.lock().unwrap()
        }

        fn with_rates(rates: &[u32]) -> Fake {
            let fake = Fake::default();
            fake.state().rates = rates.to_vec();
            fake
        }
    }

    impl Capture for Fake {
        fn available(&mut self) -> Result<usize, SinkError> {
            let s = self.state();
            if s.gone {
                return Err(SinkError::Gone);
            }
            Ok(s.queued.len())
        }

        fn read(&mut self, out: &mut [[i32; 2]]) -> Result<usize, SinkError> {
            let mut s = self.state();
            let n = out.len().min(s.queued.len());
            for o in &mut out[..n] {
                *o = s.queued.pop_front().unwrap();
            }
            Ok(n)
        }

        fn skip(&mut self, frames: usize) -> Result<(), SinkError> {
            let mut s = self.state();
            let n = frames.min(s.queued.len());
            s.queued.drain(..n);
            Ok(())
        }

        fn close(&mut self) {
            let mut s = self.state();
            s.open = None;
            s.queued.clear();
        }
    }

    impl ClockDevice for Fake {
        fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
            let mut s = self.state();
            if s.gone {
                return Err(SinkError::Gone);
            }
            s.open = Some((rate, period));
            Ok(())
        }

        fn wait(&mut self, frames: usize) -> Result<(), SinkError> {
            let mut s = self.state();
            if s.gone || s.open.is_none() {
                return Err(SinkError::Gone);
            }
            if std::mem::take(&mut s.overrun) {
                s.queued.clear();
                s.delivered = 0;
                return Err(SinkError::Xrun);
            }
            let missing = frames.saturating_sub(s.queued.len());
            for _ in 0..missing {
                let v = s.next;
                s.next += 1;
                s.queued.push_back([v, -v]);
            }
            s.delivered = missing;
            Ok(())
        }

        fn rates(&self) -> Vec<u32> {
            self.state().rates.clone()
        }

        fn is_open(&self) -> bool {
            self.state().open.is_some()
        }
    }

    const PERIOD: u32 = 64;

    fn open_sink(fake: &Fake) -> ClockSink {
        let mut sink = ClockSink::new(Box::new(fake.clone()), 2);
        sink.open(48000, PERIOD).unwrap();
        sink
    }

    fn zeros() -> Vec<i32> {
        vec![0; PERIOD as usize * 2]
    }

    #[test]
    fn wait_room_waits_for_a_period() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        assert_eq!(fake.state().open, Some((48000, PERIOD)));
        sink.wait_room().unwrap();
        assert_eq!(fake.state().delivered, PERIOD as usize);
        assert_eq!(fake.state().queued.len(), PERIOD as usize);
    }

    #[test]
    fn a_cycle_nobody_reads_takes_a_period() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        for _ in 0..3 {
            sink.wait_room().unwrap();
            sink.write(&zeros()).unwrap();
            // Each wait lasts a whole period of the card's clock.
            assert_eq!(fake.state().delivered, PERIOD as usize);
            assert_eq!(fake.state().queued.len(), 0);
        }
    }

    #[test]
    fn a_cycle_the_input_reads_takes_nothing_more() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        let mut input = sink.capture();
        let mut buf = vec![[0; 2]; PERIOD as usize];
        sink.wait_room().unwrap();
        // Frames that came in after the wait stay for the input.
        fake.state().queued.extend([[1000, 1000]; 5]);
        assert_eq!(input.read(&mut buf).unwrap(), PERIOD as usize);
        assert_eq!(buf[0], [0, 0]);
        assert_eq!(buf[63], [63, -63]);
        sink.write(&zeros()).unwrap();
        assert_eq!(fake.state().queued.len(), 5);
        // A skip counts as taken too.
        sink.wait_room().unwrap();
        input.skip(PERIOD as usize).unwrap();
        sink.write(&zeros()).unwrap();
        assert_eq!(fake.state().queued.len(), 0);
    }

    #[test]
    fn a_skip_after_new_frames_keeps_the_rest() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        let mut input = sink.capture();
        sink.wait_room().unwrap();
        input.skip(PERIOD as usize).unwrap();
        fake.state().queued.extend([[9, 9]; 5]);
        sink.write(&zeros()).unwrap();
        assert_eq!(fake.state().queued.len(), 5);
    }

    #[test]
    fn a_reopen_forgets_what_was_taken_before() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        let mut input = sink.capture();
        sink.wait_room().unwrap();
        input.read(&mut [[0; 2]; 10]).unwrap();
        sink.open(48000, PERIOD).unwrap();
        sink.wait_room().unwrap();
        sink.write(&zeros()).unwrap();
        assert_eq!(fake.state().queued.len(), 0);
    }

    #[test]
    fn a_short_read_leaves_the_rest_of_the_period_to_the_sink() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        let mut input = sink.capture();
        let mut buf = vec![[0; 2]; 10];
        sink.wait_room().unwrap();
        input.read(&mut buf).unwrap();
        sink.write(&zeros()).unwrap();
        assert_eq!(fake.state().queued.len(), 0);
    }

    #[test]
    fn an_overrun_is_an_xrun_and_the_next_wait_is_a_period() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        fake.state().overrun = true;
        assert_eq!(sink.wait_room(), Err(SinkError::Xrun));
        sink.write(&zeros()).unwrap();
        sink.wait_room().unwrap();
        assert_eq!(fake.state().delivered, PERIOD as usize);
    }

    #[test]
    fn rates_are_the_captures() {
        let fake = Fake::with_rates(&[44100, 48000, 96000]);
        let sink = ClockSink::new(Box::new(fake), 2);
        assert_eq!(sink.supported_rates(), [44100, 48000, 96000]);
    }

    #[test]
    fn a_gone_device_is_gone_and_a_reopen_works() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        fake.state().gone = true;
        assert_eq!(sink.wait_room(), Err(SinkError::Gone));
        sink.close();
        assert_eq!(sink.open(48000, PERIOD), Err(SinkError::Gone));
        fake.state().gone = false;
        sink.open(96000, 128).unwrap();
        assert_eq!(fake.state().open, Some((96000, 128)));
        sink.wait_room().unwrap();
        assert_eq!(fake.state().delivered, 128);
    }

    #[test]
    fn the_capture_reads_nothing_while_closed() {
        let fake = Fake::default();
        let mut sink = ClockSink::new(Box::new(fake.clone()), 2);
        let mut input = sink.capture();
        fake.state().queued.extend([[1, 1]; 4]);
        assert_eq!(input.available().unwrap(), 0);
        assert_eq!(input.read(&mut [[0; 2]; 4]).unwrap(), 0);
        sink.open(48000, PERIOD).unwrap();
        assert_eq!(input.available().unwrap(), 4);
    }

    #[test]
    fn closing_the_capture_leaves_the_device_open() {
        let fake = Fake::default();
        let mut sink = open_sink(&fake);
        sink.capture().close();
        assert!(fake.state().open.is_some());
        sink.close();
        assert!(fake.state().open.is_none());
    }

    #[test]
    fn the_analog_input_waits_for_a_card_that_is_not_there_yet() {
        let sink = ClockSink::new(Box::new(Fake::default()), 2);
        let found = Arc::new(Mutex::new(None::<String>));
        let seen = found.clone();
        let mut open = analog_open(sink.capture(), move || seen.lock().unwrap().clone());
        assert!(matches!(open(48000, 64), Err(OpenError::Again(_))));
        *found.lock().unwrap() = Some("snd_rpi_hifiberry_adc".to_string());
        assert!(open(48000, 64).is_ok());
    }

    #[test]
    fn a_card_without_an_adc_ends_the_analog_input() {
        let sink = ClockSink::new(Box::new(Fake::default()), 2);
        let mut open = analog_open(sink.capture(), || Some("snd_rpi_hifiberry_digi".into()));
        assert!(matches!(open(48000, 64), Err(OpenError::Never(_))));
    }

    #[test]
    fn delay_is_zero_and_channels_are_the_mix() {
        let fake = Fake::default();
        let sink = open_sink(&fake);
        assert_eq!(sink.delay_frames(), 0);
        assert_eq!(sink.channels(), 2);
        let wide = ClockSink::new(Box::new(Fake::default()), 6);
        assert_eq!(wide.channels(), 6);
    }
}
