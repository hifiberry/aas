//! An AirPlay stream between RECORD and TEARDOWN: the master's frames in,
//! from the monitor tap, and packets out, one every 352 frames of 44.1 kHz
//! by the system clock.
//!
//! The receiver plays by the time in the sync packets, which is the system
//! clock's, while the frames come at the master card's pace. As for a
//! further output, a `Drift` holds the frames waiting in the path at a
//! target by moving an `Adaptive` resampler's ratio: here the "card" is
//! the system clock taking 352 frames a packet. The delay is read when
//! each packet is made, in frames at the master's rate.

use super::ap2::audio::AudioSealer;
use super::rtp::{self, FRAMES, History, RATE, SYNC_EVERY};
use crate::adaptive::Adaptive;
use crate::config::{Format, Quality};
use crate::drift::Drift;
use std::time::Duration;

/// What goes out on the sockets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// An RTP audio packet, on the data socket.
    Audio(Vec<u8>),
    /// A sync packet, on the control socket.
    Sync([u8; 20]),
}

/// Packets of output the path holds as its target, besides the
/// resampler's filter.
const TARGET_PACKETS: f64 = 3.0;

/// A second of input beyond the target is dropped and the path primed
/// again: the master ran on while nothing was sent.
const MOST_SECONDS: f64 = 1.0;

pub struct Stream {
    master_rate: u32,
    latency: u32,
    ssrc: u32,
    first_seq: u16,
    first_rtptime: u32,
    ntp_start: u64,
    start: Duration,
    /// Packets made so far.
    made: u64,
    /// Master-rate stereo frames waiting, interleaved.
    input: Vec<i32>,
    adaptive: Adaptive,
    drift: Drift,
    /// Until the path first holds its target, packets go out as zeros.
    primed: bool,
    history: History,
    /// Packets that had to be filled with zeros after priming.
    padded: u64,
    /// The path's delay as the drift last read it.
    measured: f64,
    out: Vec<i32>,
    /// AirPlay 2's: seals each packet's payload.
    sealer: Option<AudioSealer>,
}

impl Stream {
    pub fn new(
        master_rate: u32,
        latency: u32,
        first_seq: u16,
        first_rtptime: u32,
        ntp_start: u64,
        start: Duration,
    ) -> Stream {
        let (adaptive, drift) = path(master_rate);
        Stream {
            master_rate,
            latency,
            ssrc: u32::from_be_bytes(super::raop::random()),
            first_seq,
            first_rtptime,
            ntp_start,
            start,
            made: 0,
            input: Vec::new(),
            adaptive,
            drift,
            primed: false,
            history: History::default(),
            padded: 0,
            measured: 0.0,
            out: vec![0; FRAMES * 2],
            sealer: None,
        }
    }

    /// The stream with its packets sealed under `shk`, for AirPlay 2.
    pub fn sealed(mut self, shk: &[u8; 32]) -> Stream {
        self.sealer = Some(AudioSealer::new(shk));
        self
    }

    /// Takes master-rate stereo frames from the tap.
    pub fn take(&mut self, interleaved: &[i32]) {
        self.input.extend_from_slice(interleaved);
        let most = self.target() + MOST_SECONDS * f64::from(self.master_rate);
        if self.queued() as f64 > most {
            let keep = self.target() as usize;
            let drop = self.queued() - keep;
            self.input.drain(..drop * 2);
            self.drift.reprime();
        }
    }

    /// A new master rate: the frames at the old one are dropped, the path
    /// primes again, and the RTP timeline goes on.
    pub fn set_rate(&mut self, master_rate: u32) {
        self.master_rate = master_rate;
        (self.adaptive, self.drift) = path(master_rate);
        self.input.clear();
        self.primed = false;
    }

    /// Every packet whose time has come by `now`, each sync packet before
    /// the audio packet it belongs to.
    pub fn due(&mut self, now: Duration, out: &mut Vec<Outgoing>) {
        while self.next_due() <= now {
            let k = self.made;
            let first = k == 0;
            let rtptime = self.first_rtptime.wrapping_add((k * FRAMES as u64) as u32);
            let seq = self.first_seq.wrapping_add(k as u16);
            if k % u64::from(SYNC_EVERY) == 0 {
                let ntp = self.ntp(self.next_due());
                out.push(Outgoing::Sync(rtp::sync_packet(
                    first,
                    rtptime,
                    self.latency,
                    ntp,
                )));
            }
            let frames = self.frames();
            let mut payload = Vec::with_capacity(FRAMES * 4 + 8);
            rtp::alac_escape(&frames, &mut payload);
            let mut packet = Vec::with_capacity(12 + payload.len() + 24);
            rtp::audio_packet(first, seq, rtptime, self.ssrc, &payload, &mut packet);
            if let Some(sealer) = &mut self.sealer {
                sealer.seal(&mut packet);
            }
            self.history.push(seq, &packet);
            out.push(Outgoing::Audio(packet));
            self.made += 1;
        }
    }

