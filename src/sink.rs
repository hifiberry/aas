//! Where the engine's output goes.

use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::latency::PERIODS;

/// How often the real sink is tried again, or looked for, while it is gone.
pub const RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// Frames were lost, and the sink has recovered.
    Xrun,
    /// The device has gone away.
    Gone,
}

/// A device that takes S32 stereo frames. `wait_room` is the audio thread's
/// only wait. The engine calls it before it reads the clients, so that what
/// it reads goes to the device at once and `write` finds room.
pub trait Sink: Send {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError>;
    /// Returns once the device has room for one period. A sink that never
    /// waits returns at once.
    fn wait_room(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError>;
    fn close(&mut self);
    /// Frames between the sink's input and the listener.
    fn delay_frames(&self) -> u32;
    /// Rates the device takes. Empty means any.
    fn supported_rates(&self) -> Vec<u32>;
    /// True once after a `write` that moved to another device, such as
    /// from no card to a card that is back. Opening it took that write
    /// far longer than the buffer lasts, with nothing audible at stake, so
    /// the engine does not count the time to the next cycle as a late
    /// wake-up.
    fn switched(&mut self) -> bool {
        false
    }
}

impl Sink for Box<dyn Sink> {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
        (**self).open(rate, period)
    }

    fn wait_room(&mut self) -> Result<(), SinkError> {
        (**self).wait_room()
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

    fn switched(&mut self) -> bool {
        (**self).switched()
    }
}

/// Keeps time like a card with two periods buffered: writes queue up, and
/// `wait_room` returns once the queue has room for one more period.
#[derive(Default)]
struct Pacer {
    rate: u32,
    period: u32,
    /// When the frames written so far have played.
    next: Option<Instant>,
}

impl Pacer {
    fn start(&mut self, rate: u32, period: u32) {
        self.rate = rate;
        self.period = period;
        self.next = None;
    }

    fn length(&self, frames: u64) -> Duration {
        Duration::from_nanos(frames * 1_000_000_000 / u64::from(self.rate))
    }

    fn wait_room(&mut self) {
        let Some(next) = self.next else {
            return;
        };
        let queue = self.length(u64::from(self.period) * u64::from(PERIODS - 1));
        let now = Instant::now();
        if let Some(due) = next.checked_sub(queue).filter(|&due| due > now) {
            std::thread::sleep(due - now);
        }
    }

    fn write(&mut self, frames: usize) {
        if self.rate == 0 {
            return;
        }
        let now = Instant::now();
        // A writer that fell behind starts again from now.
        let from = self.next.filter(|&t| t > now).unwrap_or(now);
        self.next = Some(from + self.length(frames as u64));
    }
}

/// Throws the audio away in real time.
#[derive(Default)]
pub struct NullSink {
    pacer: Pacer,
}

impl Sink for NullSink {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
        self.pacer.start(rate, period);
        Ok(())
    }

