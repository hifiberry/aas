//! AES67 network audio: the PTP clock, the streams announced over SAP, the
//! input that plays one of them, and the output that sends the master.
//!
//! [`Aes67`] is the control thread's handle on all of it.

pub mod clock;
pub mod config;
pub mod directory;
pub mod input;
pub mod jitter;
pub mod listen;
pub mod output;
pub mod ptp;
pub mod rtp;
pub mod sap;
pub mod sap_thread;
pub mod sdp;

use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aas_client::ring::Ring;
use std::os::fd::{AsFd, OwnedFd};

use crate::airplay::output::TapReader;
use crate::api::{
    Aes67ClockView, Aes67InputView, Aes67OutputView, Aes67StreamView, Aes67View, ApiEvent,
};
use crate::rate::ClientId;
use crate::state::{Stored, StoredAes67};
use clock::PtpClock;
use config::{Aes67Config, Group, Latency, PtpSource};
use directory::Directory;
use input::{InputCommand, InputHandle, InputStatus, Mode};
use output::{OutputHandle, OutputStatus};
use sdp::{Encoding, Session};

/// Without an address on the interface, it is looked for again this often.
const RETRY: Duration = Duration::from_secs(5);
/// The board's name, for the default latency.
const MODEL: &str = "/proc/device-tree/model";

/// The input thread as the control thread drives it; a fake in the tests.
pub trait InputPort: Send {
    fn command(&self, command: InputCommand);
    fn status(&self) -> InputStatus;
}

impl InputPort for InputHandle {
    fn command(&self, command: InputCommand) {
        InputHandle::command(self, command);
    }

    fn status(&self) -> InputStatus {
        InputHandle::status(self)
    }
}

/// The output thread as the control thread drives it; a fake in the tests.
pub trait OutputPort: Send {
    fn give_tap(&self, tap: TapReader);
    fn status(&self) -> OutputStatus;
}

impl OutputPort for OutputHandle {
    fn give_tap(&self, tap: TapReader) {
        OutputHandle::give_tap(self, tap);
    }

    fn status(&self) -> OutputStatus {
        OutputHandle::status(self)
    }
}

/// What the input thread needs of the engine: the ring of the `aes67`
/// source, and the engine's delay after it and its rate, which the control
/// thread keeps up to date.
pub struct InputLink {
    /// The ring's memory, mapped for the input thread at each try to
    /// start it.
    pub memfd: OwnedFd,
    /// Frames at `master_rate` from the ring to the card.
    pub output_delay: Arc<AtomicU32>,
    pub master_rate: Arc<AtomicU32>,
    /// The engine reads the ring: the source runs and the master is open.
    pub draining: Arc<AtomicBool>,
}

pub struct Aes67 {
    config: Aes67Config,
    /// The board's name, for `Latency::Auto`.
    model: Option<String>,
    directory: Arc<Mutex<Directory>>,
    /// What the SAP thread announces; the output fills it.
    announce: sap_thread::Announce,
    clock: PtpClock,
    input: Option<Box<dyn InputPort>>,
    /// The output thread, while the output is enabled and has started.
    output: Option<Box<dyn OutputPort>>,
    /// The id and rate of the monitor tap the output reads.
    output_tap: Option<(ClientId, u32)>,
    /// The interface's address, once it has one.
    address: Option<Ipv4Addr>,
    /// The interface's MAC could not be read, and that has been logged.
    no_mac_told: bool,
    /// The ring and the rest, until the interface has an address and the
    /// input thread starts.
    link: Option<InputLink>,
    /// The PTP listener was started, or failed for good.
    ptp_done: bool,
    /// The SAP thread is started.
    sap: bool,
    /// A failure to start the SAP or the input thread has been logged.
    failing_told: bool,
    /// The same for the output thread, which can start later.
    output_failing_told: bool,
    /// The input's status, read once per tick.
    status: InputStatus,
    /// When the interface is looked at again.
    retry_at: Option<Instant>,
    /// Waiting for the interface's address has been logged.
    waiting_told: bool,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    /// The selected stream's name. It stays while the stream is not
    /// announced, and plays when one of that name is.
    selection: Option<String>,
    latency: Latency,
    input_enabled: bool,
    output_enabled: bool,
    /// What the API set, for the state file.
    stored: StoredAes67,
    /// What the input was last told to play.
    playing: Option<Session>,
    /// The view last told, without its counters, and the directory's
    /// changes then.
    told: Option<(Aes67View, u64)>,
}