    /// The next packet's frames: resampled from the input once the path
    /// holds its target, zeros before, and zeros when it runs dry, after
    /// which it primes again.
    fn frames(&mut self) -> Vec<[i16; 2]> {
        let zeros = vec![[0i16; 2]; FRAMES];
        if !self.primed {
            if (self.queued() as f64) < self.target() {
                return zeros;
            }
            self.primed = true;
        }
        self.measured = self.delay();
        let ratio = self.drift.update(self.measured);
        self.adaptive.set_ratio(ratio);
        let need = self.adaptive.input_needed(FRAMES as u32) as usize;
        if self.queued() < need {
            self.padded += 1;
            self.primed = false;
            self.drift.reprime();
            return zeros;
        }
        let (used, made) = self.adaptive.process(&self.input, &mut self.out);
        self.input.drain(..used * 2);
        let mut frames = zeros;
        for (f, s) in frames.iter_mut().zip(self.out[..made * 2].chunks_exact(2)) {
            *f = [to_i16(s[0]), to_i16(s[1])];
        }
        frames
    }

    /// When the next packet is due.
    pub fn next_due(&self) -> Duration {
        let ns = u128::from(self.made) * FRAMES as u128 * 1_000_000_000 / u128::from(RATE);
        self.start + Duration::from_nanos(ns as u64)
    }

    /// The packets a retransmit request asks for that are still kept.
    pub fn resend(&self, first: u16, count: u16, out: &mut Vec<Vec<u8>>) {
        for i in 0..count {
            if let Some(p) = self.history.get(first.wrapping_add(i)) {
                out.push(p.to_vec());
            }
        }
    }

    /// The NTP time of `now`, on the session's line from its start.
    pub fn ntp(&self, now: Duration) -> u64 {
        let d = now.saturating_sub(self.start);
        let fraction = (u64::from(d.subsec_nanos()) << 32) / 1_000_000_000;
        self.ntp_start
            .wrapping_add(d.as_secs() << 32)
            .wrapping_add(fraction)
    }

    /// The path's target in master-rate frames.
    pub fn target(&self) -> f64 {
        TARGET_PACKETS * FRAMES as f64 * f64::from(self.master_rate) / f64::from(RATE)
    }

    /// The path's delay in master-rate frames: what waits, and the
    /// resampler's filter. The drift holds it at the target.
    pub fn delay(&self) -> f64 {
        let scale = f64::from(self.master_rate) / f64::from(RATE);
        self.queued() as f64 + self.adaptive.delay_frames() * scale
    }

    /// The path's delay as the drift read it for the last packet.
    pub fn measured(&self) -> f64 {
        self.measured
    }

    /// Master-rate frames waiting.
    pub fn queued(&self) -> usize {
        self.input.len() / 2
    }

    /// Packets filled with zeros for want of frames, after priming.
    pub fn padded(&self) -> u64 {
        self.padded
    }

    /// Audio packets made so far.
    pub fn packets(&self) -> u64 {
        self.made
    }
}

/// A resampler from the master's rate to the stream's, and its drift
/// controller, holding `TARGET_PACKETS` packets of output.
fn path(master_rate: u32) -> (Adaptive, Drift) {
    let adaptive = Adaptive::new(
        master_rate,
        RATE,
        2,
        FRAMES as u32,
        Quality::High,
        Format::I32,
    );
    let scale = f64::from(master_rate) / f64::from(RATE);
    let period = (FRAMES as f64 * scale).round() as u32;
    let drift = Drift::new(TARGET_PACKETS * FRAMES as f64 * scale, master_rate, period);
    (adaptive, drift)
}

