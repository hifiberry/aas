//! Where the engine's output goes.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often `FallbackSink` tries the real sink again.
const RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// Frames were lost, and the sink has recovered.
    Xrun,
    /// The device has gone away.
    Gone,
}

/// A device that takes S32 stereo frames. `write` is the audio thread's only
/// wait, and it returns when the device can take the next period.
pub trait Sink: Send {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError>;
    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError>;
    fn close(&mut self);
    /// Frames between the sink's input and the listener.
    fn delay_frames(&self) -> u32;
    /// Rates the device takes. Empty means any.
    fn supported_rates(&self) -> Vec<u32>;
}

impl Sink for Box<dyn Sink> {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
        (**self).open(rate, period)
    }

    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
        (**self).write(frames)
    }

    fn close(&mut self) {
        (**self).close()
    }

    fn delay_frames(&self) -> u32 {
        (**self).delay_frames()
    }

    fn supported_rates(&self) -> Vec<u32> {
        (**self).supported_rates()
    }
}

/// Makes writes take as long as the audio they carry.
#[derive(Default)]
struct Pacer {
    rate: u32,
    /// When the frames written so far have played.
    next: Option<Instant>,
}

impl Pacer {
    fn start(&mut self, rate: u32) {
        self.rate = rate;
        self.next = None;
    }

    fn wait(&mut self, frames: usize) {
        if self.rate == 0 {
            return;
        }
        let now = Instant::now();
        let length = Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(self.rate));
        // A caller that fell behind starts again from now.
        let due = self.next.filter(|&t| t > now).unwrap_or(now) + length;
        std::thread::sleep(due - now);
        self.next = Some(due);
    }
}

/// Throws the audio away in real time.
#[derive(Default)]
pub struct NullSink {
    pacer: Pacer,
}

impl Sink for NullSink {
    fn open(&mut self, rate: u32, _period: u32) -> Result<(), SinkError> {
        self.pacer.start(rate);
        Ok(())
    }

    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
        self.pacer.wait(frames.len());
        Ok(())
    }

    fn close(&mut self) {}

    fn delay_frames(&self) -> u32 {
        0
    }

    fn supported_rates(&self) -> Vec<u32> {
        Vec::new()
    }
}

#[derive(Default)]
struct Record {
    frames: Vec<[i32; 2]>,
    opens: Vec<(u32, u32)>,
    closes: usize,
    failure: Option<SinkError>,
    delay: u32,
    rates: Vec<u32>,
}

/// Records what it is given, for tests. Its log stays readable after the sink
/// has moved to the audio thread.
#[derive(Default)]
pub struct MemorySink {
    record: Arc<Mutex<Record>>,
    /// Takes one period per write when set.
    pacer: Option<Pacer>,
}

impl MemorySink {
    /// A sink that returns at once.
    pub fn new() -> MemorySink {
        MemorySink::default()
    }

    /// A sink that takes as long as a card would.
    pub fn paced() -> MemorySink {
        MemorySink {
            pacer: Some(Pacer::default()),
            ..MemorySink::default()
        }
    }

    pub fn log(&self) -> SinkLog {
        SinkLog(self.record.clone())
    }

    fn record(&self) -> MutexGuard<'_, Record> {
        self.record.lock().unwrap()
    }
}

impl Sink for MemorySink {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
        let mut record = self.record();
        if let Some(e) = record.failure {
            return Err(e);
        }
        record.opens.push((rate, period));
        drop(record);
        if let Some(pacer) = &mut self.pacer {
            pacer.start(rate);
        }
        Ok(())
    }

    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
        let mut record = self.record();
        if let Some(e) = record.failure {
            return Err(e);
        }
        record.frames.extend_from_slice(frames);
        drop(record);
        if let Some(pacer) = &mut self.pacer {
            pacer.wait(frames.len());
        }
        Ok(())
    }

    fn close(&mut self) {
        self.record().closes += 1;
    }

    fn delay_frames(&self) -> u32 {
        self.record().delay
    }

    fn supported_rates(&self) -> Vec<u32> {
        self.record().rates.clone()
    }
}

/// A handle on what a `MemorySink` has seen, and on how it behaves.
#[derive(Clone)]
pub struct SinkLog(Arc<Mutex<Record>>);