impl Aes67 {
    /// Starts AES67 as `config` says, with what the state file keeps. The
    /// PTP listener, the SAP thread and the input thread start once the
    /// interface has an IPv4 address; until then `tick` looks every 5 s.
    pub fn start(
        config: Aes67Config,
        stored: Option<&StoredAes67>,
        link: InputLink,
        now: Instant,
    ) -> Aes67 {
        let mut aes67 = Aes67::new(config, stored, board_model());
        aes67.link = Some(link);
        aes67.try_start(now);
        aes67
    }

    fn new(config: Aes67Config, stored: Option<&StoredAes67>, model: Option<String>) -> Aes67 {
        let input = &config.input;
        let config_latency = latency_code(input.latency);
        // A stored value is kept only while it applies, as
        // `Stored::resolve` has it; a config edit drops it.
        let stored = stored.map_or_else(StoredAes67::default, |s| StoredAes67 {
            stream: s.stream.clone().filter(|v| v.config == input.stream),
            latency: s.latency.clone().filter(|v| v.config == config_latency),
            input_enabled: s
                .input_enabled
                .clone()
                .filter(|v| v.config == input.enabled),
            output_enabled: s
                .output_enabled
                .clone()
                .filter(|v| v.config == config.output.enabled),
        });
        let stream = Stored::resolve(stored.stream.as_ref(), &input.stream);
        let latency = Stored::resolve(stored.latency.as_ref(), &config_latency);
        let input_enabled = Stored::resolve(stored.input_enabled.as_ref(), &input.enabled);
        let output_enabled =
            Stored::resolve(stored.output_enabled.as_ref(), &config.output.enabled);
        Aes67 {
            model,
            directory: Arc::new(Mutex::new(Directory::new(None))),
            announce: Arc::new(Mutex::new(None)),
            clock: PtpClock::off(),
            input: None,
            output: None,
            output_tap: None,
            address: None,
            no_mac_told: false,
            link: None,
            ptp_done: config.ptp == PtpSource::Off,
            sap: false,
            failing_told: false,
            output_failing_told: false,
            status: InputStatus::default(),
            retry_at: None,
            waiting_told: false,
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            selection: (!stream.is_empty()).then_some(stream),
            latency: latency_from_code(latency),
            input_enabled,
            output_enabled,
            stored,
            playing: None,
            told: None,
            config,
        }
    }

    /// Starts what waits for the interface's address: the PTP listener,
    /// the SAP thread, then the input and the output thread, which read
    /// the clock.
    fn try_start(&mut self, now: Instant) {
        self.retry_at = None;
        let interface = self.config.interface.clone();
        let addr = match listen::interface_ipv4(&interface) {
            Ok(addr) => addr,
            Err(e) => return self.wait(e, now),
        };
        self.address = Some(addr);
        if !self.ptp_done {
            match listen::start(&interface, self.config.domain, self.stop.clone()) {
                Ok((clock, thread)) => {
                    self.clock = clock;
                    self.threads.push(thread);
                }
                // The address went again meanwhile.
                Err(e) if e.kind() == io::ErrorKind::NotFound => return self.wait(e, now),
                Err(e) => {
                    let hint = if e.kind() == io::ErrorKind::PermissionDenied {
                        " (needs CAP_NET_RAW)"
                    } else {
                        ""
                    };
                    eprintln!("aas: aes67: no PTP listener: {e}{hint}");
                }
            }
            self.ptp_done = true;
        }
        if !self.sap {
            *lock(&self.directory) = Directory::new(Some(addr));
            let sap = sap_thread::start(
                addr,
                self.directory.clone(),
                self.announce.clone(),
                self.stop.clone(),
            );
            match sap {
                Ok(thread) => {
                    eprintln!("aas: aes67: on {interface}");
                    self.threads.push(thread);
                    self.sap = true;
                }
                Err(e) => self.failed("no SAP listener", e, now),
            }
        }
        if let Some(link) = &self.link {
            let started = Ring::map(link.memfd.as_fd()).and_then(|ring| {
                input::start(
                    addr,
                    self.clock.clone(),
                    ring,
                    link.output_delay.clone(),
                    link.master_rate.clone(),
                    link.draining.clone(),
                )
            });
            match started {
                Ok(handle) => {
                    self.link = None;
                    self.attach(Box::new(handle));
                }
                Err(e) => self.failed("no input", e, now),
            }
        }
        self.start_output(now);
    }

