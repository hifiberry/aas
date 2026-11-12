//! The daemon's socket and threads. The control thread runs here, polling
//! the listener, the clients, the audio thread's events and a 50 ms timer.

use crate::board;
use crate::cards;
use crate::config::{BitPerfect, Config, Format, PluginPlace};
use crate::control::{Answer, ConnId, Control, FurtherSetup, OpenCapture, eventfd, feed_capacity};
use crate::counters::{Counters, OutputCounters};
use crate::engine::{Engine, MAX_DELAY};
use crate::format::{F32, F64, I32, Processing};
use crate::further;
use crate::matrix::{INPUT_NAMES, Matrix};
use crate::plugin::Failure;
use crate::plugin::build::{Chains, Libraries, build_chain, matrix_width, widths};
use crate::rate::STANDARD_RATES;
use crate::rt;
use crate::sink::{CardSearch, CardWatch, FallbackSink, RETRY, RatesWatch, Sink};
use crate::volume::{OpenVolume, OutputVolume};
use aas_client::client::socket_address;
use aas_client::proto::{self, REQUEST_LEN, Reply};
use rtrb::RingBuffer;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// How often the control thread looks at the clock with nothing else to do.
const TICK_MS: i32 = 50;
/// How long the audio thread waits for a command while the output is closed.
const PARK: Duration = Duration::from_millis(100);
/// How long the listener is left alone after an accept that failed for
/// want of a descriptor or memory.
const ACCEPT_PAUSE: Duration = Duration::from_millis(250);
const COMMAND_SLOTS: usize = 4096;
const EVENT_SLOTS: usize = 1024;

fn check(rc: isize) -> io::Result<usize> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}

/// Says whether the audio thread should try the sink again. Called on the
/// control thread, once a second while the sink is gone, with `false`: is
/// the card present. While the card refuses the configuration it is called
/// with `true`: has the set of cards changed since last time, so that a
/// card unplugged and plugged in again, or another one, is tried once, and
/// the refusal is not retried every second.
pub type CardProbe = Box<dyn FnMut(bool) -> bool + Send>;

/// The card of a further output, and how to look for it while it is gone.
/// Without a probe it is looked for every second.
pub struct FurtherSink {
    pub sink: Box<dyn Sink>,
    pub probe: Option<CardProbe>,
}

/// The cards the daemon plays to: the master, how to look for it while it
/// is gone, and one further card for each `[[output]]` table, in order.
pub struct Devices {
    pub sink: Box<dyn Sink>,
    pub probe: Option<CardProbe>,
    pub further: Vec<FurtherSink>,
}

/// Runs the daemon as `run_probing` does, with no probe for the card: the
/// audio thread then tries a sink that is gone every second by itself.
pub fn run(
    config: Config,
    sink: Box<dyn Sink>,
    socket: &Path,
    state: &Path,
    hardware: Option<OpenVolume>,
    capture: Option<OpenCapture>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), String> {
    run_probing(
        config, sink, socket, state, hardware, None, capture, shutdown,
    )
}

/// The analog input's capture, if the config enables the input and the
/// card, by its driver name, has an ADC. Logs one line when the input is
/// enabled, whether it runs or not.
pub fn analog_capture(config: &Config, driver: &str, open: OpenCapture) -> Option<OpenCapture> {
    if !config.analog.enabled {
        return None;
    }
    if !cards::has_adc(driver) {
        eprintln!("aas: the card {driver} has no analog input aas knows, it stays off");
        return None;
    }
    eprintln!("aas: analog input on, from the card {driver}");
    Some(open)
}

/// Runs the daemon until `shutdown` is set. The sink is wrapped in a
/// `FallbackSink`, so a missing card does not stop playback. The output
/// volume is kept in the file `state`. It is set on the control that
/// `hardware` opens, if given and found, else in software. `hardware` is
/// called again each time the card goes or comes back.
///
/// With a `probe`, the control thread looks for a card that is gone and
/// has the audio thread try the sink only once the card is there. So the
/// audio thread neither reads the list of cards nor allocates while it
/// waits for one.
///
/// With a `capture`, the analog input runs; `analog_capture` decides
/// whether there is one. Every plugin library is loaded and every chain
/// built before anything starts, so a plugin that fails stops the start.
#[allow(clippy::too_many_arguments)]
pub fn run_probing(
    config: Config,
    sink: Box<dyn Sink>,
    socket: &Path,
    state: &Path,
    hardware: Option<OpenVolume>,
    probe: Option<CardProbe>,
    capture: Option<OpenCapture>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), String> {
    let devices = Devices {
        sink,
        probe,
        further: Vec::new(),
    };
    run_outputs(config, devices, socket, state, hardware, capture, shutdown)
}

