//! The daemon end to end: clients on a real socket, the output a `MemorySink`.

use aas::config::{Config, Idle};
use aas::rate::SETTLE;
use aas::server;
use aas::sink::{MemorySink, Sink, SinkError, SinkLog};
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

struct Daemon {
    _dir: tempfile::TempDir,
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl Daemon {
    fn start(config: Config, sink: Box<dyn Sink>) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aas.sock");
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let (path, shutdown) = (path.clone(), shutdown.clone());
            thread::spawn(move || server::run(config, sink, &path, shutdown))
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

    fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
        self.inner.write(frames)
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