    /// Starts the output thread while the output is enabled, once the SAP
    /// thread that announces it runs.
    fn start_output(&mut self, now: Instant) {
        let Some(addr) = self.address else {
            return;
        };
        if !self.output_enabled || !self.sap || self.output.is_some() {
            return;
        }
        let mac = match output::interface_mac(&self.config.interface) {
            Ok(mac) => mac,
            Err(e) => {
                if !std::mem::replace(&mut self.no_mac_told, true) {
                    eprintln!("aas: aes67: {e}; the output's group comes from the address");
                }
                let [.., x, y] = addr.octets();
                [0, 0, 0, 0, x, y]
            }
        };
        let started = output::start(
            self.config.output.clone(),
            addr,
            mac,
            self.clock.clone(),
            self.announce.clone(),
        );
        match started {
            Ok(handle) => self.output = Some(Box::new(handle)),
            Err(e) => {
                if !std::mem::replace(&mut self.output_failing_told, true) {
                    eprintln!(
                        "aas: aes67: no output: {e}, trying again every {} s",
                        RETRY.as_secs()
                    );
                }
                self.retry_at = Some(now + RETRY);
            }
        }
    }

    /// The SAP or the input thread did not start: tried again in 5 s,
    /// logged once.
    fn failed(&mut self, what: &str, e: io::Error, now: Instant) {
        if !std::mem::replace(&mut self.failing_told, true) {
            eprintln!(
                "aas: aes67: {what}: {e}, trying again every {} s",
                RETRY.as_secs()
            );
        }
        self.retry_at = Some(now + RETRY);
    }

    /// Tries again in 5 s, saying so once.
    fn wait(&mut self, e: io::Error, now: Instant) {
        if !std::mem::replace(&mut self.waiting_told, true) {
            eprintln!("aas: aes67: {e}, trying again every {} s", RETRY.as_secs());
        }
        self.retry_at = Some(now + RETRY);
    }

    /// Takes the input thread: its channels, its latency, and the stream.
    fn attach(&mut self, input: Box<dyn InputPort>) {
        input.command(InputCommand::Channels(self.config.input.channels));
        input.command(InputCommand::Latency(self.latency_ms()));
        self.input = Some(input);
        self.playing = None;
        self.sync();
    }

    /// Looks for the interface's address again when it is due, and has the
    /// input play the selection when it is announced and stop it when it
    /// goes. Call on the control thread's timer.
    pub fn tick(&mut self, now: Instant) {
        if self.retry_at.is_some_and(|at| now >= at) {
            self.try_start(now);
        }
        if let Some(input) = &self.input {
            self.status = input.status();
        }
        self.sync();
    }

    /// The session the input should play: the selection, while the input
    /// is enabled and a stream of that name is announced that is not ours
    /// and that it can play.
    fn wanted(&self) -> Option<Session> {
        if !self.input_enabled {
            return None;
        }
        let name = self.selection.as_deref()?;
        let directory = lock(&self.directory);
        let listed = directory.by_name(name)?;
        let playable = !listed.own && sdp::selectable(&listed.session).is_ok();
        playable.then(|| listed.session.clone())
    }

    /// Tells the input what to play when that has changed.
    fn sync(&mut self) {
        let Some(input) = &self.input else {
            return;
        };
        let wanted = self.wanted();
        if wanted != self.playing {
            input.command(InputCommand::Select(wanted.clone()));
            self.playing = wanted;
        }
    }