/// Runs the daemon as `run_probing` says, playing to every card of
/// `devices`. Each further card has its writer thread, started before the
/// audio thread, and is looked for while it is gone as the master is.
pub fn run_outputs(
    config: Config,
    devices: Devices,
    socket: &Path,
    state: &Path,
    mut hardware: Option<OpenVolume>,
    capture: Option<OpenCapture>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), String> {
    let Devices {
        sink,
        probe,
        further,
    } = devices;
    if further.len() != config.outputs.len() {
        return Err(format!(
            "{} [[output]] tables, but {} cards to play them",
            config.outputs.len(),
            further.len()
        ));
    }
    raise_file_limit();
    let latency_ms = config
        .latency_ms
        .unwrap_or_else(|| board::detect().default_latency_ms());
    // Before the socket, so that a failure here leaves no socket file.
    let supported = sink.supported_rates();
    let others = if supported.is_empty() {
        &STANDARD_RATES[..]
    } else {
        &supported[..]
    };
    let mut libraries = Libraries::default();
    let matrix = build_matrix(&config, &mut libraries)?;
    let chains = Chains::build_for_start(
        &config.plugins,
        config.default_rate,
        others,
        config.format,
        config.channels,
        &mut libraries,
    )?;
    check_forced(&config, &mut libraries)?;
    let matrices = check_outputs(&config, &further, &mut libraries)?;
    eprintln!("aas: matrix: {}", describe(&matrix));
    for (i, matrix) in matrices.iter().enumerate() {
        eprintln!("aas: output {} matrix: {}", i + 2, describe(matrix));
    }
    let notify = eventfd().map_err(|e| format!("eventfd: {e}"))?;
    let listener = listen(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let mut sink = FallbackSink::new(sink);
    let rates = sink.rates_watch();
    let card = sink.card_watch();
    let search = probe.map(|probe| (sink.card_search(), probe));
    // The writers start before the audio thread, which feeds them.
    let capacity = feed_capacity(latency_ms, config.resampling);
    let mut setups = Vec::new();
    let mut searches = Vec::new();
    let mut watches = Vec::new();
    let mut output_counters = Vec::new();
    let tables = config.outputs.iter().zip(matrices).zip(further);
    for (i, ((output, matrix), card)) in tables.enumerate() {
        let index = i + 1;
        let mut card_sink = FallbackSink::buffering(card.sink, further::BUFFERED);
        let rates = card_sink.supported_rates();
        watches.push((index, card_sink.rates_watch()));
        let counters = Arc::new(OutputCounters::default());
        let channels = output.channels;
        let (feed, handle) = further::spawn(index, card_sink, capacity, channels, counters.clone());
        searches.push(FurtherSearch {
            search: handle.search(),
            probe: card.probe,
            wait: ProbeWait::default(),
        });
        output_counters.push((index, counters));
        setups.push(FurtherSetup {
            index,
            config: output.clone(),
            matrix,
            rates,
            writer: Box::new(handle),
            feed,
            feed_frames: capacity,
        });
    }
    let (commands, command_queue) = RingBuffer::new(COMMAND_SLOTS);
    let (event_queue, events) = RingBuffer::new(EVENT_SLOTS);
    let counters = Arc::new(Counters::default());
    let engine_counters = counters.clone();
    let mode = config.bit_perfect;
    let wake = notify.as_raw_fd();
    let spawned = thread::Builder::new().name("aas-audio".into());
    // The master's delay line keeps it in time with further outputs.
    let delay = if config.outputs.is_empty() {
        0
    } else {
        MAX_DELAY as usize
    };
    let audio = match config.format {
        Format::I32 => {
            let engine = Engine::<I32, _>::new(
                sink,
                command_queue,
                event_queue,
                engine_counters,
                mode,
                matrix,
                delay,
            );
            spawned.spawn(move || audio_loop(engine, wake))
        }
        Format::F32 => {
            let engine = Engine::<F32, _>::new(
                sink,
                command_queue,
                event_queue,
                engine_counters,
                mode,
                matrix,
                delay,
            );
            spawned.spawn(move || audio_loop(engine, wake))
        }
        Format::F64 => {
            let engine = Engine::<F64, _>::new(
                sink,
                command_queue,
                event_queue,
                engine_counters,
                mode,
                matrix,
                delay,
            );
            spawned.spawn(move || audio_loop(engine, wake))
        }
    }
    .map_err(|e| {
        // Nobody will listen on the socket, so it must not stay behind.
        let _ = fs::remove_file(socket);
        format!("audio thread: {e}")
    })?;
    let mut control = Control::new(
        &config,
        latency_ms,
        supported,
        commands,
        events,
        audio.thread().clone(),
    );
    control.set_plugins(libraries, chains);
    for setup in setups {
        control.add_output(setup);
    }
    if let Some(open) = capture {
        control.set_analog(open);
    }
    let found = hardware.as_mut().and_then(|open| open());
    let volume = OutputVolume::new(found, config.bit_perfect);
    control.restore_volume(volume, state.to_path_buf());
    let mut server = Server {
        listener,
        notify,
        control,
        conns: BTreeMap::new(),
        notices: BTreeMap::new(),
        next_conn: 1,
        accept_after: None,
        accept_short: false,
        rates,
        card,
        search,
        probe_wait: ProbeWait::default(),
        hardware,
        further_search: searches,
        further_rates: watches,
    };
    server.control.start(Instant::now());
    let mut gaps_seen = 0;
    // The audio thread only ends on its own if it panicked. The daemon then
    // stops with an error, so that systemd restarts it, rather than taking
    // clients that nothing plays.
    while !shutdown.load(Ordering::Relaxed) && !audio.is_finished() {
        let was_open = server.control.output_is_open();
        server.step();
        gaps_seen = log_new_gaps(&counters, gaps_seen);
        if was_open && !server.control.output_is_open() {
            eprintln!("aas: output closed: {counters}");
        }
    }
    let died = audio.is_finished();
    server.control.shutdown();
    let joined = audio.join();
    eprintln!("aas: stopped: {counters}");
    // The fds of `notify` must outlive the audio thread. The writers stop
    // here too.
    drop(server);
    for (index, counters) in &output_counters {
        eprintln!("aas: output {}: {counters}", index + 1);
    }
    let _ = fs::remove_file(socket);
    match joined {
        Err(_) => Err("the audio thread panicked".to_string()),
        Ok(()) if died => Err("the audio thread stopped".to_string()),
        Ok(()) => Ok(()),
    }
}

/// The matrix of the config, as wide as the plugins of the `mix` and
/// `output` chains make it. Without `[[matrix]]` tables it is the default.
/// A route to a channel above the width is an error.
pub fn build_matrix(config: &Config, libraries: &mut Libraries) -> Result<Matrix, String> {
    let width = matrix_width(&config.plugins, config.channels, libraries).map_err(|e| e.message)?;
    match &config.matrix {
        None => Ok(Matrix::default_for(width)),
        Some(routes) => Matrix::from_routes(routes, width),
    }
}

/// The matrix of each `[[output]]` table, as wide as its plugins make it,
/// checked as the master's are: a route to a channel above the width, a
/// plugin that fails at any rate, chains that do not end at the card's
/// channels, or a card of another channel count stop the start. A plugin
/// that does not take the default rate does not: the output is silent at
/// a rate its chains do not build at.
fn check_outputs(
    config: &Config,
    cards: &[FurtherSink],
    libraries: &mut Libraries,
) -> Result<Vec<Matrix>, String> {
    let mut matrices = Vec::new();
    for (i, (output, card)) in config.outputs.iter().zip(cards).enumerate() {
        let n = i + 2;
        let context = |e: String| format!("output {n}: {e}");
        let channels = output.channels;
        if card.sink.channels() != channels {
            let has = card.sink.channels();
            return Err(context(format!(
                "{channels} channels, but its sink has {has}"
            )));
        }
        let w =
            widths(&output.plugins, channels, libraries).map_err(|(_, e)| context(e.message))?;
        if w.end != channels {
            return Err(context(format!(
                "the mix and output chains give {} channels, but the card has {channels}",
                w.end
            )));
        }
        for (place, width) in [(PluginPlace::Mix, w.mix), (PluginPlace::Output, w.output)] {
            let (plugins, rate, format) = (&output.plugins, config.default_rate, config.format);
            match build_chain(plugins, place, rate, format, width, libraries) {
                Err(e) if e.kind != Failure::Unsupported => return Err(context(e.message)),
                _ => {}
            }
        }
        let matrix = match &output.matrix {
            None => Matrix::default_for(w.mix),
            Some(routes) => Matrix::from_routes(routes, w.mix).map_err(context)?,
        };
        matrices.push(matrix);
    }
    Ok(matrices)
}

/// Forced bit-perfect bypasses the `mix` and `output` chains, so it cannot
/// work with chains that change the channel count: the matrix would not
/// reach the card's channels. A matrix narrower or wider than the card
/// comes only from such chains, and is refused with them.
pub fn check_forced(config: &Config, libraries: &mut Libraries) -> Result<(), String> {
    if config.bit_perfect != BitPerfect::Forced {
        return Ok(());
    }
    let w = widths(&config.plugins, config.channels, libraries).map_err(|(_, e)| e.message)?;
    if w.changes || w.mix != config.channels {
        return Err("bit_perfect = \"forced\" needs chains that keep the channel count".into());
    }
    Ok(())
}

/// The routes of `matrix` for the log, such as `players.left to 1`, with
/// the gain where it is not 0 dB.
pub fn describe(matrix: &Matrix) -> String {
    let mut routes = Vec::new();
    for (input, name) in INPUT_NAMES.iter().enumerate() {
        for channel in 0..matrix.width() {
            let gain = matrix.gain(input, channel);
            if gain == 0.0 {
                continue;
            }
            let mut route = format!("{name} to {}", channel + 1);
            if gain != 1.0 {
                route += &format!(" at {:.2} dB", 20.0 * gain.log10());
            }
            routes.push(route);
        }
    }
    if routes.is_empty() {
        return "no routes, every channel is silent".to_string();
    }
    routes.join(", ")
}

/// Logs a wake-up gap longer than the buffer soon after it happens, with
/// its time on CLOCK_MONOTONIC. Several between two looks are logged as one,
/// with the latest gap. Returns the gaps seen so far.
fn log_new_gaps(counters: &Counters, seen: u64) -> u64 {
    let gaps = counters.wake_gaps.load(Ordering::Relaxed);
    if gaps > seen {
        let ms = counters.last_gap_ns.load(Ordering::Relaxed) as f64 / 1e6;
        let at = counters.last_gap_at_ns.load(Ordering::Relaxed) as f64 / 1e9;
        eprintln!(
            "aas: wake-up gap of {ms:.3} ms at {at:.3} s monotonic, {} new, {gaps} in all",
            gaps - seen
        );
    }
    gaps
}

/// The audio thread: one cycle after another, parked while the output is
/// closed. Wakes the control thread when it has events to read.
fn audio_loop<P: Processing, S: Sink>(mut engine: Engine<P, S>, notify: RawFd) {
    match rt::set_fifo(rt::AUDIO_PRIORITY) {
        Ok(()) => eprintln!("aas: audio thread at SCHED_FIFO {}", rt::AUDIO_PRIORITY),
        Err(e) => eprintln!("aas: warning: audio thread not real time: {e}"),
    }
    while !engine.is_stopped() {
        // An error from a FallbackSink is an xrun, already counted.
        let _ = engine.cycle();
        if engine.has_unread_events() {
            let one = 1u64;
            // SAFETY: a plain write of 8 bytes to a non-blocking eventfd.
            unsafe { libc::write(notify, (&raw const one).cast(), size_of::<u64>()) };
        }
        if engine.is_closed() && !engine.is_stopped() {
            thread::park_timeout(PARK);
        }
    }
}

struct Server {
    listener: OwnedFd,
    notify: OwnedFd,
    control: Control,
    conns: BTreeMap<ConnId, OwnedFd>,
    /// The latest volume notification each subscriber has waiting.
    notices: BTreeMap<ConnId, Reply>,
    next_conn: ConnId,
    /// Set while the listener is not polled, after an accept that could not
    /// get a descriptor. The connection stays queued, so the listener would
    /// stay readable and the poll would return at once, over and over.
    accept_after: Option<Instant>,
    /// Accepting is failing for want of a descriptor, and that is logged.
    accept_short: bool,
    /// The card's rates, when they change.
    rates: RatesWatch,
    /// When the card comes back.
    card: CardWatch,
    /// Looks for a card that is gone, and has the audio thread try it.
    search: Option<(CardSearch, CardProbe)>,
    /// When to look for the card next.
    probe_wait: ProbeWait,
    /// Opens the card's volume control, unless the volume is in software
    /// by config.
    hardware: Option<OpenVolume>,
    /// Looks for each further output's card while it is gone.
    further_search: Vec<FurtherSearch>,
    /// The rates of each further output's card, by its index, when they
    /// change.
    further_rates: Vec<(usize, RatesWatch)>,
}

/// The search for a further output's card, as for the master's.
struct FurtherSearch {
    search: CardSearch,
    probe: Option<CardProbe>,
    wait: ProbeWait,
}

impl Server {
    /// Waits for something to do, or the timer, and does it.
    fn step(&mut self) {
        let poll_in = |fd: RawFd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if self.accept_after.is_some_and(|at| Instant::now() >= at) {
            self.accept_after = None;
        }
        // poll skips a negative descriptor.
        let listener = match self.accept_after {
            Some(_) => -1,
            None => self.listener.as_raw_fd(),
        };
        let mut fds = vec![poll_in(listener), poll_in(self.notify.as_raw_fd())];
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        fds.extend(self.conns.values().map(|fd| poll_in(fd.as_raw_fd())));
        // SAFETY: fds is a valid array of its length.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, TICK_MS) };
        if rc < 0 {
            // Only EINTR is expected; the next step polls again.
            return;
        }
        let now = Instant::now();
        if let Some(rates) = self.rates.take() {
            self.control.set_supported(rates, now);
        }
        // A card that is back may be a new instance, so its control is
        // opened again. One that is gone has none, and the old one is closed.
        if self.card.changed() {
            if let Some(open) = &mut self.hardware {
                self.control.card_changed(open());
            }
        }
        // After the card change above: the mixer of a card that has just
        // gone is closed by now, so the card is no longer listed for that.
        if let Some((search, probe)) = &mut self.search {
            let refused = search.is_refused();
            if self.probe_wait.due(search.is_gone() || refused, now) && probe(refused) {
                search.retry();
            }
        }
        // A further card without a probe is tried every second while it is
        // gone, and one that refuses is left alone.
        for further in &mut self.further_search {
            let FurtherSearch {
                search,
                probe,
                wait,
            } = further;
            let refused = search.is_refused();
            if wait.due(search.is_gone() || refused, now)
                && probe.as_mut().map_or(!refused, |probe| probe(refused))
            {
                search.retry();
            }
        }
        for (index, watch) in &mut self.further_rates {
            if let Some(rates) = watch.take() {
                self.control.set_output_rates(*index, rates, now);
            }
        }
        if fds[1].revents != 0 {
            let mut count = 0u64;
            // SAFETY: reads 8 bytes into a u64; non-blocking.
            unsafe { libc::read(fds[1].fd, (&raw mut count).cast(), size_of::<u64>()) };
        }
        self.control.handle_events(now);
        if fds[0].revents != 0 {
            self.accept_all();
        }
        for (conn, pfd) in ids.into_iter().zip(&fds[2..]) {
            if pfd.revents != 0 {
                self.serve(conn, now);
            }
        }
        self.control.tick(now);
        for (conn, reply) in self.control.take_replies() {
            self.answer(conn, Answer::from(reply), now);
        }
        self.notify();
    }

    /// Sends the volume notifications. Each subscriber has at most one
    /// waiting, the latest, and it is sent only once the subscriber has
    /// read everything sent to it before. So notifications never fill its
    /// socket, its replies always fit, and a subscriber that has not read
    /// for a while still gets the current volume. What cannot be sent now
    /// is tried again at the next step, within the tick. If the socket
    /// cannot tell what is unread, the notice is sent rather than kept for
    /// good.
    fn notify(&mut self) {
        self.notices.extend(self.control.take_notices());
        self.notices.retain(|conn, notice| {
            let Some(fd) = self.conns.get(conn) else {
                return false;
            };
            if !notice_is_due(fd) {
                return true;
            }
            // A connection that has gone is closed by the next poll.
            let _ = send_reply(fd, notice, &[]);
            false
        });
    }

    fn accept_all(&mut self) {
        loop {
            // SAFETY: plain syscall; the descriptor is owned at once.
            let raw = unsafe {
                libc::accept4(
                    self.listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            if raw < 0 {
                let e = io::Error::last_os_error();
                if accept_must_pause(&e) {
                    // Logged once, not at every pause until it passes.
                    if !std::mem::replace(&mut self.accept_short, true) {
                        let ms = ACCEPT_PAUSE.as_millis();
                        eprintln!("aas: accept: {e}, trying every {ms} ms until it works");
                    }
                    self.accept_after = Some(Instant::now() + ACCEPT_PAUSE);
                } else if e.kind() != io::ErrorKind::WouldBlock {
                    eprintln!("aas: accept: {e}");
                }
                return;
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            if std::mem::take(&mut self.accept_short) {
                eprintln!("aas: accepting clients again");
            }
            let conn = self.next_conn;
            self.next_conn += 1;
            let name = peer_name(&fd);
            self.conns.insert(conn, fd);
            self.control.connected(conn, name);
        }
    }

    /// Reads every message waiting on a connection.
    fn serve(&mut self, conn: ConnId, now: Instant) {
        loop {
            let Some(fd) = self.conns.get(&conn) else {
                return;
            };
            let mut buf = [0u8; 64];
            // SAFETY: the buffer is valid for its length.
            let rc = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            let n = match check(rc) {
                Ok(0) => return self.close(conn, now),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return self.close(conn, now),
            };
            let request = (n == REQUEST_LEN)
                .then(|| proto::decode_request(&buf[..n]))
                .flatten();
            let answer = self.control.request(conn, request, now);
            // Replies to earlier requests, such as a drain that finished or
            // was cancelled by this one, go first.
            for reply in self.control.take_replies_for(conn) {
                self.answer(conn, Answer::from(reply), now);
            }
            self.answer(conn, answer, now);
            self.notify();
        }
    }

    fn answer(&mut self, conn: ConnId, answer: Answer, now: Instant) {
        let Some(fd) = self.conns.get(&conn) else {
            return;
        };
        let sent = match answer.reply {
            Some(reply) => send_reply(fd, &reply, &answer.fds).is_ok(),
            None => true,
        };
        if !sent || answer.close {
            self.close(conn, now);
        }
    }

    fn close(&mut self, conn: ConnId, now: Instant) {
        self.notices.remove(&conn);
        if self.conns.remove(&conn).is_some() {
            self.control.disconnected(conn, now);
        }
    }
}

impl From<Reply> for Answer {
    fn from(reply: Reply) -> Answer {
        Answer {
            reply: Some(reply),
            fds: Vec::new(),
            close: false,
        }
    }
}

/// Sends a reply, with descriptors by `SCM_RIGHTS`.
fn send_reply(fd: &OwnedFd, reply: &Reply, fds: &[OwnedFd]) -> io::Result<()> {
    let data = proto::encode_reply(reply);
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let mut control = [0u64; 8];
    // SAFETY: an all-zero msghdr is valid; the pointers set below live
    // until sendmsg returns, and the control buffer is large enough for
    // the two descriptors a reply carries at most.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let bytes = size_of_val(fds) as u32;
        let space = unsafe { libc::CMSG_SPACE(bytes) } as usize;
        assert!(space <= size_of_val(&control));
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
            let out = libc::CMSG_DATA(cmsg) as *mut RawFd;
            for (i, fd) in fds.iter().enumerate() {
                out.add(i).write_unaligned(fd.as_raw_fd());
            }
        }
    }
    loop {
        let flags = libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT;
        let rc = unsafe { libc::sendmsg(fd.as_raw_fd(), &msg, flags) };
        match check(rc) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other.map(drop),
        }
    }
}

/// Bytes sent on a connection that the peer has not read yet.
fn unread_bytes(fd: &OwnedFd) -> Option<usize> {
    let mut queued: libc::c_int = 0;
    // SAFETY: TIOCOUTQ, which is SIOCOUTQ on a socket, writes one int.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
    (rc == 0).then_some(queued as usize)
}

/// When to look for a card that is gone: a second after it went, and
/// every second from then. Never at once, since a card that has just gone
/// can still be listed, and a try that is bound to fail costs the audio
/// thread an open.
#[derive(Default)]
struct ProbeWait {
    /// When to look next. None while the card is not gone.
    at: Option<Instant>,
}

impl ProbeWait {
    /// Whether to look now. `gone` is what the sink says in this step.
    fn due(&mut self, gone: bool, now: Instant) -> bool {
        if !gone {
            self.at = None;
            return false;
        }
        match self.at {
            Some(at) if now >= at => {
                self.at = Some(now + RETRY);
                true
            }
            Some(_) => false,
            None => {
                self.at = Some(now + RETRY);
                false
            }
        }
    }
}

/// Notices a change in the list of cards between looks.
#[derive(Default)]
pub struct ListChange {
    last: Option<Vec<String>>,
}

impl ListChange {
    /// Whether `now` differs from the list at the last look. The first look
    /// only takes the baseline.
    pub fn changed(&mut self, now: Vec<String>) -> bool {
        self.last
            .replace(now.clone())
            .is_some_and(|last| last != now)
    }

    /// Forgets the list, so that the next look is a baseline.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Whether an accept failed for want of a descriptor or memory. The
/// connection is still queued then, and trying again at once cannot help.
fn accept_must_pause(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

/// Whether a subscriber's notice can go out now: it has read everything
/// sent before, or the socket cannot tell.
fn notice_is_due(fd: &OwnedFd) -> bool {
    !matches!(unread_bytes(fd), Some(unread) if unread > 0)
}

/// The client's process name, from its credentials, for the log.
fn peer_name(fd: &OwnedFd) -> String {
    // SAFETY: an all-zero ucred is valid, and getsockopt fills at most len.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return "a client".into();
    }
    let comm = fs::read_to_string(format!("/proc/{}/comm", cred.pid));
    let name = comm.map(|c| c.trim_end().to_string()).unwrap_or_default();
    format!("{name} ({})", cred.pid)
}

/// A listening seqpacket socket at `path`, readable and writable by its
/// owner and group. The group is `audio` where the process may set it.
///
/// The socket is made under another name and renamed into place once it
/// listens, so that no client finds the path before it can connect.
fn listen(path: &Path) -> io::Result<OwnedFd> {
    let mut building = path.as_os_str().to_owned();
    building.push(format!(".{}", std::process::id()));
    let building = Path::new(&building);
    let (addr, len) = socket_address(building)?;
    let _ = fs::remove_file(building);
    let flags = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    // SAFETY: plain syscalls; the descriptor is owned at once.
    let raw = unsafe { libc::socket(libc::AF_UNIX, flags, 0) };
    check(raw as isize)?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    check(unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len) } as isize)?;
    let ready = (|| {
        fs::set_permissions(building, fs::Permissions::from_mode(0o660))?;
        set_audio_group(building);
        check(unsafe { libc::listen(fd.as_raw_fd(), libc::SOMAXCONN) } as isize)?;
        // Replaces a socket left by a daemon that died.
        fs::rename(building, path)
    })();
    if let Err(e) = ready {
        let _ = fs::remove_file(building);
        return Err(e);
    }
    Ok(fd)
}

