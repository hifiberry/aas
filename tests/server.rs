//! The daemon end to end: clients on a real socket, the output a `MemorySink`.

use aas::capture::{Capture, CaptureFeed, MemoryCapture};
use aas::config::{Analog, Config, Idle, OutputConfig, PluginConfig, PluginPlace};
use aas::control::OpenCapture;
use aas::rate::SETTLE;
use aas::server::{self, CardProbe, FurtherSink};
use aas::sink::{MemorySink, Sink, SinkError, SinkLog};
use aas::volume::{HardwareVolume, OpenVolume};
use aas_client::client::Client;
use aas_client::convert::ClientFormat;
use aas_client::proto::{self, Reply, Request};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Gives a period of 64 frames at 44.1 and 48 kHz.
const LATENCY_MS: f32 = 4.0;
const PERIOD: u32 = 64;
/// Long enough for anything here on a loaded machine.
const PATIENCE: Duration = Duration::from_secs(20);

/// Opens a capture. `Send`, so that it can go to the daemon's thread.
type SendCapture =
    Box<dyn FnMut(u32, u32) -> Result<Box<dyn Capture>, aas::capture::OpenError> + Send>;

/// Decides, on the daemon's thread and from its config, whether the
/// daemon gets a capture, as `main` does.
type CaptureSetup = Box<dyn FnOnce(&Config) -> Option<OpenCapture> + Send>;

struct Daemon {
    _dir: tempfile::TempDir,
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl Daemon {
    fn start(config: Config, sink: Box<dyn Sink>) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.toml");
        Daemon::start_with(config, sink, dir, state)
    }

    /// A daemon that keeps its state in `state`.
    fn start_with(
        config: Config,
        sink: Box<dyn Sink>,
        dir: tempfile::TempDir,
        state: PathBuf,
    ) -> Daemon {
        Daemon::start_full(config, sink, dir, state, None)
    }

    /// A daemon with a card volume control that `hardware` opens.
    fn start_with_volume(config: Config, sink: Box<dyn Sink>, hardware: OpenVolume) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.toml");
        Daemon::start_full(config, sink, dir, state, Some(hardware))
    }

    /// A daemon whose control thread looks for the card with `probe`.
    fn start_probing(config: Config, sink: Box<dyn Sink>, probe: CardProbe) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.toml");
        Daemon::start_all(
            config,
            sink,
            dir,
            state,
            None,
            Some(probe),
            None,
            Vec::new(),
        )
    }

    /// A daemon that also plays to `further`, one for each `[[output]]`.
    fn start_outputs(config: Config, sink: Box<dyn Sink>, further: Vec<FurtherSink>) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.toml");
        Daemon::start_all(config, sink, dir, state, None, None, None, further)
    }

    /// A daemon with the capture that `capture` decides on.
    fn start_analog(config: Config, sink: Box<dyn Sink>, capture: CaptureSetup) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.toml");
        Daemon::start_all(
            config,
            sink,
            dir,
            state,
            None,
            None,
            Some(capture),
            Vec::new(),
        )
    }

    fn start_full(
        config: Config,
        sink: Box<dyn Sink>,
        dir: tempfile::TempDir,
        state: PathBuf,
        hardware: Option<OpenVolume>,
    ) -> Daemon {
        Daemon::start_all(config, sink, dir, state, hardware, None, None, Vec::new())
    }

    #[allow(clippy::too_many_arguments)]
    fn start_all(
        config: Config,
        sink: Box<dyn Sink>,
        dir: tempfile::TempDir,
        state: PathBuf,
        hardware: Option<OpenVolume>,
        probe: Option<CardProbe>,
        capture: Option<CaptureSetup>,
        further: Vec<FurtherSink>,
    ) -> Daemon {
        let path = dir.path().join("aas.sock");
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let (path, shutdown) = (path.clone(), shutdown.clone());
            thread::spawn(move || {
                let capture = capture.and_then(|setup| setup(&config));
                let devices = server::Devices {
                    sink,
                    probe,
                    further,
                };
                server::run_outputs(config, devices, &path, &state, hardware, capture, shutdown)
            })
        };
        let daemon = Daemon {
            _dir: dir,
            path,
            shutdown,
            thread: Some(thread),
        };
        assert!(
            wait_for(|| daemon.path.exists()),
            "the socket never appeared"
        );
        daemon
    }

    fn client(&self) -> Client {
        Client::connect(&self.path).unwrap()
    }

    /// A configured client playing S32 stereo.
    fn client_at(&self, rate: u32, buffer_frames: u32) -> Client {
        let mut client = self.client();
        client
            .configure(rate, ClientFormat::S32, 2, buffer_frames)
            .unwrap();
        client
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            assert_eq!(result.unwrap(), Ok(()));
        }
    }
}

fn config(idle: Idle) -> Config {
    Config {
        latency_ms: Some(LATENCY_MS),
        idle,
        ..Config::default()
    }
}

fn default_config() -> Config {
    config(Idle::Off { delay_s: 60 })
}

/// A paced memory sink and its log.
fn memory_sink() -> (Box<dyn Sink>, SinkLog) {
    let sink = MemorySink::paced();
    let log = sink.log();
    (Box::new(sink), log)
}

fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    condition()
}

fn bytes(frames: &[[i32; 2]]) -> Vec<u8> {
    frames
        .iter()
        .flat_map(|f| [f[0].to_le_bytes(), f[1].to_le_bytes()])
        .flatten()
        .collect()
}