impl SinkLog {
    fn record(&self) -> MutexGuard<'_, Record> {
        self.0.lock().unwrap()
    }

    pub fn frames(&self) -> Vec<[i32; 2]> {
        self.record().frames.clone()
    }

    /// Every successful `open`, as rate and period.
    pub fn opens(&self) -> Vec<(u32, u32)> {
        self.record().opens.clone()
    }

    pub fn closes(&self) -> usize {
        self.record().closes
    }

    /// Makes `open` and `write` fail with this error until told otherwise.
    pub fn fail(&self, failure: Option<SinkError>) {
        self.record().failure = failure;
    }

    pub fn set_delay(&self, frames: u32) {
        self.record().delay = frames;
    }

    pub fn set_rates(&self, rates: Vec<u32>) {
        self.record().rates = rates;
    }

    /// Makes room, so that recording does not allocate during a test of the
    /// audio thread.
    pub fn reserve(&self, frames: usize) {
        self.record().frames.reserve(frames);
    }
}

/// Keeps the engine running while the real sink is gone. It writes to a
/// `NullSink` meanwhile and retries the real one every second. Only an xrun
/// gets through to the engine.
pub struct FallbackSink {
    inner: Box<dyn Sink>,
    null: NullSink,
    /// The format to reopen with.
    format: (u32, u32),
    /// True while `inner` is open and working.
    live: bool,
    retry_at: Instant,
    retry: Duration,
}

impl FallbackSink {
    pub fn new(inner: Box<dyn Sink>) -> FallbackSink {
        FallbackSink {
            inner,
            null: NullSink::default(),
            format: (0, 0),
            live: false,
            retry_at: Instant::now(),
            retry: RETRY,
        }
    }

    fn try_inner(&mut self) {
        let (rate, period) = self.format;
        self.live = self.inner.open(rate, period).is_ok();
        self.retry_at = Instant::now() + self.retry;
    }
}