fn set_audio_group(path: &Path) {
    let Some(gid) = group_id("audio") else {
        eprintln!("aas: no group audio, the socket keeps its group");
        return;
    };
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: a valid C string; uid -1 leaves the owner alone.
    if unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) } < 0 {
        let e = io::Error::last_os_error();
        eprintln!("aas: could not give the socket to group audio: {e}");
    }
}

fn group_id(name: &str) -> Option<libc::gid_t> {
    let c_name = CString::new(name).ok()?;
    // SAFETY: getgrnam_r writes into the buffers given, with their sizes.
    let mut group: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16384];
    let mut found: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrnam_r(
            c_name.as_ptr(),
            &mut group,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    (rc == 0 && !found.is_null()).then_some(group.gr_gid)
}

/// Lets the daemon hold as many descriptors as it is allowed, so that no
/// client is refused for want of one.
fn raise_file_limit() {
    // SAFETY: plain syscalls on a local struct.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0
        && limit.rlim_cur < limit.rlim_max
    {
        limit.rlim_cur = limit.rlim_max;
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket_pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        let kind = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC;
        // SAFETY: fds has room for the two descriptors.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn a_notice_waits_until_the_subscriber_has_read_what_was_sent() {
        let (ours, theirs) = socket_pair();
        assert!(notice_is_due(&ours));
        send_reply(&ours, &Reply::Ok, &[]).unwrap();
        assert!(!notice_is_due(&ours));
        let mut buf = [0u8; 64];
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { libc::recv(theirs.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        assert!(n > 0);
        assert!(notice_is_due(&ours));
    }

    #[test]
    fn the_matrix_is_logged_route_by_route_with_gains_off_0_db() {
        assert_eq!(
            describe(&Matrix::default_for(2)),
            "players.left to 1, players.right to 2, analog.left to 1, analog.right to 2"
        );
        let route = |from, to, gain_db| crate::config::Route { from, to, gain_db };
        let m = Matrix::from_routes(&[route(1, 1, 0.0), route(0, 2, -6.0)], 2).unwrap();
        assert_eq!(
            describe(&m),
            "players.left to 2 at -6.00 dB, players.right to 1"
        );
        let m = Matrix::from_routes(&[], 2).unwrap();
        assert_eq!(describe(&m), "no routes, every channel is silent");
    }

    #[test]
    fn a_route_above_the_width_of_the_matrix_is_an_error() {
        let mut config = Config {
            channels: 4,
            ..Config::default()
        };
        let route = |from, to| crate::config::Route {
            from,
            to,
            gain_db: 0.0,
        };
        config.matrix = Some(vec![route(0, 4)]);
        let mut libraries = Libraries::default();
        assert_eq!(build_matrix(&config, &mut libraries).unwrap().width(), 4);
        // A balance on the mix takes two channels, so the matrix has two.
        config.plugins = vec![crate::config::PluginConfig {
            place: crate::config::PluginPlace::Mix,
            kind: "balance".into(),
            path: None,
            params: Default::default(),
        }];
        let e = build_matrix(&config, &mut libraries).unwrap_err();
        assert!(e.contains("channel 4") && e.contains("2 channels"), "{e}");
        config.matrix = None;
        assert_eq!(
            build_matrix(&config, &mut libraries).unwrap(),
            Matrix::default_for(2)
        );
    }

    #[test]
    fn forced_bit_perfect_with_chains_that_change_the_count_stops_the_start() {
        let mix = |kind: &str| crate::config::PluginConfig {
            place: crate::config::PluginPlace::Mix,
            kind: kind.into(),
            path: None,
            params: Default::default(),
        };
        let output = |kind: &str| crate::config::PluginConfig {
            place: crate::config::PluginPlace::Output,
            ..mix(kind)
        };
        // Widened on the mix and narrowed on the output: in automatic mode
        // the chains end at the card's two channels.
        let config = Config {
            bit_perfect: BitPerfect::Forced,
            plugins: vec![mix("widen"), output("narrow")],
            ..Config::default()
        };
        let mut libraries = Libraries::default();
        let auto = Config {
            bit_perfect: BitPerfect::Auto,
            ..config.clone()
        };
        assert_eq!(check_forced(&auto, &mut libraries), Ok(()));
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("aas.sock");
        let state = dir.path().join("state.toml");
        let sink = crate::sink::MemorySink::paced();
        let log = sink.log();
        let shutdown = Arc::new(AtomicBool::new(true));
        let result = run(
            config,
            Box::new(sink),
            &socket,
            &state,
            None,
            None,
            shutdown,
        );
        assert_eq!(
            result,
            Err("bit_perfect = \"forced\" needs chains that keep the channel count".into())
        );
        assert!(log.opens().is_empty());
        assert!(!socket.exists());
        // Chains that keep the count are fine.
        let kept = Config {
            bit_perfect: BitPerfect::Forced,
            plugins: vec![mix("balance")],
            ..Config::default()
        };
        assert_eq!(check_forced(&kept, &mut libraries), Ok(()));
    }

    #[test]
    fn a_notice_is_sent_when_the_unread_bytes_cannot_be_told() {
        // The ioctl fails on a descriptor that is no socket.
        let file = OwnedFd::from(fs::File::open("/dev/null").unwrap());
        assert_eq!(unread_bytes(&file), None);
        assert!(notice_is_due(&file));
    }

    #[test]
    fn a_gone_card_is_looked_for_a_second_after_it_went_and_each_second_after() {
        let mut wait = ProbeWait::default();
        let t = Instant::now();
        let ms = Duration::from_millis;
        assert!(!wait.due(false, t));
        // Just gone: not yet.
        assert!(!wait.due(true, t + ms(50)));
        assert!(!wait.due(true, t + ms(1000)));
        assert!(wait.due(true, t + ms(1050)));
        assert!(!wait.due(true, t + ms(1100)));
        assert!(wait.due(true, t + ms(2050)));
        // Back and gone again: the second starts anew.
        assert!(!wait.due(false, t + ms(2100)));
        assert!(!wait.due(true, t + ms(4000)));
        assert!(!wait.due(true, t + ms(4900)));
        assert!(wait.due(true, t + ms(5000)));
    }

    #[test]
    fn a_change_in_the_card_list_is_seen_once() {
        let list = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut change = ListChange::default();
        // The first look is the baseline.
        assert!(!change.changed(list(&["a"])));
        assert!(!change.changed(list(&["a"])));
        assert!(change.changed(list(&[])));
        assert!(!change.changed(list(&[])));
        assert!(change.changed(list(&["a"])));
        assert!(change.changed(list(&["a", "b"])));
        assert!(!change.changed(list(&["a", "b"])));
        // Forgotten while not refused: the next look is a baseline again.
        change.reset();
        assert!(!change.changed(list(&["a"])));
    }

    #[test]
    fn only_running_out_of_descriptors_or_memory_pauses_accepting() {
        for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(accept_must_pause(&io::Error::from_raw_os_error(errno)));
        }
        for errno in [libc::EAGAIN, libc::ECONNABORTED, libc::EINTR] {
            assert!(!accept_must_pause(&io::Error::from_raw_os_error(errno)));
        }
    }
}