/// Writes every frame, waiting for room in the ring as needed.
fn write_all(client: &mut Client, frames: &[[i32; 2]]) {
    let data = bytes(frames);
    let mut done = 0;
    let deadline = Instant::now() + PATIENCE;
    while done < frames.len() {
        assert!(Instant::now() < deadline, "the ring never took the frames");
        let n = client.write(&data[done * 8..]).unwrap();
        done += n;
        if n == 0 {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

fn wait_played(client: &Client, frames: u64) {
    assert!(
        wait_for(|| client.hw_position().unwrap() >= frames),
        "only {} of {frames} frames were played",
        client.hw_position().unwrap()
    );
}

/// Distinct frames using all 32 bits, so that only an untouched path keeps
/// them. No sample carries a DoP marker.
fn exact_pattern(n: usize) -> Vec<[i32; 2]> {
    (0..n as i32)
        .map(|i| {
            let v = 0x1000_0000 + i * 0x0001_0203;
            [v, !v]
        })
        .collect()
}

/// Distinct non-zero samples that `f32` holds exactly, on the left channel
/// only, so a mix with a right-only source keeps them.
fn left_pattern(n: usize) -> Vec<[i32; 2]> {
    (1..=n as i32).map(|i| [i << 8, 0]).collect()
}

fn right_pattern(n: usize) -> Vec<[i32; 2]> {
    (1..=n as i32).map(|i| [0, -(i << 8)]).collect()
}

/// Fails with the first difference, not with both sequences in full.
fn assert_same(got: &[i32], want: &[i32], what: &str) {
    let first = got.iter().zip(want).position(|(g, w)| g != w);
    assert!(
        first.is_none() && got.len() == want.len(),
        "{what}: {} samples for {}, first difference at {first:?}",
        got.len(),
        want.len()
    );
}

fn find(haystack: &[[i32; 2]], needle: &[[i32; 2]]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn last_open(log: &SinkLog) -> Option<(u32, u32)> {
    log.opens().last().copied()
}

#[test]
fn a_client_plays_bit_exact_through_the_daemon() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut client = daemon.client_at(48000, 4096);
    let audio = exact_pattern(3000);
    write_all(&mut client, &audio);
    client.start().unwrap();
    wait_played(&client, 3000);
    client.drain().unwrap();
    let frames = log.frames();
    assert!(find(&frames, &audio).is_some(), "the audio was altered");
    assert_eq!(last_open(&log), Some((48000, PERIOD)));
}

#[test]
fn two_hundred_clients_can_connect() {
    let (sink, _log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut clients = Vec::new();
    for _ in 0..200 {
        let mut client = daemon.client_at(48000, 1024);
        write_all(&mut client, &left_pattern(128));
        client.start().unwrap();
        clients.push(client);
    }
    // Every source reached the engine's table, past its first 16 slots.
    for client in &clients {
        wait_played(client, 128);
    }
}

#[test]
fn second_client_at_another_rate_is_resampled() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut first = daemon.client_at(48000, 4096);
    first.start().unwrap();
    let mut second = daemon.client_at(44100, 32768);
    let tone: Vec<[i32; 2]> = (0..22050)
        .map(|i| {
            let phase = 2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44100.0;
            let v = (phase.sin() * 0.5 * 2147483648.0) as i32;
            [v, v]
        })
        .collect();
    write_all(&mut second, &tone);
    second.start().unwrap();
    // A resampled stream keeps a short last block until it drains.
    second.drain().unwrap();
    assert_eq!(second.hw_position().unwrap(), 22050);
    assert!(log.opens().iter().all(|&(rate, _)| rate == 48000));

    let frames = log.frames();
    let first_sound = frames.iter().position(|f| f[0] != 0).unwrap();
    let last_sound = frames.iter().rposition(|f| f[0] != 0).unwrap();
    // The middle, clear of the filter's start and end.
    let middle = &frames[first_sound + 2000..last_sound - 2000];
    let rising = middle
        .windows(2)
        .filter(|w| w[0][0] < 0 && w[1][0] >= 0)
        .count();
    let hz = rising as f64 * 48000.0 / middle.len() as f64;
    // Played unresampled it would be 1088 Hz.
    assert!((hz - 1000.0).abs() < 5.0, "measured {hz} Hz");
}

#[test]
fn rate_follows_the_remaining_client() {
    let (sink, log, _gate, opened) = gated_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut first = daemon.client_at(44100, 4096);
    first.start().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((44100, PERIOD))));
    let mut second = daemon.client_at(48000, 4096);
    second.start().unwrap();
    let stopping = Instant::now();
    first.stop().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((48000, PERIOD))));
    // Not before the settle time, however fast or slow the machine is.
    let (_, at) = *opened.lock().unwrap().last().unwrap();
    assert!(
        at >= stopping + SETTLE,
        "switched {:?} after the stop",
        at - stopping
    );
}

/// Stops the first `open` after `arm` until `release`.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<(bool, bool)>, Condvar)>);

impl Gate {
    fn arm(&self) {
        self.0.0.lock().unwrap().0 = true;
    }

    fn release(&self) {
        self.0.0.lock().unwrap().0 = false;
        self.0.1.notify_all();
    }

    fn entered(&self) -> bool {
        self.0.0.lock().unwrap().1
    }

    fn pass(&self) {
        let (lock, wake) = &*self.0;
        let mut state = lock.lock().unwrap();
        if state.0 {
            state.1 = true;
            while state.0 {
                state = wake.wait(state).unwrap();
            }
        }
    }
}

/// Each successful open's rate and when it happened.
type Opened = Arc<Mutex<Vec<(u32, Instant)>>>;

/// A paced memory sink whose opens can be held, and are timed.
struct GatedSink {
    inner: MemorySink,
    gate: Gate,
    opened: Opened,
}

fn gated_sink() -> (Box<dyn Sink>, SinkLog, Gate, Opened) {
    let inner = MemorySink::paced();
    let log = inner.log();
    let gate = Gate::default();
    let opened = Opened::default();
    let sink = GatedSink {
        inner,
        gate: gate.clone(),
        opened: opened.clone(),
    };
    (Box::new(sink), log, gate, opened)
}

impl Sink for GatedSink {
    fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
        self.gate.pass();
        self.inner.open(rate, period)?;
        self.opened.lock().unwrap().push((rate, Instant::now()));
        Ok(())
    }

    fn wait_room(&mut self) -> Result<(), SinkError> {
        self.inner.wait_room()
    }

    fn write(&mut self, samples: &[i32]) -> Result<(), SinkError> {
        self.inner.write(samples)
    }

    fn channels(&self) -> usize {
        self.inner.channels()
    }

    fn close(&mut self) {
        self.inner.close()
    }

    fn delay_frames(&self) -> u32 {
        self.inner.delay_frames()
    }

    fn supported_rates(&self) -> Vec<u32> {
        self.inner.supported_rates()
    }
}

#[test]
fn client_starting_during_reconfigure_loses_no_audio() {
    let (sink, log, gate, _opened) = gated_sink();
    let daemon = Daemon::start(default_config(), sink);
    assert!(wait_for(|| last_open(&log) == Some((48000, PERIOD))));

    let mut first = daemon.client_at(44100, 4096);
    let mut second = daemon.client_at(44100, 4096);
    let first_audio = left_pattern(2000);
    let second_audio = right_pattern(2000);
    write_all(&mut first, &first_audio);
    write_all(&mut second, &second_audio);

    gate.arm();
    // The output moves to 44.1 kHz, and the card is held in its reopen.
    first.start().unwrap();
    assert!(wait_for(|| gate.entered()), "the output was never reopened");
    second.start().unwrap();
    thread::sleep(Duration::from_millis(50));
    gate.release();
    assert!(wait_for(|| last_open(&log) == Some((44100, PERIOD))));

    wait_played(&first, 2000);
    wait_played(&second, 2000);
    let frames = log.frames();
    let left: Vec<i32> = frames.iter().map(|f| f[0]).filter(|&s| s != 0).collect();
    let right: Vec<i32> = frames.iter().map(|f| f[1]).filter(|&s| s != 0).collect();
    let want_left: Vec<i32> = first_audio.iter().map(|f| f[0]).collect();
    let want_right: Vec<i32> = second_audio.iter().map(|f| f[1]).collect();
    assert_same(&left, &want_left, "the first client lost or altered audio");
    assert_same(
        &right,
        &want_right,
        "the second client lost or altered audio",
    );
}

#[test]
fn drain_plays_the_last_partial_period() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    assert!(wait_for(|| last_open(&log) == Some((48000, 64))));
    let mut client = daemon.client_at(48000, 4096);
    let audio = exact_pattern(100);
    write_all(&mut client, &audio);
    // As ALSA does, a drain of a prepared stream starts it.
    client.drain().unwrap();

    let frames = log.frames();
    let at = find(&frames, &audio).expect("the last frames were not played");
    // The 100 frames start a period and the rest of the second is zeros.
    assert_eq!(at % 64, 0);
    assert_eq!(frames.len() % 64, 0);
    assert!(
        frames.len() >= at + 128,
        "drain returned before the padding"
    );
    assert!(frames[at + 100..at + 128].iter().all(|f| *f == [0, 0]));
    assert_eq!(client.hw_position().unwrap(), 100);
    assert_eq!(client.underruns(), 0, "the padding counted as an underrun");
}