impl Sink for FallbackSink {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
        self.format = (rate, period);
        self.null.open(rate, period)?;
        self.try_inner();
        Ok(())
    }

    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
        if !self.live && Instant::now() >= self.retry_at {
            self.try_inner();
        }
        if self.live {
            match self.inner.write(frames) {
                Err(SinkError::Gone) => {
                    self.inner.close();
                    self.live = false;
                    self.retry_at = Instant::now() + self.retry;
                }
                result => return result,
            }
        }
        self.null.write(frames)
    }

    fn close(&mut self) {
        if self.live {
            self.inner.close();
            self.live = false;
        }
    }

    fn delay_frames(&self) -> u32 {
        if self.live {
            self.inner.delay_frames()
        } else {
            self.null.delay_frames()
        }
    }

    fn supported_rates(&self) -> Vec<u32> {
        self.inner.supported_rates()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sink_records_what_it_is_given() {
        let mut sink = MemorySink::new();
        let log = sink.log();
        sink.open(48000, 2).unwrap();
        sink.write(&[[1, 2], [3, 4]]).unwrap();
        sink.close();
        assert_eq!(log.frames(), [[1, 2], [3, 4]]);
        assert_eq!(log.opens(), [(48000, 2)]);
        assert_eq!(log.closes(), 1);
    }

    #[test]
    fn memory_sink_fails_when_told_to() {
        let mut sink = MemorySink::new();
        let log = sink.log();
        sink.open(48000, 2).unwrap();
        log.fail(Some(SinkError::Xrun));
        assert_eq!(sink.write(&[[1, 2]]), Err(SinkError::Xrun));
        log.fail(Some(SinkError::Gone));
        assert_eq!(sink.write(&[[1, 2]]), Err(SinkError::Gone));
        assert_eq!(sink.open(48000, 2), Err(SinkError::Gone));
        log.fail(None);
        sink.write(&[[5, 6]]).unwrap();
        assert_eq!(log.frames(), [[5, 6]]);
    }

    fn time_of_writes(sink: &mut dyn Sink, writes: usize) -> Duration {
        sink.open(48000, 480).unwrap();
        let frames = [[0; 2]; 480];
        let start = Instant::now();
        for _ in 0..writes {
            sink.write(&frames).unwrap();
        }
        start.elapsed()
    }

    #[test]
    fn a_paced_memory_sink_takes_one_period_per_write() {
        let took = time_of_writes(&mut MemorySink::paced(), 5);
        assert!(took >= Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn an_unpaced_memory_sink_does_not_wait() {
        let took = time_of_writes(&mut MemorySink::new(), 5);
        assert!(took < Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn null_sink_takes_one_period_per_write() {
        let took = time_of_writes(&mut NullSink::default(), 5);
        assert!(took >= Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn a_boxed_sink_is_a_sink() {
        let memory = MemorySink::new();
        let log = memory.log();
        let mut sink: Box<dyn Sink> = Box::new(memory);
        log.set_delay(7);
        log.set_rates(vec![44100]);
        sink.open(44100, 1).unwrap();
        sink.write(&[[9, 9]]).unwrap();
        sink.close();
        assert_eq!(sink.delay_frames(), 7);
        assert_eq!(sink.supported_rates(), [44100]);
        assert_eq!(log.frames(), [[9, 9]]);
    }

    fn fallback(retry: Duration) -> (FallbackSink, SinkLog) {
        let memory = MemorySink::new();
        let log = memory.log();
        let mut sink = FallbackSink::new(Box::new(memory));
        sink.retry = retry;
        (sink, log)
    }

    #[test]
    fn fallback_passes_through_while_the_inner_sink_works() {
        let (mut sink, log) = fallback(RETRY);
        log.set_delay(11);
        sink.open(48000, 1).unwrap();
        sink.write(&[[1, 1]]).unwrap();
        assert_eq!(log.frames(), [[1, 1]]);
        assert_eq!(sink.delay_frames(), 11);
        sink.close();
        assert_eq!(log.closes(), 1);
    }

    #[test]
    fn fallback_passes_an_xrun_on() {
        let (mut sink, log) = fallback(RETRY);
        sink.open(48000, 1).unwrap();
        log.fail(Some(SinkError::Xrun));
        assert_eq!(sink.write(&[[1, 1]]), Err(SinkError::Xrun));
    }

    #[test]
    fn fallback_hides_a_gone_sink_and_keeps_time() {
        let (mut sink, log) = fallback(RETRY);
        sink.open(48000, 480).unwrap();
        log.fail(Some(SinkError::Gone));
        assert_eq!(sink.write(&[[0; 2]; 480]), Ok(()));
        let start = Instant::now();
        assert_eq!(sink.write(&[[0; 2]; 480]), Ok(()));
        assert_eq!(sink.write(&[[0; 2]; 480]), Ok(()));
        assert!(start.elapsed() >= Duration::from_millis(10));
        assert_eq!(sink.delay_frames(), 0);
    }

    #[test]
    fn fallback_opens_without_error_when_the_inner_sink_is_gone() {
        let (mut sink, log) = fallback(RETRY);
        log.fail(Some(SinkError::Gone));
        assert_eq!(sink.open(48000, 48), Ok(()));
        assert_eq!(sink.write(&[[0; 2]; 48]), Ok(()));
    }

    #[test]
    fn fallback_returns_to_the_inner_sink_at_the_next_retry() {
        let (mut sink, log) = fallback(Duration::from_millis(5));
        sink.open(48000, 48).unwrap();
        log.fail(Some(SinkError::Gone));
        sink.write(&[[1; 2]; 48]).unwrap();
        log.fail(None);
        // Too early: the retry is not due, so this goes to the null sink.
        sink.write(&[[2; 2]; 48]).unwrap();
        assert!(log.frames().is_empty());
        std::thread::sleep(Duration::from_millis(6));
        sink.write(&[[3; 2]; 48]).unwrap();
        assert_eq!(log.frames(), [[3; 2]; 48]);
        assert_eq!(log.opens(), [(48000, 48), (48000, 48)]);
    }

    #[test]
    fn fallback_tries_the_inner_sink_again_when_reopened() {
        let (mut sink, log) = fallback(Duration::ZERO);
        sink.open(48000, 48).unwrap();
        log.fail(Some(SinkError::Gone));
        sink.write(&[[1; 2]; 48]).unwrap();
        sink.close();
        log.fail(None);
        sink.open(44100, 32).unwrap();
        assert_eq!(*log.opens().last().unwrap(), (44100, 32));
        sink.write(&[[4; 2]; 32]).unwrap();
        assert_eq!(log.frames(), [[4; 2]; 32]);
    }
}