    fn wait_room(&mut self) -> Result<(), SinkError> {
        self.pacer.wait_room();
        Ok(())
    }

    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
        self.pacer.write(frames.len());
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
    /// Keeps time like a card when set.
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
            pacer.start(rate, period);
        }
        Ok(())
    }

    fn wait_room(&mut self) -> Result<(), SinkError> {
        if let Some(pacer) = &mut self.pacer {
            pacer.wait_room();
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
            pacer.write(frames.len());
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

/// Slots for rate changes not yet picked up. A change that finds them full
/// is offered again at the next try.
const RATE_SLOTS: usize = 4;

/// Rates a sink has newly reported, for the control thread to pick up. The
/// audio thread hands them over through a lock-free queue.
pub struct RatesWatch(Consumer<Vec<u32>>);

impl RatesWatch {
    /// The latest rates reported since the last call, if any.
    pub fn take(&mut self) -> Option<Vec<u32>> {
        let mut latest = None;
        while let Ok(rates) = self.0.pop() {
            latest = Some(rates);
        }
        latest
    }
}

/// Tells the control thread that the real sink has gone or started to play
/// again, so that it can let go of the card's volume control or open it
/// anew. The audio thread only adds to a shared counter, which neither locks
/// nor allocates.
pub struct CardWatch {
    changes: Arc<AtomicU64>,
    seen: u64,
}

impl CardWatch {
    /// Whether the real sink has gone or started to play since the last call.
    pub fn changed(&mut self) -> bool {
        let changes = self.changes.load(Ordering::Acquire);
        let changed = changes != self.seen;
        self.seen = changes;
        changed
    }
}

/// The control thread's side of the search for a card that is gone. The
/// control thread looks for the card, which reads a file and allocates,
/// and tells the audio thread to try the sink once it is there. The audio
/// thread only reads a flag meanwhile.
pub struct CardSearch {
    gone: Arc<AtomicBool>,
    retry: Arc<AtomicBool>,
}

impl CardSearch {
    /// Whether the output is meant to play and the real sink is gone.
    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// Has the audio thread try the real sink at its next write.
    pub fn retry(&self) {
        self.retry.store(true, Ordering::Release);
    }
}

/// Keeps the engine running while the real sink is gone. It writes to a
/// `NullSink` meanwhile and tries the real one again: when the control
/// thread says so through a `CardSearch`, or every second by itself if
/// nobody holds one. Only an xrun gets through to the engine.
///
/// A try opens the sink, which may allocate and read files. By itself the
/// audio thread would do that once a second for as long as the card is
/// gone. With a `CardSearch` it does so only once the card has been seen,
/// as for any other open.
///
/// A card can report other rates once it is found, so after each try the
/// rates are compared with the last known ones, and a change is published
/// to the `RatesWatch`.
pub struct FallbackSink {
    inner: Box<dyn Sink>,
    /// The rates last published, or found at the start.
    rates: Vec<u32>,
    watch: Option<Producer<Vec<u32>>>,
    null: NullSink,
    /// The format to reopen with.
    format: (u32, u32),
    /// True while `inner` is open and working.
    live: bool,
    /// A `write` has just gone back to `inner`, until the engine asks.
    switched: bool,
    /// `inner` was open and working when it was last closed. If it cannot
    /// be opened after that, the card went while the output was closed.
    closed_live: bool,
    /// Counts the times `inner` went from closed or gone to open, and from
    /// open to gone, which includes an open that fails after it was closed
    /// while it worked. A card that is back may be a new instance, its
    /// mixer at its own default. A card that is gone may wait to be removed
    /// until every handle on it is closed, the mixer's too.
    changes: Arc<AtomicU64>,
    /// The output is open for the engine and `inner` is not.
    gone: Arc<AtomicBool>,
    /// Set by the control thread to ask for a try.
    retry_now: Arc<AtomicBool>,
    /// A `CardSearch` was handed out, so retries come from there.
    searched: bool,
    retry_at: Instant,
    retry: Duration,
}

impl FallbackSink {
    pub fn new(inner: Box<dyn Sink>) -> FallbackSink {
        FallbackSink {
            rates: inner.supported_rates(),
            watch: None,
            inner,
            null: NullSink::default(),
            format: (0, 0),
            live: false,
            switched: false,
            closed_live: false,
            changes: Arc::default(),
            gone: Arc::default(),
            retry_now: Arc::default(),
            searched: false,
            retry_at: Instant::now(),
            retry: RETRY,
        }
    }

    /// Where changes to the inner sink's rates are published from now on.
    pub fn rates_watch(&mut self) -> RatesWatch {
        let (producer, consumer) = RingBuffer::new(RATE_SLOTS);
        self.watch = Some(producer);
        RatesWatch(consumer)
    }

    /// Where the control thread learns that the inner sink has gone or plays
    /// again.
    pub fn card_watch(&self) -> CardWatch {
        CardWatch {
            changes: self.changes.clone(),
            seen: self.changes.load(Ordering::Acquire),
        }
    }

    /// Hands the search for a card that is gone to the control thread. From
    /// now on the inner sink is retried only when the `CardSearch` says so.
    pub fn card_search(&mut self) -> CardSearch {
        self.searched = true;
        CardSearch {
            gone: self.gone.clone(),
            retry: self.retry_now.clone(),
        }
    }

    /// Whether to try the inner sink again now.
    fn retry_due(&mut self) -> bool {
        if self.searched {
            self.retry_now.swap(false, Ordering::AcqRel)
        } else {
            Instant::now() >= self.retry_at
        }
    }

    /// Opens the inner sink. This only happens on a reopen or a retry, so
    /// it may allocate.
    fn try_inner(&mut self) {
        let (rate, period) = self.format;
        // The control thread holds the card's mixer open while the output
        // is closed, and must let go of a card that went meanwhile.
        let was_live = self.live;
        let closed_live = std::mem::take(&mut self.closed_live);
        self.live = self.inner.open(rate, period).is_ok();
        let changed = if self.live {
            !was_live
        } else {
            was_live || closed_live
        };
        if changed {
            self.changes.fetch_add(1, Ordering::Release);
        }
        self.gone.store(!self.live, Ordering::Release);
        self.retry_at = Instant::now() + self.retry;
        let rates = self.inner.supported_rates();
        if rates == self.rates {
            return;
        }
        let Some(watch) = &mut self.watch else {
            self.rates = rates;
            return;
        };
        if watch.push(rates.clone()).is_ok() {
            self.rates = rates;
        }
    }
}

impl FallbackSink {
    /// The inner sink has gone. It is closed, and tried again at the next
    /// retry.
    fn lost(&mut self) {
        self.inner.close();
        self.live = false;
        // A retry asked for while the sink still worked is about a loss
        // before this one.
        self.retry_now.store(false, Ordering::Release);
        self.gone.store(true, Ordering::Release);
        self.retry_at = Instant::now() + self.retry;
        self.changes.fetch_add(1, Ordering::Release);
    }
}

impl Sink for FallbackSink {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
        self.format = (rate, period);
        self.null.open(rate, period)?;
        // This open is a try of its own. One asked for before it is stale.
        self.retry_now.store(false, Ordering::Release);
        self.try_inner();
        Ok(())
    }

    fn wait_room(&mut self) -> Result<(), SinkError> {
        if self.live {
            match self.inner.wait_room() {
                Err(SinkError::Gone) => self.lost(),
                result => return result,
            }
        }
        self.null.wait_room()
    }

    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
        if !self.live && self.retry_due() {
            self.try_inner();
            self.switched = self.live;
        }
        if self.live {
            match self.inner.write(frames) {
                Err(SinkError::Gone) => self.lost(),
                result => return result,
            }
        }
        self.null.write(frames)
    }

    fn close(&mut self) {
        if self.live {
            self.inner.close();
            self.live = false;
            self.closed_live = true;
        }
        self.gone.store(false, Ordering::Release);
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

    fn switched(&mut self) -> bool {
        std::mem::take(&mut self.switched)
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

    /// Waits for room and writes a 10 ms period, as the engine does.
    fn time_of_cycles(sink: &mut dyn Sink, cycles: usize) -> Duration {
        sink.open(48000, 480).unwrap();
        let frames = [[0; 2]; 480];
        let start = Instant::now();
        for _ in 0..cycles {
            sink.wait_room().unwrap();
            sink.write(&frames).unwrap();
        }
        start.elapsed()
    }

    #[test]
    fn a_paced_memory_sink_takes_one_period_per_cycle_once_full() {
        // Two periods fill the queue, five more take a period each.
        let took = time_of_cycles(&mut MemorySink::paced(), 7);
        assert!(took >= Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn an_unpaced_memory_sink_does_not_wait() {
        let took = time_of_cycles(&mut MemorySink::new(), 7);
        assert!(took < Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn null_sink_takes_one_period_per_cycle_once_full() {
        let took = time_of_cycles(&mut NullSink::default(), 7);
        assert!(took >= Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn null_sink_has_room_while_less_than_two_periods_are_queued() {
        let took = time_of_cycles(&mut NullSink::default(), 2);
        assert!(took < Duration::from_millis(10), "{took:?}");
    }

    #[test]
    fn null_sink_writes_do_not_wait() {
        let mut sink = NullSink::default();
        sink.open(48000, 480).unwrap();
        let start = Instant::now();
        for _ in 0..5 {
            sink.write(&[[0; 2]; 480]).unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(10));
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
        for _ in 0..3 {
            assert_eq!(sink.wait_room(), Ok(()));
            assert_eq!(sink.write(&[[0; 2]; 480]), Ok(()));
        }
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
        let (mut sink, log) = fallback(RETRY);
        sink.open(48000, 48).unwrap();
        log.fail(Some(SinkError::Gone));
        sink.write(&[[1; 2]; 48]).unwrap();
        log.fail(None);
        // Too early: the retry is not due, so this goes to the null sink.
        sink.write(&[[2; 2]; 48]).unwrap();
        assert!(log.frames().is_empty());
        sink.retry_at = Instant::now();
        sink.write(&[[3; 2]; 48]).unwrap();
        assert_eq!(log.frames(), [[3; 2]; 48]);
        assert_eq!(log.opens(), [(48000, 48), (48000, 48)]);
    }

    #[test]
    fn fallback_says_once_that_a_write_went_back_to_the_inner_sink() {
        let (mut sink, log) = fallback(Duration::ZERO);
        sink.open(48000, 48).unwrap();
        // Opened by the engine, not inside a write.
        assert!(!sink.switched());
        log.fail(Some(SinkError::Gone));
        sink.write(&[[1; 2]; 48]).unwrap();
        sink.write(&[[1; 2]; 48]).unwrap();
        assert!(!sink.switched());
        log.fail(None);
        sink.write(&[[2; 2]; 48]).unwrap();
        assert!(sink.switched());
        assert!(!sink.switched());
        sink.write(&[[2; 2]; 48]).unwrap();
        assert!(!sink.switched());
    }

    #[test]
    fn a_searched_fallback_retries_only_when_told() {
        // No wait between retries, so a retry by the clock would come at
        // every write.
        let (mut sink, log) = fallback(Duration::ZERO);
        let search = sink.card_search();
        assert!(!search.is_gone());
        log.fail(Some(SinkError::Gone));
        sink.open(48000, 48).unwrap();
        assert!(search.is_gone());
        log.fail(None);
        for _ in 0..3 {
            sink.write(&[[1; 2]; 48]).unwrap();
        }
        assert!(log.opens().is_empty());
        search.retry();
        sink.write(&[[2; 2]; 48]).unwrap();
        assert_eq!(log.frames(), [[2; 2]; 48]);
        assert!(!search.is_gone());
        // A retry that fails is asked for again, not repeated by itself.
        log.fail(Some(SinkError::Gone));
        sink.write(&[[3; 2]; 48]).unwrap();
        assert!(search.is_gone());
        search.retry();
        sink.write(&[[3; 2]; 48]).unwrap();
        log.fail(None);
        sink.write(&[[3; 2]; 48]).unwrap();
        assert_eq!(log.opens().len(), 1);
        assert!(search.is_gone());
        // A closed output is not looked for.
        sink.close();
        assert!(!search.is_gone());
    }

    #[test]
    fn a_retry_asked_for_while_the_sink_worked_is_not_kept_for_the_next_loss() {
        let (mut sink, log) = fallback(Duration::ZERO);
        let search = sink.card_search();
        sink.open(48000, 48).unwrap();
        // The control thread saw the sink gone a moment ago and asks late.
        search.retry();
        sink.write(&[[1; 2]; 48]).unwrap();
        log.fail(Some(SinkError::Gone));
        sink.write(&[[1; 2]; 48]).unwrap();
        assert!(search.is_gone());
        // Nobody has seen the card since it went, so it is not tried, even
        // though it would open.
        log.fail(None);
        sink.write(&[[2; 2]; 48]).unwrap();
        assert_eq!(log.opens().len(), 1);
        assert!(search.is_gone());
    }

    #[test]
    fn a_searched_fallback_does_not_allocate_while_the_card_is_gone() {
        let (mut sink, log) = fallback(Duration::ZERO);
        let _search = sink.card_search();
        let _rates = sink.rates_watch();
        // A try would copy these, which allocates.
        log.set_rates(vec![44100, 48000]);
        log.fail(Some(SinkError::Gone));
        sink.open(48000, 48).unwrap();
        let frames = [[0; 2]; 48];
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..3 {
                sink.wait_room().unwrap();
                sink.write(&frames).unwrap();
            }
        });
    }

    #[test]
    fn fallback_publishes_the_rates_of_a_sink_that_appears() {
        let (mut sink, log) = fallback(Duration::ZERO);
        let mut watch = sink.rates_watch();
        log.fail(Some(SinkError::Gone));
        sink.open(96000, 48).unwrap();
        assert_eq!(watch.take(), None);
        log.set_rates(vec![44100, 48000]);
        log.fail(None);
        sink.write(&[[0; 2]; 48]).unwrap();
        assert_eq!(watch.take(), Some(vec![44100, 48000]));
        // Unchanged rates are not published again.
        sink.close();
        sink.open(48000, 48).unwrap();
        assert_eq!(watch.take(), None);
    }

    #[test]
    fn a_rate_change_that_finds_the_queue_full_is_offered_again() {
        let (mut sink, log) = fallback(Duration::ZERO);
        let mut watch = sink.rates_watch();
        for rate in 1..=RATE_SLOTS as u32 + 1 {
            log.set_rates(vec![rate]);
            sink.open(48000, 48).unwrap();
        }
        assert_eq!(watch.take(), Some(vec![RATE_SLOTS as u32]));
        sink.open(48000, 48).unwrap();
        assert_eq!(watch.take(), Some(vec![RATE_SLOTS as u32 + 1]));
    }

    #[test]
    fn the_card_watch_sees_each_loss_and_return_of_the_inner_sink() {
        let (mut sink, log) = fallback(Duration::ZERO);
        let mut watch = sink.card_watch();
        assert!(!watch.changed());
        sink.open(48000, 48).unwrap();
        assert!(watch.changed());
        assert!(!watch.changed());
        // A rate change while it plays is neither.
        sink.open(44100, 48).unwrap();
        assert!(!watch.changed());
        // Gone, so the control thread can let go of the old card.
        log.fail(Some(SinkError::Gone));
        sink.write(&[[0; 2]; 48]).unwrap();
        assert!(watch.changed());
        // A failed retry is no change.
        sink.write(&[[0; 2]; 48]).unwrap();
        assert!(!watch.changed());
        log.fail(None);
        sink.write(&[[0; 2]; 48]).unwrap();
        assert!(watch.changed());
        // Closed while idle, then opened: the card may have changed meanwhile.
        sink.close();
        assert!(!watch.changed());
        sink.open(48000, 48).unwrap();
        assert!(watch.changed());
    }

    #[test]
    fn the_card_watch_sees_a_card_lost_while_the_output_was_closed() {
        let (mut sink, log) = fallback(Duration::ZERO);
        let mut watch = sink.card_watch();
        sink.open(48000, 48).unwrap();
        assert!(watch.changed());
        // Closed while idle. The card goes meanwhile, and nothing can tell.
        sink.close();
        log.fail(Some(SinkError::Gone));
        assert!(!watch.changed());
        // The next open finds it gone: the control thread must hear of it,
        // to let go of the card's volume control.
        sink.open(48000, 48).unwrap();
        assert!(watch.changed());
        // Further failed tries are no change.
        sink.write(&[[0; 2]; 48]).unwrap();
        sink.close();
        sink.open(48000, 48).unwrap();
        assert!(!watch.changed());
        log.fail(None);
        sink.write(&[[0; 2]; 48]).unwrap();
        assert!(watch.changed());
    }

    #[test]
    fn the_card_watch_sees_a_card_lost_at_a_reopen_while_it_played() {
        let (mut sink, log) = fallback(Duration::ZERO);
        let mut watch = sink.card_watch();
        sink.open(48000, 48).unwrap();
        assert!(watch.changed());
        log.fail(Some(SinkError::Gone));
        sink.open(44100, 48).unwrap();
        assert!(watch.changed());
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