#[test]
fn a_killed_client_is_removed_and_others_keep_playing() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut keeps = daemon.client_at(48000, 65536);
    let audio = left_pattern(48000);
    write_all(&mut keeps, &audio);
    let mut killed = daemon.client_at(48000, 262144);
    // Four seconds of audio, so it would still be playing at the end.
    write_all(&mut killed, &vec![[0, 0x100]; 192000]);
    keeps.start().unwrap();
    killed.start().unwrap();
    wait_played(&keeps, 4800);
    // Its socket closes and its mapping goes, as when the process dies.
    drop(killed);
    wait_played(&keeps, 48000);

    let frames = log.frames();
    let left: Vec<i32> = frames.iter().map(|f| f[0]).filter(|&s| s != 0).collect();
    let want: Vec<i32> = audio.iter().map(|f| f[0]).collect();
    assert_same(&left, &want, "the remaining client lost or altered audio");
    let last = frames.iter().rposition(|f| f[0] != 0).unwrap();
    let tail = &frames[last - 24000..=last];
    assert!(
        tail.iter().all(|f| f[1] == 0),
        "the killed client was still played"
    );
}

#[test]
fn a_stopped_client_does_not_hold_the_rate() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut paused = daemon.client_at(44100, 4096);
    paused.start().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((44100, PERIOD))));
    paused.stop().unwrap();
    let mut playing = daemon.client_at(48000, 4096);
    playing.start().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((48000, PERIOD))));
    let opens = log.opens().len();
    // Resuming, it is resampled and the rate stays.
    paused.start().unwrap();
    thread::sleep(Duration::from_millis(1000));
    assert_eq!(log.opens().len(), opens);
}

#[test]
fn output_sends_zeros_then_goes_off_after_the_delay() {
    let (sink, log) = memory_sink();
    let started = Instant::now();
    let _daemon = Daemon::start(config(Idle::Off { delay_s: 1 }), sink);
    assert!(wait_for(|| !log.frames().is_empty()));
    assert!(wait_for(|| log.closes() >= 1), "the output never went off");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "went off early"
    );
    let frames = log.frames();
    assert!(frames.iter().all(|f| *f == [0, 0]));
    assert!(
        frames.len() >= 48000 * 9 / 10,
        "zeros did not last the delay"
    );
    thread::sleep(Duration::from_millis(300));
    assert_eq!(log.frames().len(), frames.len(), "still writing when off");
    assert_eq!(log.opens().len(), 1);
}

#[test]
fn a_client_within_the_delay_needs_no_reopen() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(config(Idle::Off { delay_s: 30 }), sink);
    assert!(wait_for(|| last_open(&log).is_some()));
    let mut client = daemon.client_at(48000, 4096);
    write_all(&mut client, &left_pattern(1000));
    client.start().unwrap();
    wait_played(&client, 1000);
    client.stop().unwrap();
    thread::sleep(Duration::from_millis(300));
    client.start().unwrap();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(log.opens(), vec![(48000, PERIOD)]);
    assert_eq!(log.closes(), 0);
}

#[test]
fn with_idle_zeros_the_output_never_closes() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(config(Idle::Zeros), sink);
    let mut client = daemon.client_at(48000, 4096);
    write_all(&mut client, &left_pattern(1000));
    client.start().unwrap();
    wait_played(&client, 1000);
    client.stop().unwrap();
    thread::sleep(Duration::from_millis(2500));
    let written = log.frames().len();
    assert!(wait_for(|| log.frames().len() > written + 4800));
    assert_eq!(log.closes(), 0);
    assert_eq!(log.opens().len(), 1);
}

#[test]
fn a_gone_sink_falls_back_to_null_and_retries() {
    let (sink, log) = memory_sink();
    log.fail(Some(SinkError::Gone));
    let daemon = Daemon::start(default_config(), sink);
    let mut client = daemon.client_at(48000, 131072);
    write_all(&mut client, &vec![[0x100, 0x100]; 96000]);
    client.start().unwrap();
    // Played in real time to nowhere, while the card is gone.
    wait_played(&client, 4800);
    assert!(log.frames().is_empty());
    log.fail(None);
    assert!(wait_for(|| log.frames().contains(&[0x100, 0x100])));
    assert_eq!(last_open(&log), Some((48000, PERIOD)));
}

#[test]
fn a_gone_sink_is_tried_again_only_once_the_card_is_seen() {
    let (sink, log) = memory_sink();
    log.fail(Some(SinkError::Gone));
    let present = Arc::new(AtomicBool::new(false));
    let seen = present.clone();
    let probe = Box::new(move |_refused: bool| seen.load(Ordering::Relaxed));
    let daemon = Daemon::start_probing(default_config(), sink, probe);
    let mut client = daemon.client_at(48000, 262144);
    write_all(&mut client, &vec![[0x100, 0x100]; 192000]);
    client.start().unwrap();
    wait_played(&client, 4800);
    // The sink would open now, but the control thread sees no card, so
    // the audio thread is not asked to try.
    log.fail(None);
    thread::sleep(Duration::from_millis(2500));
    assert!(log.opens().is_empty());
    assert!(log.frames().is_empty());
    present.store(true, Ordering::Relaxed);
    assert!(wait_for(|| log.frames().contains(&[0x100, 0x100])));
    assert_eq!(last_open(&log), Some((48000, PERIOD)));
}

#[test]
fn a_lost_further_card_is_looked_for_every_second() {
    let (sink, _log) = memory_sink();
    // A further card buffers three periods.
    let card = MemorySink::paced_buffering(3);
    let further = card.log();
    further.fail(Some(SinkError::Gone));
    let present = Arc::new(AtomicBool::new(false));
    let looks = Arc::new(Mutex::new(0u32));
    let (seen, counted) = (present.clone(), looks.clone());
    let probe: CardProbe = Box::new(move |_refused: bool| {
        *counted.lock().unwrap() += 1;
        seen.load(Ordering::Relaxed)
    });
    let output = OutputConfig {
        card: "Loopback".into(),
        channels: 2,
        latency_ms: None,
        idle: Idle::Zeros,
        volume_db: 0.0,
        plugins: Vec::new(),
        matrix: None,
    };
    let config = Config {
        outputs: vec![output],
        ..default_config()
    };
    let outputs = vec![FurtherSink {
        sink: Box::new(card),
        probe: Some(probe),
    }];
    let daemon = Daemon::start_outputs(config, sink, outputs);
    let mut client = daemon.client_at(48000, 1 << 20);
    write_all(&mut client, &vec![[1 << 24, 1 << 24]; 480000]);
    client.start().unwrap();
    wait_played(&client, 4800);
    // The card would open now, but nothing sees it, so its writer is not
    // asked to try. It is looked for about once a second meanwhile.
    further.fail(None);
    *looks.lock().unwrap() = 0;
    thread::sleep(Duration::from_millis(3500));
    assert!(further.opens().is_empty());
    let looked = *looks.lock().unwrap();
    assert!((2..=5).contains(&looked), "looked {looked} times in 3.5 s");
    present.store(true, Ordering::Relaxed);
    assert!(wait_for(|| further.samples().iter().any(|&s| s != 0)));
    assert_eq!(further.opens().last(), Some(&(48000, PERIOD)));
}