    pub fn view(&self) -> Aes67View {
        let ptp = self.clock.now();
        let status = &self.status;
        let output = &self.config.output;
        let sending = self.output.as_ref().map(|o| o.status());
        Aes67View {
            clock: Aes67ClockView {
                source: match self.config.ptp {
                    PtpSource::Listen => "listen",
                    PtpSource::Off => "off",
                },
                domain: self.config.domain,
                locked: ptp.is_some(),
                grandmaster: ptp.map(|p| ptp::clock_id_text(&p.gm)),
                rate_ppm: ptp.map(|p| (p.rate - 1.0) * 1e6),
            },
            input: Aes67InputView {
                enabled: self.input_enabled,
                stream: self.selection.clone(),
                mode: status.mode.map(mode_name),
                receiving: self.receiving_in(status),
                latency_ms: self.latency_ms(),
                latency_auto: self.latency == Latency::Auto,
                late: status.late,
                lost: status.lost,
            },
            output: Aes67OutputView {
                enabled: self.output_enabled,
                sending: sending.is_some_and(|s| s.sending),
                group: match (sending, output.group) {
                    (Some(s), _) => Some(s.group.to_string()),
                    (None, Group::Addr(addr)) => Some(addr.to_string()),
                    (None, Group::Auto) => None,
                },
                port: output.port,
            },
        }
    }

    /// Every announced stream, by name.
    pub fn streams(&self) -> Vec<Aes67StreamView> {
        let ptp = self.clock.now();
        let directory = lock(&self.directory);
        let mut streams: Vec<Aes67StreamView> = directory
            .all()
            .map(|listed| {
                let s = &listed.session;
                let reason = if listed.own {
                    Some("own")
                } else {
                    sdp::selectable(s).err()
                };
                let follows = match (ptp, s.refclk) {
                    (Some(p), Some((gm, domain))) => gm == p.gm && domain == p.domain,
                    _ => false,
                };
                let mode = if follows { Mode::Ptp } else { Mode::Buffer };
                Aes67StreamView {
                    name: s.name.clone(),
                    origin: s.origin.to_string(),
                    group: s.group.to_string(),
                    port: s.port,
                    format: match s.encoding {
                        Encoding::L16 => "L16",
                        Encoding::L24 => "L24",
                        Encoding::Other => "other",
                    },
                    rate: s.rate,
                    channels: s.channels,
                    grandmaster: s.refclk.map(|(gm, _)| ptp::clock_id_text(&gm)),
                    selectable: reason.is_none(),
                    reason,
                    mode: reason.is_none().then(|| mode_name(mode)),
                }
            })
            .collect();
        streams.sort_by(|a, b| (&a.name, &a.origin).cmp(&(&b.name, &b.origin)));
        streams
    }

    /// Selects the stream called `name`, or none; an empty name is none. A
    /// name that is not announced, our own stream, and one the input
    /// cannot play are refused.
    pub fn select(&mut self, name: Option<String>) -> Result<(), String> {
        let name = name.filter(|n| !n.is_empty());
        self.check_select(name.as_deref())?;
        self.selection = name;
        self.stored.stream = Some(Stored {
            value: self.selection.clone().unwrap_or_default(),
            config: self.config.input.stream.clone(),
        });
        self.sync();
        Ok(())
    }