/// A sample's upper 16 bits, rounded.
fn to_i16(v: i32) -> i16 {
    ((i64::from(v) + 0x8000) >> 16).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATENCY: u32 = 66150;

    fn stream(rate: u32) -> Stream {
        Stream::new(rate, LATENCY, 100, 5000, 1 << 32, Duration::ZERO)
    }

    fn audio(out: &[Outgoing]) -> Vec<&Vec<u8>> {
        out.iter()
            .filter_map(|o| match o {
                Outgoing::Audio(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn syncs(out: &[Outgoing]) -> Vec<(usize, [u8; 20])> {
        out.iter()
            .enumerate()
            .filter_map(|(i, o)| match o {
                Outgoing::Sync(p) => Some((i, *p)),
                _ => None,
            })
            .collect()
    }

    fn timestamp(packet: &[u8]) -> u32 {
        u32::from_be_bytes(packet[4..8].try_into().unwrap())
    }

    /// A 1 kHz sine, stereo, `frames` long, from frame `from`.
    fn sine(rate: u32, from: u64, frames: usize) -> Vec<i32> {
        (0..frames as u64)
            .flat_map(|n| {
                let t = (from + n) as f64 / f64::from(rate);
                let v =
                    (0.25 * f64::from(i32::MAX) * (2.0 * std::f64::consts::PI * 1000.0 * t).sin())
                        as i32;
                [v, v]
            })
            .collect()
    }

    /// Runs `seconds` of a master at `rate` that is `ppm` fast, pushing
    /// periods of `period` frames, and sends what is due after each.
    fn run(
        s: &mut Stream,
        rate: u32,
        ppm: f64,
        period: usize,
        seconds: f64,
        out: &mut Vec<Outgoing>,
    ) {
        let pushes = (seconds * f64::from(rate) / period as f64) as u64;
        for i in 0..pushes {
            let at = Duration::from_secs_f64(
                (i + 1) as f64 * period as f64 / (f64::from(rate) * (1.0 + ppm * 1e-6)),
            );
            s.take(&sine(rate, i * period as u64, period));
            s.due(at, out);
        }
    }

    /// As `run` at 48 kHz in time, from `from` seconds on.
    fn run_from(s: &mut Stream, from: f64, seconds: f64, out: &mut Vec<Outgoing>) {
        for i in 0..(seconds * 48000.0 / 256.0) as u64 {
            let at = Duration::from_secs_f64(from + (i + 1) as f64 * 256.0 / 48000.0);
            s.take(&sine(48000, i * 256, 256));
            s.due(at, out);
        }
    }

    #[test]
    fn packets_are_due_every_352_frames_of_44100() {
        let mut s = stream(48000);
        let mut out = Vec::new();
        s.due(Duration::from_secs(1), &mut out);
        // Packets 0 to 125 are due by 1 s: 125 * 352 / 44100 = 0.9977 s.
        assert_eq!(audio(&out).len(), 126);
        let at = s.next_due();
        assert_eq!(at.as_nanos(), 126 * 352 * 1_000_000_000 / 44100);
        for (k, p) in audio(&out).iter().enumerate() {
            assert_eq!(timestamp(p), 5000 + 352 * k as u32);
            assert_eq!(u16::from_be_bytes([p[2], p[3]]), 100 + k as u16);
            assert_eq!(p.len(), 12 + 1416);
        }
    }

    #[test]
    fn the_first_packet_has_the_marker_and_a_first_sync_before_it() {
        let mut s = stream(48000);
        let mut out = Vec::new();
        s.due(Duration::ZERO, &mut out);
        assert_eq!(out.len(), 2);
        let Outgoing::Sync(sync) = out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(sync, rtp::sync_packet(true, 5000, LATENCY, 1 << 32));
        let Outgoing::Audio(p) = &out[1] else {
            panic!()
        };
        assert_eq!(p[1], 0xe0);
        out.clear();
        s.due(Duration::from_millis(8), &mut out);
        let Outgoing::Audio(p) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(p[1], 0x60);
    }

    #[test]
    fn a_sync_follows_every_121_packets_with_the_time_of_its_packet() {
        let mut s = stream(48000);
        let mut out = Vec::new();
        s.due(Duration::from_secs(3), &mut out);
        let at: Vec<usize> = syncs(&out).iter().map(|(i, _)| *i).collect();
        // Sync k sits before audio packet 121 k: at index 121 k + k.
        assert_eq!(at, [0, 122, 244, 366]);
        let (_, second) = syncs(&out)[1];
        let k = 121u64;
        let rtp_now = 5000 + 352 * k as u32;
        let ntp = s.ntp(Duration::from_nanos(k * 352 * 1_000_000_000 / 44100));
        assert_eq!(second, rtp::sync_packet(false, rtp_now, LATENCY, ntp));
    }

    #[test]
    fn ntp_runs_from_the_start() {
        let s = stream(48000);
        assert_eq!(s.ntp(Duration::ZERO), 1 << 32);
        assert_eq!(s.ntp(Duration::from_millis(1500)), (2 << 32) + 0x8000_0000);
    }

    #[test]
    fn at_48k_in_the_path_settles_on_its_target() {
        let mut s = stream(48000);
        let mut out = Vec::new();
        run(&mut s, 48000, 100.0, 256, 60.0, &mut out);
        let err = s.measured() - s.target();
        // Read just after a push of 256 frames: within that and a margin.
        assert!(
            err.abs() < 320.0,
            "measured {} target {}",
            s.measured(),
            s.target()
        );
        assert_eq!(s.padded(), 0);
        // The last packets carry the sine, not zeros.
        let last = audio(&out).last().unwrap().to_vec();
        assert!(last[20..].iter().any(|&b| b != 0));
    }

    #[test]
    fn a_starved_packet_is_zeros_and_counted() {
        let mut s = stream(48000);
        let mut out = Vec::new();
        run(&mut s, 48000, 0.0, 256, 1.0, &mut out);
        assert_eq!(s.padded(), 0);
        out.clear();
        // Nothing more comes for half a second: one packet starves, and
        // the path primes again with zeros.
        s.due(Duration::from_millis(1500), &mut out);
        assert_eq!(s.padded(), 1);
        let last = audio(&out).last().unwrap().to_vec();
        assert!(last[20..1420].iter().all(|&b| b == 0));
        // Frames again: it plays them once the path holds its target.
        out.clear();
        run_from(&mut s, 1.5, 1.0, &mut out);
        let last = audio(&out).last().unwrap().to_vec();
        assert!(last[20..].iter().any(|&b| b != 0));
        assert_eq!(s.padded(), 1);
    }

    #[test]
    fn a_master_that_ran_on_is_cut_back() {
        let mut s = stream(48000);
        let mut out = Vec::new();
        run(&mut s, 48000, 0.0, 256, 1.0, &mut out);
        // Three seconds arrive at once.
        s.take(&sine(48000, 0, 3 * 48000));
        s.due(Duration::from_millis(1010), &mut out);
        assert!(
            (s.queued() as f64) <= s.target() + 48000.0,
            "{}",
            s.queued()
        );
    }

    #[test]
    fn a_rate_change_keeps_rtptime_continuous() {
        let mut s = stream(48000);
        let mut out = Vec::new();
        run(&mut s, 48000, 0.0, 256, 2.0, &mut out);
        let before = audio(&out).len();
        assert!(s.queued() > 0);
        s.set_rate(96000);
        // Frames at 48 kHz would play at the wrong pitch.
        assert_eq!(s.queued(), 0);
        let mut later = Vec::new();
        for i in 0..(2 * 96000 / 512) as u64 {
            let at =
                Duration::from_secs(2) + Duration::from_secs_f64((i + 1) as f64 * 512.0 / 96000.0);
            s.take(&sine(96000, i * 512, 512));
            s.due(at, &mut later);
        }
        out.extend(later);
        let packets = audio(&out);
        for w in packets.windows(2) {
            assert_eq!(timestamp(w[1]).wrapping_sub(timestamp(w[0])), 352);
        }
        assert!(packets.len() > before + 240);
        assert_eq!(s.padded(), 0);
    }

    #[test]
    fn a_sealed_stream_seals_each_packet_once_and_resends_it_as_sent() {
        let shk = [3u8; 32];
        let mut clear = stream(48000);
        let mut sealed = stream(48000).sealed(&shk);
        sealed.ssrc = clear.ssrc;
        let (mut a, mut b) = (Vec::new(), Vec::new());
        clear.due(Duration::from_millis(50), &mut a);
        sealed.due(Duration::from_millis(50), &mut b);
        let (a, b) = (audio(&a), audio(&b));
        assert_eq!(a.len(), b.len());
        for (i, (c, s)) in a.iter().zip(&b).enumerate() {
            // The header as it is; the payload sealed, with the tag and
            // the packet's count after it.
            assert_eq!(c[..12], s[..12]);
            assert_eq!(s.len(), c.len() + 24);
            assert_ne!(c[12..], s[12..c.len()]);
            assert_eq!(s[s.len() - 8..], (i as u64).to_le_bytes());
        }
        // Syncs stay in the clear.
        let mut again = Vec::new();
        sealed.resend(101, 1, &mut again);
        assert_eq!(again[0], *b[1]);
    }

    #[test]
    fn resend_returns_the_stored_packets() {
        let mut s = stream(48000);
        let mut out = Vec::new();
        s.due(Duration::from_millis(100), &mut out);
        let sent: Vec<Vec<u8>> = audio(&out).into_iter().cloned().collect();
        let mut again = Vec::new();
        s.resend(102, 3, &mut again);
        assert_eq!(again, sent[2..5]);
        again.clear();
        s.resend(60000, 2, &mut again);
        assert!(again.is_empty());
    }
}