#[test]
fn a_further_output_on_zeros_plays_on_while_the_masters_card_is_off() {
    let (sink, log) = memory_sink();
    let card = MemorySink::paced_buffering(3);
    let further = card.log();
    let output = OutputConfig {
        card: "Loopback".into(),
        channels: 2,
        latency_ms: None,
        idle: Idle::Zeros,
        volume_db: 0.0,
        plugins: Vec::new(),
        matrix: None,
    };
    let config = Config {
        outputs: vec![output],
        ..config(Idle::Off { delay_s: 1 })
    };
    let outputs = vec![FurtherSink {
        sink: Box::new(card),
        probe: None,
    }];
    let daemon = Daemon::start_outputs(config, sink, outputs);
    assert!(wait_for(|| log.closes() >= 1), "the master never went off");
    // The master's card is closed and gets nothing; the further card gets
    // zeros, as the master's own clock ran it.
    let (master, played) = (log.frames().len(), further.frames().len());
    thread::sleep(Duration::from_millis(500));
    assert_eq!(log.frames().len(), master, "the master still plays");
    let got = further.frames().len() - played;
    assert!(
        got >= 48000 * 4 / 10,
        "the further card got {got} frames in 0.5 s"
    );
    assert!(further.frames().iter().all(|f| *f == [0, 0]));
    assert_eq!(further.closes(), 0);
    // A player at the same rate opens the master's card under it, and the
    // further card plays on, never closed or opened again.
    let mut client = daemon.client_at(48000, 1 << 20);
    write_all(&mut client, &vec![[1 << 24, 1 << 24]; 96000]);
    client.start().unwrap();
    assert!(wait_for(|| log.frames().contains(&[1 << 24, 1 << 24])));
    assert_eq!(log.opens(), [(48000, PERIOD); 2]);
    assert!(wait_for(|| further.samples().iter().any(|&s| s != 0)));
    assert_eq!(further.opens().len(), 1);
    assert_eq!(further.closes(), 0);
}

#[test]
fn a_refused_sink_is_tried_again_only_when_the_card_list_changes() {
    let (sink, log) = memory_sink();
    log.fail(Some(SinkError::Refused));
    let changed = Arc::new(AtomicBool::new(false));
    let seen = changed.clone();
    // Only a card that is refusing is asked about changes.
    let probe = Box::new(move |refused: bool| refused && seen.load(Ordering::Relaxed));
    let daemon = Daemon::start_probing(default_config(), sink, probe);
    let mut client = daemon.client_at(48000, 262144);
    write_all(&mut client, &vec![[0x100, 0x100]; 192000]);
    client.start().unwrap();
    wait_played(&client, 4800);
    // The card would open now, but nothing changed: no try in seconds.
    log.fail(None);
    thread::sleep(Duration::from_millis(2500));
    assert!(log.opens().is_empty());
    changed.store(true, Ordering::Relaxed);
    assert!(wait_for(|| log.frames().contains(&[0x100, 0x100])));
}

#[test]
fn a_card_that_appears_later_brings_its_rates() {
    let (sink, log) = memory_sink();
    log.fail(Some(SinkError::Gone));
    let daemon = Daemon::start(default_config(), sink);
    let mut client = daemon.client_at(96000, 131072);
    write_all(&mut client, &vec![[0x100, 0x100]; 96000]);
    client.start().unwrap();
    // With no card, any rate goes, so the output follows the client.
    wait_played(&client, 9600);
    log.set_rates(vec![44100, 48000]);
    log.fail(None);
    assert!(wait_for(|| last_open(&log) == Some((48000, PERIOD))));
    // A client starting again at 96 kHz no longer takes the card there.
    client.stop().unwrap();
    client.start().unwrap();
    let opens = log.opens().len();
    thread::sleep(SETTLE * 2);
    assert!(log.opens()[opens..].iter().all(|&(rate, _)| rate != 96000));
    assert_eq!(last_open(&log), Some((48000, PERIOD)));
}

#[test]
fn a_dop_stream_restarting_does_not_change_the_rate() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut pcm = daemon.client_at(44100, 4096);
    pcm.start().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((44100, PERIOD))));
    let mut dop = daemon.client_at(176400, 262144);
    let frames: Vec<[i32; 2]> = (0..176400u32)
        .map(|i| {
            let marker: u32 = if i % 2 == 0 { 0x05 } else { 0xFA };
            let s = ((marker << 24) | 0x0012_3400) as i32;
            [s, s]
        })
        .collect();
    write_all(&mut dop, &frames);
    dop.start().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((176400, 256))));
    let opens = log.opens().len();
    dop.stop().unwrap();
    dop.start().unwrap();
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        log.opens().len(),
        opens,
        "a stop and start changed the rate"
    );
}

#[test]
fn a_second_short_drain_returns_promptly() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    // A large ring, so that the safety net would take about 6 s.
    let mut client = daemon.client_at(48000, 262144);
    write_all(&mut client, &left_pattern(1000));
    client.start().unwrap();
    client.drain().unwrap();
    // Under one period, then nothing at all.
    let short: Vec<[i32; 2]> = (1..=32).map(|i| [0, i << 12]).collect();
    for audio in [short.clone(), Vec::new()] {
        client.start().unwrap();
        write_all(&mut client, &audio);
        let started = Instant::now();
        client.drain().unwrap();
        let took = started.elapsed();
        assert!(took < Duration::from_secs(1), "the drain took {took:?}");
    }
    assert_eq!(client.hw_position().unwrap(), 1032);
    assert!(
        find(&log.frames(), &short).is_some(),
        "the short drain was not played"
    );
}

/// DoP frames carrying `payload` in the bits below the marker.
fn dop_frames(n: usize, payload: u32) -> Vec<[i32; 2]> {
    (0..n)
        .map(|i| {
            let marker: u32 = if i % 2 == 0 { 0x05 } else { 0xFA };
            let s = ((marker << 24) | payload) as i32;
            [s, s]
        })
        .collect()
}

fn carries(frame: &[i32; 2], payload: u32) -> bool {
    frame[0] as u32 & 0x00FF_FF00 == payload
}

const OLDER: u32 = 0x0012_3400;
const NEWER: u32 = 0x0056_7800;

/// The client configured first starts its DoP stream second, at `rate`. The
/// stream that was playing keeps the output and the rate.
fn oldest_dop_claim_keeps_the_output(rate: u32) {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut newer = daemon.client_at(rate, 262144);
    let mut older = daemon.client_at(176400, 262144);
    write_all(&mut older, &dop_frames(176400, OLDER));
    older.start().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((176400, 256))));
    assert!(wait_for(|| log.frames().iter().any(|f| carries(f, OLDER))));
    let opens = log.opens().len();

    write_all(&mut newer, &dop_frames(rate as usize, NEWER));
    newer.start().unwrap();
    // It is judged DoP and discarded from its first period on.
    wait_played(&newer, 1);
    let from = log.frames().len();
    thread::sleep(Duration::from_millis(300));
    let frames = log.frames();
    let after = &frames[from..];
    assert!(
        after.iter().any(|f| carries(f, OLDER)),
        "the playing stream lost the output"
    );
    assert!(
        !after.iter().any(|f| carries(f, NEWER)),
        "the newer claim was played"
    );
    assert_eq!(log.opens().len(), opens, "the output was reopened");
}

#[test]
fn the_oldest_dop_claim_keeps_the_output_at_another_rate() {
    oldest_dop_claim_keeps_the_output(88200);
}

#[test]
fn the_oldest_dop_claim_keeps_the_output_at_the_same_rate() {
    oldest_dop_claim_keeps_the_output(176400);
}

/// A seqpacket socket connected to `path`, with no client library.
fn raw_connect(path: &Path) -> OwnedFd {
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    assert!(raw >= 0);
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
        *dst = *src as libc::c_char;
    }
    let len = size_of::<libc::sockaddr_un>() as libc::socklen_t;
    let rc = unsafe { libc::connect(fd.as_raw_fd(), (&raw const addr).cast(), len) };
    assert_eq!(rc, 0);
    fd
}