    /// Why `select` would refuse `name`, if it would.
    pub fn check_select(&self, name: Option<&str>) -> Result<(), String> {
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            let directory = lock(&self.directory);
            let Some(listed) = directory.by_name(name) else {
                return Err(format!("no stream {name:?} is announced"));
            };
            if listed.own {
                return Err(format!("{name:?} is our own stream"));
            }
            if let Err(field) = sdp::selectable(&listed.session) {
                return Err(format!("{name:?} cannot be played: its {field}"));
            }
        }
        Ok(())
    }

    pub fn set_latency(&mut self, latency: Latency) {
        self.latency = latency;
        self.stored.latency = Some(Stored {
            value: latency_code(latency),
            config: latency_code(self.config.input.latency),
        });
        if let Some(input) = &self.input {
            input.command(InputCommand::Latency(self.latency_ms()));
        }
    }

    pub fn set_input_enabled(&mut self, on: bool) {
        self.input_enabled = on;
        self.stored.input_enabled = Some(Stored {
            value: on,
            config: self.config.input.enabled,
        });
        self.sync();
    }

    /// Turns the output on or off. Off stops its thread, which withdraws
    /// the announcement; the control thread then removes its tap.
    pub fn set_output_enabled(&mut self, on: bool) {
        self.output_enabled = on;
        self.stored.output_enabled = Some(Stored {
            value: on,
            config: self.config.output.enabled,
        });
        if on {
            self.start_output(Instant::now());
        } else {
            // Dropping the handle joins the thread.
            self.output = None;
        }
    }

    /// Whether the output needs a tap at `rate`: it runs and has none at
    /// that rate.
    pub fn output_wants_tap(&self, rate: u32) -> bool {
        self.output.is_some() && self.output_tap.map(|(_, r)| r) != Some(rate)
    }

    /// The id of the tap the output had, for the audio thread to remove.
    pub fn take_output_tap(&mut self) -> Option<ClientId> {
        self.output_tap.take().map(|(id, _)| id)
    }

    /// The id of the tap of an output that no longer runs, to remove.
    pub fn stale_output_tap(&mut self) -> Option<ClientId> {
        if self.output.is_some() {
            return None;
        }
        self.take_output_tap()
    }

    /// Hands the output thread the reading end of its new tap.
    pub fn give_output_tap(&mut self, id: ClientId, rate: u32, reader: Ring) {
        if let Some(output) = &self.output {
            output.give_tap(TapReader { ring: reader, rate });
            self.output_tap = Some((id, rate));
        }
    }

    /// An event when the view or the list of streams has changed since the
    /// last call, the counters and the clock's rate aside.
    pub fn changed(&mut self) -> Option<ApiEvent> {
        let view = self.view();
        let mut steady = view.clone();
        steady.clock.rate_ppm = None;
        steady.input.late = 0;
        steady.input.lost = 0;
        let now = (steady, lock(&self.directory).changes());
        if self.told.as_ref() == Some(&now) {
            return None;
        }
        self.told = Some(now);
        Some(ApiEvent::Aes67(view))
    }

    /// Whether the input plays a stream that arrives, as of the last
    /// `tick`: the `aes67` source then counts as a running client.
    pub fn receiving(&self) -> bool {
        self.receiving_in(&self.status)
    }

    fn receiving_in(&self, status: &InputStatus) -> bool {
        self.playing.is_some() && status.receiving
    }

    /// What the API set, for the state file; None when nothing applies.
    pub fn stored(&self) -> Option<StoredAes67> {
        (self.stored != StoredAes67::default()).then(|| self.stored.clone())
    }

    /// The config's `volume_db` for the input, which its client volume
    /// starts from.
    pub fn volume_db(&self) -> f32 {
        self.config.input.volume_db
    }

    /// The slot the output fills with the session the SAP thread announces.
    pub fn announce(&self) -> sap_thread::Announce {
        self.announce.clone()
    }

    /// The input's latency in ms.
    fn latency_ms(&self) -> u32 {
        config::latency_ms(self.latency, self.model.as_deref())
    }

    /// Sets every thread's stop flag; `stop` waits for them.
    pub fn ask_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Stops every thread and waits for it. The SAP thread withdraws what it
    /// announced.
    pub fn stop(&mut self) {
        self.ask_stop();
        // Their handles join the input and the output thread.
        self.input = None;
        self.output = None;
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for Aes67 {
    fn drop(&mut self) {
        self.stop();
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Ptp => "ptp",
        Mode::Buffer => "buffer",
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The latency as the state file keeps it: 0 for `"auto"`.
fn latency_code(latency: Latency) -> u32 {
    match latency {
        Latency::Auto => 0,
        Latency::Ms(ms) => ms,
    }
}

fn latency_from_code(code: u32) -> Latency {
    match code {
        0 => Latency::Auto,
        ms => Latency::Ms(ms),
    }
}

/// `/proc/device-tree/model`, without its trailing NUL.
fn board_model() -> Option<String> {
    let bytes = std::fs::read(MODEL).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    Some(
        text.trim_end_matches(|c: char| c == '\0' || c.is_whitespace())
            .to_string(),
    )
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::net::Ipv4Addr;

    /// An input thread that records its commands and reports what a test
    /// sets.
    #[derive(Clone, Default)]
    pub(crate) struct FakeInput {
        pub(crate) commands: Arc<Mutex<Vec<InputCommand>>>,
        pub(crate) status: Arc<Mutex<InputStatus>>,
    }

    impl InputPort for FakeInput {
        fn command(&self, command: InputCommand) {
            lock(&self.commands).push(command);
        }

        fn status(&self) -> InputStatus {
            lock(&self.status).clone()
        }
    }

    impl FakeInput {
        /// The sessions selected since the last call, by name.
        pub(crate) fn selected(&self) -> Vec<Option<String>> {
            let mut commands = lock(&self.commands);
            let selected = commands.iter().filter_map(|c| match c {
                InputCommand::Select(s) => Some(s.as_ref().map(|s| s.name.clone())),
                _ => None,
            });
            let out = selected.collect();
            commands.clear();
            out
        }

        pub(crate) fn set_receiving(&self, on: bool) {
            lock(&self.status).receiving = on;
        }
    }

    /// An output thread that records the rates of its taps and reports
    /// what a test sets.
    #[derive(Clone)]
    pub(crate) struct FakeOutput {
        pub(crate) taps: Arc<Mutex<Vec<u32>>>,
        pub(crate) status: Arc<Mutex<OutputStatus>>,
    }

    impl Default for FakeOutput {
        fn default() -> FakeOutput {
            FakeOutput {
                taps: Arc::default(),
                status: Arc::new(Mutex::new(OutputStatus {
                    enabled: true,
                    sending: false,
                    group: Ipv4Addr::new(239, 69, 0, 1),
                    port: 5004,
                })),
            }
        }
    }

    impl OutputPort for FakeOutput {
        fn give_tap(&self, tap: TapReader) {
            lock(&self.taps).push(tap.rate);
        }

        fn status(&self) -> OutputStatus {
            *lock(&self.status)
        }
    }

    /// The config of an empty `[aes67]` table.
    pub(crate) fn config() -> Aes67Config {
        config::from_table(Some(toml::Table::new()))
            .unwrap()
            .unwrap()
    }

    /// Announces a stream as `source` does, with the Dante SDP renamed.
    pub(crate) fn announce(aes67: &Aes67, hash: u16, source: Ipv4Addr, name: &str) {
        let sdp = sdp::DANTE_SDP
            .replace("IN IP4 192.0.2.10", &format!("IN IP4 {source}"))
            .replace("s=AU-U22-000001 : 1", &format!("s={name}"));
        let msg = sap::SapMessage::Announce { hash, source, sdp };
        lock(&aes67.directory).message(msg, Instant::now());
    }

    pub(crate) fn withdraw(aes67: &Aes67, hash: u16, source: Ipv4Addr) {
        let msg = sap::SapMessage::Delete { hash, source };
        lock(&aes67.directory).message(msg, Instant::now());
    }

    impl Aes67 {
        /// AES67 on a fake input, with no thread, the interface at `own`.
        pub(crate) fn with_input(
            config: Aes67Config,
            stored: Option<&StoredAes67>,
            model: Option<&str>,
            input: FakeInput,
            own: Option<Ipv4Addr>,
        ) -> Aes67 {
            let mut aes67 = Aes67::new(config, stored, model.map(str::to_owned));
            *lock(&aes67.directory) = Directory::new(own);
            aes67.ptp_done = true;
            aes67.sap = true;
            aes67.attach(Box::new(input));
            aes67
        }

        /// Has the output run on `output`, as if enabled and started.
        pub(crate) fn with_output(&mut self, output: FakeOutput) {
            self.output_enabled = true;
            self.output = Some(Box::new(output));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeInput, FakeOutput, announce, config, withdraw};
    use super::*;
    use std::net::Ipv4Addr;

    const US: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 20);
    const THEM: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);

    fn with_stream(name: &str) -> (Aes67, FakeInput) {
        let mut c = config();
        c.input.stream = name.into();
        let input = FakeInput::default();
        let aes67 = Aes67::with_input(c, None, None, input.clone(), Some(US));
        (aes67, input)
    }

    fn named(name: &str) -> Vec<Option<String>> {
        vec![Some(name.to_string())]
    }

    #[test]
    fn a_selected_name_plays_when_it_is_announced() {
        let (mut aes67, input) = with_stream("studio");
        let now = Instant::now();
        // The first commands set the channels and the latency.
        {
            let commands = lock(&input.commands);
            assert!(matches!(commands[0], InputCommand::Channels([1, 2])));
            assert!(matches!(commands[1], InputCommand::Latency(20)));
        }
        aes67.tick(now);
        assert!(input.selected().is_empty());
        assert_eq!(aes67.view().input.stream.as_deref(), Some("studio"));
        announce(&aes67, 1, THEM, "other");
        aes67.tick(now);
        assert!(input.selected().is_empty());
        announce(&aes67, 2, THEM, "studio");
        aes67.tick(now);
        assert_eq!(input.selected(), named("studio"));
        // Nothing new while it stays as it is.
        aes67.tick(now);
        assert!(input.selected().is_empty());
    }

    #[test]
    fn a_withdrawn_stream_stays_selected_and_resumes() {
        let (mut aes67, input) = with_stream("studio");
        let now = Instant::now();
        announce(&aes67, 2, THEM, "studio");
        aes67.tick(now);
        assert_eq!(input.selected(), named("studio"));
        withdraw(&aes67, 2, THEM);
        aes67.tick(now);
        assert_eq!(input.selected(), [None]);
        assert_eq!(aes67.view().input.stream.as_deref(), Some("studio"));
        announce(&aes67, 2, THEM, "studio");
        aes67.tick(now);
        assert_eq!(input.selected(), named("studio"));
    }

    #[test]
    fn selecting_our_own_stream_is_refused() {
        let (mut aes67, input) = with_stream("");
        announce(&aes67, 1, US, "aas");
        announce(&aes67, 2, THEM, "studio");
        assert!(aes67.select(Some("aas".into())).is_err());
        assert!(aes67.select(Some("nowhere".into())).is_err());
        assert_eq!(aes67.view().input.stream, None);
        assert!(input.selected().is_empty());
        assert_eq!(aes67.select(Some("studio".into())), Ok(()));
        assert_eq!(input.selected(), named("studio"));
        assert_eq!(aes67.select(None), Ok(()));
        assert_eq!(input.selected(), [None]);
        let listed = aes67.streams();
        let own = listed.iter().find(|s| s.name == "aas").unwrap();
        assert!(!own.selectable);
        assert_eq!(own.reason, Some("own"));
        let theirs = listed.iter().find(|s| s.name == "studio").unwrap();
        assert!(theirs.selectable);
        assert_eq!(theirs.mode, Some("buffer"));
    }

    #[test]
    fn stored_selection_applies_while_the_config_says_the_same() {
        let stored = StoredAes67 {
            stream: Some(Stored {
                value: "picked".into(),
                config: "configured".into(),
            }),
            latency: Some(Stored {
                value: 7,
                config: 0,
            }),
            input_enabled: Some(Stored {
                value: false,
                config: true,
            }),
            output_enabled: Some(Stored {
                value: true,
                config: false,
            }),
        };
        let mut c = config();
        c.input.stream = "configured".into();
        let fake = FakeInput::default;
        let aes67 = Aes67::with_input(c.clone(), Some(&stored), None, fake(), None);
        let view = aes67.view();
        assert_eq!(view.input.stream.as_deref(), Some("picked"));
        assert_eq!((view.input.latency_ms, view.input.latency_auto), (7, false));
        assert!(!view.input.enabled);
        assert!(view.output.enabled);
        assert_eq!(aes67.stored(), Some(stored.clone()));
        // The config edited: its value wins and the stored one is dropped.
        c.input.stream = "edited".into();
        c.input.latency = Latency::Ms(9);
        let aes67 = Aes67::with_input(c.clone(), Some(&stored), None, fake(), None);
        let view = aes67.view();
        assert_eq!(view.input.stream.as_deref(), Some("edited"));
        assert_eq!(view.input.latency_ms, 9);
        assert!(!view.input.enabled);
        let kept = aes67.stored().unwrap();
        assert_eq!((kept.stream, kept.latency), (None, None));
        // The output's config edited.
        c.output.enabled = true;
        let aes67 = Aes67::with_input(c, Some(&stored), None, fake(), None);
        assert!(aes67.view().output.enabled);
        assert_eq!(aes67.stored().unwrap().output_enabled, None);
    }

    #[test]
    fn the_output_has_a_tap_at_the_masters_rate() {
        let (mut aes67, _) = with_stream("");
        assert!(!aes67.output_wants_tap(48000), "no output thread");
        let output = FakeOutput::default();
        aes67.with_output(output.clone());
        assert!(aes67.output_wants_tap(48000));
        let (_ring, fd) = Ring::create(4096).unwrap();
        aes67.give_output_tap(7, 48000, Ring::map(fd.as_fd()).unwrap());
        assert_eq!(*lock(&output.taps), [48000]);
        assert!(!aes67.output_wants_tap(48000));
        // A new rate: the old tap goes, a new one comes.
        assert!(aes67.output_wants_tap(44100));
        assert_eq!(aes67.take_output_tap(), Some(7));
        aes67.give_output_tap(8, 44100, Ring::map(fd.as_fd()).unwrap());
        assert_eq!(*lock(&output.taps), [48000, 44100]);
        // The view shows the group the output sends to.
        lock(&output.status).sending = true;
        let view = aes67.view().output;
        assert!(view.enabled && view.sending);
        assert_eq!(view.group.as_deref(), Some("239.69.0.1"));
        // Disabled: the thread goes, and its tap with it.
        assert_eq!(aes67.stale_output_tap(), None);
        aes67.set_output_enabled(false);
        assert_eq!(aes67.stale_output_tap(), Some(8));
        assert_eq!(aes67.stale_output_tap(), None);
        let view = aes67.view().output;
        assert!(!view.enabled && !view.sending);
        assert_eq!(view.group, None);
        assert!(!aes67.stored().unwrap().output_enabled.unwrap().value);
    }

    #[test]
    fn changes_are_stored_and_sent_to_the_input() {
        let (mut aes67, input) = with_stream("");
        announce(&aes67, 2, THEM, "studio");
        aes67.select(Some("studio".into())).unwrap();
        aes67.set_latency(Latency::Ms(5));
        aes67.set_input_enabled(false);
        assert_eq!(input.selected(), [Some("studio".to_string()), None]);
        let stored = aes67.stored().unwrap();
        assert_eq!(stored.stream.unwrap().value, "studio");
        assert_eq!(stored.latency.unwrap().value, 5);
        assert!(!stored.input_enabled.unwrap().value);
        aes67.set_input_enabled(true);
        assert_eq!(input.selected(), named("studio"));
        // The board decides "auto".
        let model = Some("Raspberry Pi 5 Model B Rev 1.0");
        let pi5 = Aes67::with_input(config(), None, model, FakeInput::default(), None);
        assert_eq!(pi5.view().input.latency_ms, 3);
    }

    #[test]
    fn an_event_comes_when_something_but_the_counters_changes() {
        let (mut aes67, input) = with_stream("studio");
        let now = Instant::now();
        assert!(matches!(aes67.changed(), Some(ApiEvent::Aes67(_))));
        assert_eq!(aes67.changed(), None);
        lock(&input.status).lost = 480;
        assert_eq!(aes67.changed(), None);
        announce(&aes67, 2, THEM, "other");
        assert!(aes67.changed().is_some());
        input.set_receiving(true);
        aes67.tick(now);
        assert!(!aes67.receiving(), "nothing announced plays");
        assert_eq!(aes67.changed(), None);
        announce(&aes67, 3, THEM, "studio");
        aes67.tick(now);
        assert!(aes67.receiving());
        let Some(ApiEvent::Aes67(view)) = aes67.changed() else {
            panic!("no event");
        };
        assert!(view.input.receiving);
    }
}