#[test]
fn a_client_of_another_protocol_version_gets_an_error() {
    let (sink, _log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let fd = raw_connect(&daemon.path);
    let hello = proto::encode_request(&Request::Hello {
        version: proto::VERSION + 1,
    });
    let sent = unsafe {
        libc::send(
            fd.as_raw_fd(),
            hello.as_ptr().cast(),
            hello.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    assert_eq!(sent, hello.len() as isize);
    let mut reply = [0u8; 64];
    let n = unsafe { libc::recv(fd.as_raw_fd(), reply.as_mut_ptr().cast(), reply.len(), 0) };
    assert!(n > 0);
    assert!(matches!(
        proto::decode_reply(&reply[..n as usize]),
        Some(Reply::Error { .. })
    ));
    // Then the daemon hangs up.
    let n = unsafe { libc::recv(fd.as_raw_fd(), reply.as_mut_ptr().cast(), reply.len(), 0) };
    assert_eq!(n, 0);
    // A client of this version is still served.
    let mut client = daemon.client_at(48000, 1024);
    client.start().unwrap();
}

/// Sends one request on a raw connection.
fn send_raw(fd: &OwnedFd, request: &Request) {
    let message = proto::encode_request(request);
    let sent = unsafe {
        libc::send(
            fd.as_raw_fd(),
            message.as_ptr().cast(),
            message.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    assert_eq!(sent, message.len() as isize);
}

/// The next message on a raw connection, or None if none comes in time.
fn receive_raw(fd: &OwnedFd, timeout: Duration) -> Option<Reply> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
    if rc <= 0 {
        return None;
    }
    let mut reply = [0u8; 64];
    let n = unsafe { libc::recv(fd.as_raw_fd(), reply.as_mut_ptr().cast(), reply.len(), 0) };
    assert!(n > 0, "the daemon hung up");
    proto::decode_reply(&reply[..n as usize])
}

/// A raw connection that has said hello.
fn volume_client(daemon: &Daemon) -> OwnedFd {
    let fd = raw_connect(&daemon.path);
    send_raw(
        &fd,
        &Request::Hello {
            version: proto::VERSION,
        },
    );
    let said = Reply::Hello {
        version: proto::VERSION,
    };
    assert_eq!(receive_raw(&fd, PATIENCE), Some(said));
    fd
}

fn call_raw(fd: &OwnedFd, request: &Request) -> Option<Reply> {
    send_raw(fd, request);
    receive_raw(fd, PATIENCE)
}

#[test]
fn a_volume_set_by_one_client_reaches_a_subscriber_and_survives_a_restart() {
    let keep = tempfile::tempdir().unwrap();
    let state = keep.path().join("state.toml");
    {
        let (sink, _log) = memory_sink();
        let dir = tempfile::tempdir().unwrap();
        let daemon = Daemon::start_with(default_config(), sink, dir, state.clone());
        let watcher = volume_client(&daemon);
        // A fresh install starts at 0 dB.
        let subscribed = call_raw(&watcher, &Request::SubscribeVolume);
        assert_eq!(subscribed, Some(Reply::Volume { percent: 100 }));
        let setter = volume_client(&daemon);
        let set = call_raw(&setter, &Request::SetVolume { percent: 40 });
        assert_eq!(set, Some(Reply::Ok));
        let notice = receive_raw(&watcher, PATIENCE);
        assert_eq!(notice, Some(Reply::VolumeChanged { percent: 40 }));
        // The same value again is no change, and over 100 is refused.
        let same = call_raw(&setter, &Request::SetVolume { percent: 40 });
        assert_eq!(same, Some(Reply::Ok));
        let over = call_raw(&setter, &Request::SetVolume { percent: 101 });
        assert!(matches!(over, Some(Reply::Error { .. })));
        assert_eq!(receive_raw(&watcher, Duration::from_millis(200)), None);
        // A subscriber can still ask and set.
        let got = call_raw(&watcher, &Request::GetVolume);
        assert_eq!(got, Some(Reply::Volume { percent: 40 }));
        let set = call_raw(&watcher, &Request::SetVolume { percent: 45 });
        assert_eq!(set, Some(Reply::Ok));
        let own = receive_raw(&watcher, PATIENCE);
        assert_eq!(own, Some(Reply::VolumeChanged { percent: 45 }));
        let got = call_raw(&setter, &Request::GetVolume);
        assert_eq!(got, Some(Reply::Volume { percent: 45 }));
    }
    let (sink, _log) = memory_sink();
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start_with(default_config(), sink, dir, state);
    let client = volume_client(&daemon);
    let got = call_raw(&client, &Request::GetVolume);
    assert_eq!(got, Some(Reply::Volume { percent: 45 }));
}

#[test]
fn software_volume_scales_the_output() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let control = volume_client(&daemon);
    let set = call_raw(&control, &Request::SetVolume { percent: 50 });
    assert_eq!(set, Some(Reply::Ok));
    let mut client = daemon.client_at(48000, 4096);
    let frames = vec![[1 << 30, -(1 << 30)]; 4096];
    client.start().unwrap();
    write_all(&mut client, &frames);
    client.drain().unwrap();
    // -30 dB of 2^30, as f32 mixing rounds it.
    let want = (f64::from(1 << 30) * 10f64.powf(-30.0 / 20.0)) as i32;
    let played = log.frames();
    let hit = played
        .iter()
        .any(|f| (f[0] - want).abs() < 64 && (f[1] + want).abs() < 64);
    assert!(hit, "no frame at -30 dB");
    assert!(!played.contains(&[1 << 30, -(1 << 30)]));
}

#[test]
fn a_subscriber_that_does_not_read_keeps_its_connection_and_gets_the_last_value() {
    let (sink, _log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let watcher = volume_client(&daemon);
    let subscribed = call_raw(&watcher, &Request::SubscribeVolume);
    assert_eq!(subscribed, Some(Reply::Volume { percent: 100 }));
    // Far more changes than the watcher's socket could hold, unread.
    let setter = volume_client(&daemon);
    let last = 33;
    for i in 0..3000u32 {
        let percent = if i == 2999 { last } else { (i % 100) as u8 };
        let set = call_raw(&setter, &Request::SetVolume { percent });
        assert_eq!(set, Some(Reply::Ok));
    }
    // The watcher gets one notification sent before it read, then the
    // latest, and nothing more.
    let mut notices = Vec::new();
    while let Some(reply) = receive_raw(&watcher, Duration::from_secs(1)) {
        notices.push(reply);
    }
    let (count, newest) = (notices.len(), notices.last());
    assert!(count <= 2, "{count} notifications, the last {newest:?}");
    assert_eq!(
        notices.last(),
        Some(&Reply::VolumeChanged { percent: last })
    );
    // Its connection still answers.
    let got = call_raw(&watcher, &Request::GetVolume);
    assert_eq!(got, Some(Reply::Volume { percent: last }));
}

#[test]
fn a_drain_started_without_waiting_finishes_and_a_stop_cancels_it() {
    let (sink, _log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut client = daemon.client_at(48000, 4096);
    write_all(&mut client, &left_pattern(4096));
    client.start_drain().unwrap();
    // 4096 frames take 85 ms, so it cannot be done yet.
    assert!(!client.poll_drain().unwrap());
    assert!(wait_for(|| client.poll_drain().unwrap()));
    assert_eq!(client.hw_position().unwrap(), 4096);
    // A stop while a drain waits cancels it, and gets its own reply.
    write_all(&mut client, &left_pattern(4096));
    client.start_drain().unwrap();
    client.stop().unwrap();
    assert!(client.poll_drain().unwrap());
    // The replies that follow still belong to their requests.
    client.start().unwrap();
    client.drain().unwrap();
    assert_eq!(client.hw_position().unwrap(), 8192);
}

/// A sink whose first write panics, as a bug on the audio thread would.
struct PanickingSink;

impl Sink for PanickingSink {
    fn open(&mut self, _rate: u32, _period: u32) -> Result<(), SinkError> {
        Ok(())
    }

    fn write(&mut self, _samples: &[i32]) -> Result<(), SinkError> {
        panic!("a test sink that panics on write");
    }

    fn channels(&self) -> usize {
        2
    }

    fn close(&mut self) {}

    fn delay_frames(&self) -> u32 {
        0
    }

    fn supported_rates(&self) -> Vec<u32> {
        Vec::new()
    }
}

#[test]
fn a_panic_on_the_audio_thread_stops_the_daemon_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aas.sock");
    let state = dir.path().join("state.toml");
    let shutdown = Arc::new(AtomicBool::new(false));
    let (done, finished) = std::sync::mpsc::channel();
    let flag = shutdown.clone();
    thread::spawn(move || {
        let result = server::run(
            default_config(),
            Box::new(PanickingSink),
            &path,
            &state,
            None,
            None,
            flag,
        );
        let _ = done.send(result);
    });
    // The output opens at the start, so the first write comes at once.
    let result = finished.recv_timeout(PATIENCE);
    shutdown.store(true, Ordering::Relaxed);
    let result = result.expect("the daemon kept running without its audio thread");
    assert!(result.is_err(), "{result:?}");
}

/// The dB values one opening of a fake card's volume control was set to.
type Mixer = Arc<Mutex<Vec<f32>>>;

/// A card's volume control that records what it is set to.
struct FakeVolume(Mixer);

impl HardwareVolume for FakeVolume {
    fn set_db(&mut self, db: f32) {
        self.0.lock().unwrap().push(db);
    }

    fn range_db(&self) -> (f32, f32) {
        (-103.5, 0.0)
    }

    fn can_mute(&self) -> bool {
        true
    }
}

/// Opens a fake control while `present` is set, and keeps every opening.
fn fake_volume(present: Arc<AtomicBool>) -> (OpenVolume, Arc<Mutex<Vec<Mixer>>>) {
    let mixers = Arc::new(Mutex::new(Vec::new()));
    let opened = mixers.clone();
    let open: OpenVolume = Box::new(move || {
        if !present.load(Ordering::Relaxed) {
            return None;
        }
        let mixer = Mixer::default();
        opened.lock().unwrap().push(mixer.clone());
        Some(Box::new(FakeVolume(mixer)) as Box<dyn HardwareVolume>)
    });
    (open, mixers)
}

fn last_set(mixers: &Arc<Mutex<Vec<Mixer>>>) -> Option<f32> {
    let mixers = mixers.lock().unwrap();
    mixers.last()?.lock().unwrap().last().copied()
}

#[test]
fn a_card_missing_at_the_start_gets_the_volume_when_it_appears() {
    let (sink, log) = memory_sink();
    log.fail(Some(SinkError::Gone));
    let present = Arc::new(AtomicBool::new(false));
    let (open, mixers) = fake_volume(present.clone());
    let daemon = Daemon::start_with_volume(default_config(), sink, open);
    let control = volume_client(&daemon);
    let set = call_raw(&control, &Request::SetVolume { percent: 50 });
    assert_eq!(set, Some(Reply::Ok));
    assert!(mixers.lock().unwrap().is_empty());
    present.store(true, Ordering::Relaxed);
    log.fail(None);
    assert!(wait_for(|| last_set(&mixers) == Some(-30.0)));
    // The card's control carries the volume now, so the samples pass as
    // they are.
    let mut client = daemon.client_at(48000, 4096);
    let frames = vec![[1 << 30, -(1 << 30)]; 4096];
    client.start().unwrap();
    write_all(&mut client, &frames);
    client.drain().unwrap();
    assert!(log.frames().contains(&[1 << 30, -(1 << 30)]));
}

#[test]
fn a_card_that_comes_back_gets_the_volume_again() {
    let (sink, log) = memory_sink();
    let present = Arc::new(AtomicBool::new(true));
    let (open, mixers) = fake_volume(present.clone());
    let daemon = Daemon::start_with_volume(default_config(), sink, open);
    // The card is lost only once the output has opened on it.
    assert!(wait_for(|| last_open(&log).is_some()));
    let control = volume_client(&daemon);
    let set = call_raw(&control, &Request::SetVolume { percent: 40 });
    assert_eq!(set, Some(Reply::Ok));
    assert_eq!(last_set(&mixers), Some(-36.0));
    present.store(false, Ordering::Relaxed);
    log.fail(Some(SinkError::Gone));
    // The old control is let go, as a card being removed waits for that.
    let old = mixers.lock().unwrap().last().unwrap().clone();
    assert!(wait_for(|| Arc::strong_count(&old) == 2));
    let before = mixers.lock().unwrap().len();
    present.store(true, Ordering::Relaxed);
    log.fail(None);
    // The new control is listed when it is opened, and set just after:
    // wait for the setting, as the test before does.
    assert!(wait_for(|| mixers.lock().unwrap().len() > before));
    assert!(wait_for(|| last_set(&mixers) == Some(-36.0)));
}

/// How often the wake-up was written since it was last read, and whether
/// it is readable within `timeout`.
fn wake_ups(client: &Client, timeout: Duration) -> u64 {
    let mut pfd = libc::pollfd {
        fd: client.poll_fds()[0],
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd; then 8 bytes from a non-blocking eventfd.
    unsafe {
        if libc::poll(&mut pfd, 1, timeout.as_millis() as i32) != 1 {
            return 0;
        }
        let mut count = 0u64;
        libc::read(pfd.fd, (&raw mut count).cast(), 8);
        count
    }
}

#[test]
fn a_client_waiting_for_a_position_is_woken_once_when_it_is_reached() {
    let (sink, _log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut client = daemon.client_at(48000, 8192);
    write_all(&mut client, &left_pattern(8192));
    // Many periods away, so a wake-up after each would come far earlier.
    assert!(!client.wake_at(6000).unwrap());
    client.start().unwrap();
    assert_eq!(wake_ups(&client, PATIENCE), 1);
    let position = client.hw_position().unwrap();
    assert!(position >= 6000, "woken at {position}");
    // The position has had its wake-up: no more come while audio is left.
    assert!(position < 8192 - 1024, "too late to tell: {position}");
    assert_eq!(wake_ups(&client, Duration::from_millis(10)), 0);
    // A position already reached is told at once, and need not wake.
    assert!(client.wake_at(100).unwrap());
}

#[test]
fn a_client_waiting_for_a_position_is_woken_by_an_underrun() {
    let (sink, _log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut client = daemon.client_at(48000, 4096);
    write_all(&mut client, &left_pattern(2048));
    // Never reached: the stream runs dry first.
    assert!(!client.wake_at(1 << 40).unwrap());
    client.start().unwrap();
    assert!(wake_ups(&client, PATIENCE) >= 1);
    assert!(client.underruns() >= 1);
    assert_eq!(client.hw_position().unwrap(), 2048);
}

#[test]
fn without_a_position_the_client_is_woken_after_every_period() {
    let (sink, _log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut client = daemon.client_at(48000, 8192);
    write_all(&mut client, &left_pattern(8192));
    client.start().unwrap();
    assert!(wake_ups(&client, PATIENCE) >= 1);
    assert!(client.hw_position().unwrap() < 4096);
}

#[test]
fn a_reset_empties_the_ring_and_the_stream_plays_on_in_it() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    let mut client = daemon.client_at(48000, 8192);
    // Reset while playing: what is left is dropped, not played.
    write_all(&mut client, &left_pattern(8192));
    client.start().unwrap();
    wait_played(&client, 1024);
    client.reset().unwrap();
    let base = client.hw_position().unwrap();
    assert_eq!(base, 8192, "the ring is empty after a reset");
    assert_eq!(client.delay_frames(), 0);
    thread::sleep(Duration::from_millis(50));
    let dropped = log.frames().iter().filter(|f| f[0] != 0).count();
    assert!(dropped < 8192, "all of the old audio played");
    // The same ring takes the next stream, from its first frame.
    let next = right_pattern(4096);
    write_all(&mut client, &next);
    client.start().unwrap();
    client.drain().unwrap();
    assert_eq!(client.hw_position().unwrap(), base + 4096);
    let played = log.frames();
    let at = find(&played, &next).expect("the new stream played whole");
    assert!(played[at..].iter().all(|f| f[0] == 0), "old audio after it");
    // A reset of a stopped stream with frames in it, and one of an empty one.
    write_all(&mut client, &left_pattern(1000));
    client.reset().unwrap();
    assert_eq!(client.hw_position().unwrap(), base + 5096);
    client.reset().unwrap();
    assert_eq!(client.hw_position().unwrap(), base + 5096);
}

#[test]
fn a_reset_gives_a_resampled_stream_an_empty_filter() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    // The first client sets the rate, so the second is resampled.
    let mut first = daemon.client_at(48000, 8192);
    first.start().unwrap();
    let mut second = daemon.client_at(44100, 8192);
    write_all(&mut second, &left_pattern(8192));
    second.start().unwrap();
    wait_played(&second, 2048);
    second.reset().unwrap();
    // What the filter still held must not come out with the next stream,
    // which is on the other channel.
    thread::sleep(Duration::from_millis(50));
    let before = log.frames().len();
    write_all(&mut second, &right_pattern(4096));
    second.start().unwrap();
    second.drain().unwrap();
    let after = log.frames();
    assert!(after[before..].iter().any(|f| f[1] != 0), "nothing played");
    assert!(
        after[before..].iter().all(|f| f[0] == 0),
        "the filter's old audio played after the reset"
    );
}

fn analog_config() -> Config {
    Config {
        analog: Analog {
            enabled: true,
            volume_db: 0.0,
        },
        ..default_config()
    }
}

#[test]
fn the_analog_input_plays_through_the_daemon() {
    let (sink, log) = memory_sink();
    let feeds: Arc<Mutex<Vec<(u32, u32, CaptureFeed)>>> = Arc::default();
    let opened = feeds.clone();
    let capture: SendCapture = Box::new(move |rate, period| {
        let (capture, feed) = MemoryCapture::new();
        opened.lock().unwrap().push((rate, period, feed));
        Ok(Box::new(capture) as Box<dyn Capture>)
    });
    let setup: CaptureSetup = Box::new(move |_| Some(capture as OpenCapture));
    let _daemon = Daemon::start_analog(analog_config(), sink, setup);
    // The output opens without a client, and the capture after it.
    assert!(wait_for(|| !feeds.lock().unwrap().is_empty()));
    let (rate, period, feed) = feeds.lock().unwrap()[0].clone();
    assert_eq!((rate, period), (48000, PERIOD));
    assert_eq!(log.opens(), [(48000, PERIOD)]);
    // A signal at -18 dBFS, delivered as a card would: a period or two
    // queued at a time.
    let v = 1 << 28;
    let done = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (feed, done) = (feed.clone(), done.clone());
        thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if feed.queued() < 2 * PERIOD as usize {
                    feed.push(&[[v, -v]; PERIOD as usize]);
                }
                thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let played = wait_for(|| log.frames().contains(&[v, -v]));
    done.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    assert!(played, "the input never reached the card");
    // It was never closed or opened again.
    assert!(!feed.closed());
    assert_eq!(feeds.lock().unwrap().len(), 1);
}

#[test]
fn a_plugin_that_cannot_be_loaded_stops_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aas.sock");
    let state = dir.path().join("state.toml");
    let missing = "/nonexistent/libaas_nothing.so";
    let config = Config {
        plugins: vec![PluginConfig {
            place: PluginPlace::Output,
            kind: "library".into(),
            path: Some(missing.into()),
            params: Default::default(),
        }],
        ..default_config()
    };
    let (sink, log) = memory_sink();
    // Set at once, so that a daemon that starts after all stops again.
    let shutdown = Arc::new(AtomicBool::new(true));
    let result = server::run(config, sink, &path, &state, None, None, shutdown);
    let e = result.expect_err("the daemon started");
    assert!(e.contains(missing), "{e}");
    assert!(log.opens().is_empty());
    assert!(!path.exists());
}

#[test]
fn a_matrix_route_above_the_cards_channels_stops_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aas.sock");
    let state = dir.path().join("state.toml");
    let config = Config {
        matrix: Some(vec![aas::config::Route {
            from: 0,
            to: 3,
            gain_db: 0.0,
        }]),
        ..default_config()
    };
    let (sink, log) = memory_sink();
    let shutdown = Arc::new(AtomicBool::new(true));
    let result = server::run(config, sink, &path, &state, None, None, shutdown);
    let e = result.expect_err("the daemon started");
    assert!(e.contains("matrix 1") && e.contains("channel 3"), "{e}");
    assert!(log.opens().is_empty());
    assert!(!path.exists());
}

#[test]
fn an_enabled_input_on_a_card_without_an_adc_opens_nothing() {
    let config = Config {
        idle: Idle::Off { delay_s: 1 },
        ..analog_config()
    };
    // Only an enabled input counts.
    let refused: OpenCapture = Box::new(|_, _| Err("unused".into()));
    let adc = "snd_rpi_hifiberry_dacplusadc";
    assert!(server::analog_capture(&default_config(), adc, refused).is_none());
    // A daemon on a card with this driver, with an opener that records its
    // calls and fails, so that the output is left alone.
    let start = |driver: &'static str| {
        let asked: Arc<Mutex<Vec<(u32, u32)>>> = Arc::default();
        let calls = asked.clone();
        let open: SendCapture = Box::new(move |rate, period| {
            calls.lock().unwrap().push((rate, period));
            Err("no capture in this test".into())
        });
        let setup: CaptureSetup =
            Box::new(move |config| server::analog_capture(config, driver, open as OpenCapture));
        let (sink, log) = memory_sink();
        let daemon = Daemon::start_analog(config.clone(), sink, setup);
        (daemon, log, asked)
    };
    // A Digi and an unknown card have no ADC: no capture is opened, and
    // the output goes off when idle as without the input.
    for driver in ["snd_rpi_hifiberry_digi", "snd_some_other_card"] {
        let (daemon, log, asked) = start(driver);
        assert!(wait_for(|| log.closes() >= 1), "{driver}: never went off");
        assert!(asked.lock().unwrap().is_empty(), "{driver}");
        drop(daemon);
    }
    // A card with an ADC gets its capture opened once the output is open.
    let (daemon, log, asked) = start(adc);
    assert!(wait_for(|| !asked.lock().unwrap().is_empty()));
    assert_eq!(asked.lock().unwrap()[0], (48000, PERIOD));
    assert_eq!(log.closes(), 0);
    drop(daemon);
}

/// Reads what the recorder's ring holds, until `want` frames have come or
/// the patience runs out.
fn record(recorder: &mut Client, want: usize) -> Vec<[i32; 2]> {
    let mut got = Vec::new();
    let mut buf = [[0i32; 2]; 1024];
    let deadline = Instant::now() + PATIENCE;
    while got.len() < want && Instant::now() < deadline {
        let room = buf.len().min(want - got.len());
        let n = recorder.read(&mut buf[..room]).unwrap();
        got.extend_from_slice(&buf[..n]);
        if n == 0 {
            thread::sleep(Duration::from_millis(2));
        }
    }
    got
}

#[test]
fn a_recorder_reads_what_the_capture_delivers() {
    let (sink, log) = memory_sink();
    let feeds: Arc<Mutex<Vec<CaptureFeed>>> = Arc::default();
    let opened = feeds.clone();
    let capture: SendCapture = Box::new(move |_, _| {
        let (capture, feed) = MemoryCapture::new();
        opened.lock().unwrap().push(feed);
        Ok(Box::new(capture) as Box<dyn Capture>)
    });
    let setup: CaptureSetup = Box::new(move |_| Some(capture as OpenCapture));
    let daemon = Daemon::start_analog(analog_config(), sink, setup);
    assert!(wait_for(|| !feeds.lock().unwrap().is_empty()));
    let feed = feeds.lock().unwrap()[0].clone();
    let mut recorder = daemon.client();
    assert_eq!(recorder.configure_capture(4096).unwrap(), 48000);
    // Nothing is written before the start.
    thread::sleep(Duration::from_millis(20));
    let mut buf = [[0i32; 2]; 1024];
    assert_eq!(recorder.read(&mut buf).unwrap(), 0);
    assert_eq!(recorder.hw_position().unwrap(), 0);
    recorder.start().unwrap();
    // Silence first: the input has no signal yet.
    let quiet = record(&mut recorder, 4 * PERIOD as usize);
    assert!(
        quiet.iter().all(|f| *f == [0, 0]),
        "audio before any signal"
    );
    // A signal at -18 dBFS, delivered as a card would.
    let v = 1 << 28;
    let done = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (feed, done) = (feed.clone(), done.clone());
        thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if feed.queued() < 2 * PERIOD as usize {
                    feed.push(&[[v, -v]; PERIOD as usize]);
                }
                thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let mut heard = false;
    let deadline = Instant::now() + PATIENCE;
    while !heard && Instant::now() < deadline {
        heard = record(&mut recorder, PERIOD as usize).contains(&[v, -v]);
    }
    done.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    assert!(heard, "the input never reached the recorder");
    assert!(
        log.frames().contains(&[v, -v]),
        "the input never reached the card"
    );
    // A reset empties the ring from the reader's side and stops the stream.
    recorder.reset().unwrap();
    let at = recorder.hw_position().unwrap();
    thread::sleep(Duration::from_millis(20));
    assert_eq!(recorder.hw_position().unwrap(), at);
    assert_eq!(recorder.read(&mut buf).unwrap(), 0);
    assert_eq!(recorder.delay_frames(), 0);
    recorder.start().unwrap();
    assert_eq!(record(&mut recorder, 64).len(), 64);
    assert!(!recorder.ended());
}

#[test]
fn a_rate_change_ends_an_open_capture_stream() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(default_config(), sink);
    assert!(wait_for(|| last_open(&log) == Some((48000, PERIOD))));
    let mut recorder = daemon.client();
    assert_eq!(recorder.configure_capture(4096).unwrap(), 48000);
    recorder.start().unwrap();
    // Without an input it records silence in real time.
    assert_eq!(record(&mut recorder, 256).len(), 256);
    // A player at the same rate leaves it alone.
    let mut same = daemon.client_at(48000, 4096);
    write_all(&mut same, &left_pattern(1024));
    same.start().unwrap();
    wait_played(&same, 1024);
    assert!(!recorder.ended());
    same.stop().unwrap();
    // One at another rate takes the output there, and ends the recorder.
    let mut player = daemon.client_at(44100, 4096);
    player.start().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((44100, PERIOD))));
    assert!(wait_for(|| recorder.ended()));
    // Once it reads ended, nothing more is written.
    let at = recorder.hw_position().unwrap();
    thread::sleep(Duration::from_millis(20));
    assert_eq!(recorder.hw_position().unwrap(), at);
    let start = recorder.start().err().and_then(|e| e.raw_os_error());
    assert_eq!(start, Some(libc::ENODEV));
    // What it had recorded can still be read.
    let mut buf = [[0i32; 2]; 1024];
    while recorder.read(&mut buf).unwrap() > 0 {}
    assert_eq!(recorder.hw_position().unwrap(), at);
    // Reopened, it records at the new rate.
    assert_eq!(recorder.configure_capture(4096).unwrap(), 44100);
    recorder.start().unwrap();
    assert_eq!(record(&mut recorder, 256).len(), 256);
    assert!(!recorder.ended());
}

#[test]
fn a_recorder_does_not_count_as_a_client() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(config(Idle::Off { delay_s: 1 }), sink);
    let mut recorder = daemon.client();
    assert_eq!(recorder.configure_capture(0).unwrap(), 48000);
    recorder.start().unwrap();
    // The smallest ring is four periods. One that is not read fills, and
    // counts what it could not take.
    assert!(wait_for(|| recorder.underruns() > 0));
    let mut buf = [[0i32; 2]; 1024];
    assert_eq!(recorder.read(&mut buf).unwrap(), 4 * PERIOD as usize);
    // It does not keep the output open.
    assert!(
        wait_for(|| log.closes() >= 1),
        "the recorder kept the output on"
    );
    // Configured while the output is off, it gets the rate it will open at.
    let mut second = daemon.client();
    assert_eq!(second.configure_capture(4096).unwrap(), 48000);
    // It does not hold the rate: the first player sets it.
    let mut player = daemon.client_at(44100, 4096);
    player.start().unwrap();
    assert!(wait_for(|| last_open(&log) == Some((44100, PERIOD))));
    assert!(wait_for(|| recorder.ended() && second.ended()));
}

#[test]
fn a_recorder_keeps_its_stream_when_the_output_reopens_at_its_rate() {
    let (sink, log) = memory_sink();
    let daemon = Daemon::start(config(Idle::Off { delay_s: 1 }), sink);
    let mut recorder = daemon.client();
    assert_eq!(recorder.configure_capture(4096).unwrap(), 48000);
    recorder.start().unwrap();
    assert_eq!(record(&mut recorder, 256).len(), 256);
    // The output goes off when idle, and the recorder gets nothing then.
    assert!(wait_for(|| log.closes() >= 1), "the output never went off");
    let mut buf = [[0i32; 2]; 1024];
    while recorder.read(&mut buf).unwrap() > 0 {}
    let at = recorder.hw_position().unwrap();
    thread::sleep(Duration::from_millis(20));
    assert_eq!(recorder.hw_position().unwrap(), at);
    // A player at the same rate opens it again: the stream goes on.
    let mut player = daemon.client_at(48000, 4096);
    player.start().unwrap();
    assert!(wait_for(|| log.opens().len() >= 2));
    assert_eq!(last_open(&log), Some((48000, PERIOD)));
    assert_eq!(record(&mut recorder, 256).len(), 256);
    assert!(recorder.hw_position().unwrap() > at);
    assert!(!recorder.ended());
}
