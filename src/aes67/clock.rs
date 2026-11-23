//! The grandmaster's time, estimated from Sync/Follow_Up pairs.
//!
//! `Estimator` is pure: it is fed packets with their kernel receive time
//! (CLOCK_REALTIME ns) and keeps a sliding window of (receive time, offset)
//! pairs, where offset = grandmaster time - local time. A least-squares line
//! through the window gives the offset now and the rate. `PtpClock` is the
//! shared, published view the AES67 threads read.
//!
//! The owner (the listener thread) calls `Estimator::tick` now and then, so
//! loss is noticed without traffic, and `PtpClock::publish` after each packet
//! or tick. All PTP arithmetic is relative; the PTP timescale is arbitrary.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use super::ptp::{self, Announce, ClockId, Kind};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtpNow {
    pub local_ns: i128,
    pub ptp_ns: i128,
    pub gm: ClockId,
    pub domain: u8,
    /// PTP seconds per local second.
    pub rate: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EstimatorStats {
    pub pairs: u64,
    pub unmatched_syncs: u64,
    pub gm_addr: Option<Ipv4Addr>,
    pub jitter_ns: f64,
}

/// Pairs older than this are dropped from the window.
const WINDOW_NS: i128 = 16_000_000_000;
/// No pair for this long: PTP time is lost.
const LOSS_NS: i128 = 5_000_000_000;
/// A Sync waits this long for its Follow_Up.
const PENDING_NS: i128 = 1_000_000_000;
/// A pair this far off the fit is suspect; `STEP_PAIRS` in a row mean the
/// local clock (or the grandmaster) stepped, and the window is discarded.
const STEP_NS: i128 = 10_000_000;
const STEP_PAIRS: usize = 3;
/// ... and they must agree on the new offset within this.
const STEP_AGREE_NS: i128 = 1_000_000;
/// The fit needs this many pairs spanning this long before it is used.
const MIN_PAIRS: usize = 8;
const MIN_SPAN_NS: i128 = 2_000_000_000;
/// Floor for the MAD, so a noiseless master does not reject everything.
const MIN_MAD_NS: f64 = 2_000.0;
const MAX_PENDING: usize = 8;

/// The fitted line: offset(local) = base_off + slope * (local - base_local).
#[derive(Debug, Clone, Copy)]
struct Fit {
    base_local: i128,
    base_off: i128,
    slope: f64,
    jitter_ns: f64,
}

#[derive(Clone, Copy)]
struct Snapshot {
    fit: Fit,
    gm: ClockId,
    domain: u8,
    last_pair_ns: i128,
}

impl Fit {
    fn offset_at(&self, local_ns: i128) -> i128 {
        let dt = (local_ns - self.base_local) as f64;
        self.base_off + (self.slope * dt).round() as i128
    }
}

impl Snapshot {
    fn at(&self, local_ns: i128) -> Option<PtpNow> {
        if local_ns - self.last_pair_ns >= LOSS_NS {
            return None;
        }
        let offset = self.fit.offset_at(local_ns);
        Some(PtpNow {
            local_ns,
            ptp_ns: local_ns + offset,
            gm: self.gm,
            domain: self.domain,
            rate: 1.0 + self.fit.slope,
        })
    }
}

struct Master {
    src: ClockId,
    addr: Ipv4Addr,
    ann: Announce,
    interval_ns: i128,
    last_rx: i128,
}

struct Pending {
    addr: Ipv4Addr,
    seq: u16,
    rx: i128,
    correction_ns: i64,
}

pub struct Estimator {
    domain: u8,
    masters: Vec<Master>,
    /// Source clock ID of the master being followed.
    followed: Option<ClockId>,
    /// The grandmaster that master announced when it was chosen.
    followed_gm: Option<ClockId>,
    pending: Vec<Pending>,
    window: Vec<(i128, i128)>,
    /// Consecutive pairs far off the fit, held until they are a step or not.
    suspects: Vec<(i128, i128)>,
    fit: Option<Fit>,
    last_pair_rx: i128,
    generation: u64,
    pairs: u64,
    unmatched: u64,
}

impl Estimator {
    pub fn new(domain: u8) -> Estimator {
        Estimator {
            domain,
            masters: Vec::new(),
            followed: None,
            followed_gm: None,
            pending: Vec::new(),
            window: Vec::new(),
            suspects: Vec::new(),
            fit: None,
            last_pair_rx: 0,
            generation: 0,
            pairs: 0,
            unmatched: 0,
        }
    }

    /// One packet off the wire: its source address, UDP port, payload and
    /// kernel receive time (CLOCK_REALTIME ns). The port is not needed:
    /// messages are told apart by their type.
    pub fn packet(&mut self, from: Ipv4Addr, _port: u16, payload: &[u8], rx_ns: i128) {
        let Some(h) = ptp::header(payload) else {
            return;
        };
        if h.domain != self.domain {
            return;
        }
        self.tick(rx_ns);
        match h.kind {
            Kind::Announce => {
                if let Some(a) = ptp::announce(payload) {
                    // logMessageInterval is byte 33; 0 (1 s) if absurd.
                    let log = payload[33] as i8;
                    let log = if (-4..=6).contains(&log) { log } else { 0 };
                    let interval_ns = 1_000_000_000i128 << log.max(0) >> (-log).max(0);
                    let m = Master {
                        src: h.source,
                        addr: from,
                        ann: a,
                        interval_ns,
                        last_rx: rx_ns,
                    };
                    match self.masters.iter_mut().find(|x| x.src == h.source) {
                        Some(slot) => *slot = m,
                        None => self.masters.push(m),
                    }
                    self.reselect(rx_ns);
                }
            }
            Kind::Sync => {
                if !h.two_step || self.followed != Some(h.source) {
                    return;
                }
                self.pending.retain(|p| !(p.addr == from && p.seq == h.seq));
                if self.pending.len() >= MAX_PENDING {
                    self.pending.remove(0);
                    self.unmatched += 1;
                }
                self.pending.push(Pending {
                    addr: from,
                    seq: h.seq,
                    rx: rx_ns,
                    correction_ns: h.correction_ns,
                });
            }
            Kind::FollowUp => {
                if self.followed != Some(h.source) {
                    return;
                }
                let Some(i) = self
                    .pending
                    .iter()
                    .position(|p| p.addr == from && p.seq == h.seq)
                else {
                    return;
                };
                let sync = self.pending.remove(i);
                let Some(origin) = ptp::origin_ns(payload) else {
                    return;
                };
                let offset =
                    origin + h.correction_ns as i128 + sync.correction_ns as i128 - sync.rx;
                self.add_pair(sync.rx, offset);
            }
            Kind::Other => {}
        }
    }

    /// Time passes without traffic: expire Syncs, stale masters and lost
    /// lock. `packet` does this itself; the owner also calls it on a timer.
    pub fn tick(&mut self, now_ns: i128) {
        let before = self.pending.len();
        self.pending.retain(|p| now_ns - p.rx <= PENDING_NS);
        self.unmatched += (before - self.pending.len()) as u64;
        self.reselect(now_ns);
        if !self.window.is_empty() && now_ns - self.last_pair_rx >= LOSS_NS {
            self.reset();
        }
    }

    pub fn now(&self, local_ns: i128) -> Option<PtpNow> {
        self.snapshot()?.at(local_ns)
    }

    /// +1 on every grandmaster change, loss of lock and local clock step.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn stats(&self) -> EstimatorStats {
        let followed = self.followed_master();
        EstimatorStats {
            pairs: self.pairs,
            unmatched_syncs: self.unmatched,
            gm_addr: followed.map(|m| m.addr),
            jitter_ns: self.fit.map_or(0.0, |f| f.jitter_ns),
        }
    }

    fn followed_master(&self) -> Option<&Master> {
        let src = self.followed?;
        self.masters.iter().find(|m| m.src == src)
    }

    fn snapshot(&self) -> Option<Snapshot> {
        let fit = self.fit?;
        let master = self.followed_master()?;
        Some(Snapshot {
            fit,
            gm: master.ann.gm,
            domain: self.domain,
            last_pair_ns: self.last_pair_rx,
        })
    }

    /// Drop masters silent for 3 Announce intervals, then follow the best
    /// of the rest (keeping the current one on a tie). A change of master
    /// discards everything learned from the old one.
    fn reselect(&mut self, now_ns: i128) {
        self.masters
            .retain(|m| now_ns - m.last_rx <= 3 * m.interval_ns);
        let mut best = self.followed_master();
        for m in &self.masters {
            if best.is_none_or(|b| ptp::better(&m.ann, &b.ann)) {
                best = Some(m);
            }
        }
        let chosen = best.map(|m| (m.src, m.ann.gm));
        if chosen != self.followed.zip(self.followed_gm) {
            self.followed = chosen.map(|c| c.0);
            self.followed_gm = chosen.map(|c| c.1);
            self.reset();
        }
    }

    /// Discard the window and the Syncs waiting for a Follow_Up.
    fn reset(&mut self) {
        self.window.clear();
        self.suspects.clear();
        self.pending.clear();
        self.fit = None;
        self.generation += 1;
    }

    fn add_pair(&mut self, rx: i128, offset: i128) {
        self.pairs += 1;
        self.last_pair_rx = rx;
        if let Some(fit) = self.fit {
            let dev = |p: &(i128, i128)| p.1 - fit.offset_at(p.0);
            if dev(&(rx, offset)).abs() > STEP_NS {
                self.suspects.push((rx, offset));
                if self.suspects.len() < STEP_PAIRS {
                    return;
                }
                // A step moves every pair by the same amount (the fit already
                // accounts for the rate); delayed or garbage pairs do not agree.
                // Disagreeing suspects are outliers: forget the oldest, keep the window.
                let devs: Vec<i128> = self.suspects.iter().map(dev).collect();
                let spread = devs.iter().max().unwrap() - devs.iter().min().unwrap();
                if spread > STEP_AGREE_NS {
                    self.suspects.remove(0);
                    return;
                }
                // The clock stepped: start over from the pairs after the step.
                let kept = std::mem::take(&mut self.suspects);
                self.reset();
                self.window = kept;
                return;
            }
        }
        self.suspects.clear();
        self.window.push((rx, offset));
        self.window.retain(|p| rx - p.0 <= WINDOW_NS);
        self.fit = fit_window(&self.window);
    }
}

/// Least squares through (x, y), returning slope and the means.
fn least_squares(points: &[(f64, f64)]) -> (f64, f64, f64) {
    let n = points.len() as f64;
    let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    (if sxx > 0.0 { sxy / sxx } else { 0.0 }, mx, my)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Fit offset over local time: a first line through everything, then drop
/// pairs more than 3 MAD from it and fit again.
fn fit_window(window: &[(i128, i128)]) -> Option<Fit> {
    let first = *window.first()?;
    let last = *window.last()?;
    if window.len() < MIN_PAIRS || last.0 - first.0 < MIN_SPAN_NS {
        return None;
    }
    let pts: Vec<(f64, f64)> = window
        .iter()
        .map(|p| ((p.0 - first.0) as f64, (p.1 - first.1) as f64))
        .collect();
    let (slope, mx, my) = least_squares(&pts);
    let mut residuals: Vec<f64> = pts.iter().map(|p| p.1 - my - slope * (p.0 - mx)).collect();
    let center = median(&mut residuals.clone());
    let mut dev: Vec<f64> = residuals.iter().map(|r| (r - center).abs()).collect();
    let mad = median(&mut dev);
    let limit = 3.0 * mad.max(MIN_MAD_NS);
    let kept: Vec<(f64, f64)> = pts
        .iter()
        .zip(residuals.drain(..))
        .filter(|(_, r)| (r - center).abs() <= limit)
        .map(|(p, _)| *p)
        .collect();
    if kept.len() < MIN_PAIRS {
        return None;
    }
    let (slope, mx, my) = least_squares(&kept);
    Some(Fit {
        base_local: first.0 + mx.round() as i128,
        base_off: first.1 + my.round() as i128,
        slope,
        jitter_ns: mad,
    })
}

struct Shared {
    snapshot: Option<Snapshot>,
    generation: u64,
}

/// The grandmaster's time, as the AES67 threads read it. The estimator's
/// owner calls `publish` after each packet or tick; readers call `now`.
#[derive(Clone)]
pub struct PtpClock(Arc<Mutex<Shared>>);

impl PtpClock {
    /// A clock that never has PTP time.
    pub fn off() -> PtpClock {
        PtpClock(Arc::new(Mutex::new(Shared {
            snapshot: None,
            generation: 0,
        })))
    }

    /// Copy the estimator's current fit and generation into the clock.
    pub fn publish(&self, est: &Estimator) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.snapshot = est.snapshot();
        s.generation = est.generation();
    }

    /// The grandmaster's time now, reading CLOCK_REALTIME itself.
    pub fn now(&self) -> Option<PtpNow> {
        self.now_at(realtime_ns())
    }

    fn now_at(&self, local_ns: i128) -> Option<PtpNow> {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.snapshot?.at(local_ns)
    }

    /// +1 on every grandmaster change, loss of lock and local clock step.
    pub fn generation(&self) -> u64 {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).generation
    }

    /// `now` and `generation` together, under one lock, so the two agree.
    pub fn snapshot(&self) -> (Option<PtpNow>, u64) {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.snapshot.and_then(|snap| snap.at(realtime_ns())),
            s.generation,
        )
    }
}

pub(crate) fn realtime_ns() -> i128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec for the call; CLOCK_REALTIME always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aes67::ptp::{announce_bytes, follow_up_bytes, sync_bytes};

    const MASTER: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55];
    const OTHER: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x66];
    const ADDR: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const ADDR2: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
    const L0: i128 = 1_770_000_000_000_000_000;
    const M0: i128 = L0 - 1_700_000_000_000_000_000;
    const PPM_NUM: i128 = 10_000_163; // 16.3 ppm fast
    const PPM_DEN: i128 = 10_000_000;
    const SYNC_NS: i128 = 125_000_000;

    fn ann(gm: ClockId, priority1: u8) -> Announce {
        Announce {
            gm,
            priority1,
            class: 248,
            accuracy: 254,
            variance: 61536,
            priority2: 112,
            steps_removed: 0,
        }
    }

    /// A master at +16.3 ppm with a deterministic +-20 us receive jitter.
    struct Sim {
        addr: Ipv4Addr,
        id: ClockId,
        domain: u8,
        priority1: u8,
        gm: ClockId,
        k: i128,
        seq: u16,
        rng: u64,
        /// Added to every receive time: the local clock was stepped.
        local_step: i128,
    }

    impl Sim {
        fn new(addr: Ipv4Addr, id: ClockId) -> Sim {
            Sim {
                addr,
                id,
                domain: 0,
                priority1: 128,
                gm: id,
                k: 0,
                seq: 0,
                rng: 12345,
                local_step: 0,
            }
        }

        fn jitter(&mut self) -> i128 {
            self.rng = self
                .rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.rng >> 33) % 40_001) as i128 - 20_000
        }

        /// True local time (before any step) of the next Sync.
        fn local_now(&self) -> i128 {
            L0 + self.k * SYNC_NS * PPM_DEN / PPM_NUM
        }

        /// What the master's time is at true local time `l`.
        fn truth(l: i128) -> i128 {
            M0 + (l - L0) * PPM_NUM / PPM_DEN
        }

        /// One Sync/Follow_Up pair, plus an Announce every 8th.
        fn step(&mut self, est: &mut Estimator, announce: bool, follow_up: bool) {
            let l = self.local_now();
            let m = M0 + self.k * SYNC_NS;
            let j = self.jitter();
            self.send_pair(est, l, m, j, announce, follow_up);
        }

        fn send_pair(
            &mut self,
            est: &mut Estimator,
            l: i128,
            m: i128,
            j: i128,
            announce: bool,
            follow_up: bool,
        ) {
            let rx = l + j + self.local_step;
            if announce && self.k % 8 == 0 {
                let a = announce_bytes(
                    self.domain,
                    self.id,
                    self.seq,
                    &ann(self.gm, self.priority1),
                );
                est.packet(self.addr, 320, &a, rx - 100_000);
            }
            let s = sync_bytes(self.domain, self.id, self.seq);
            est.packet(self.addr, 319, &s, rx);
            if follow_up {
                let secs = (m / 1_000_000_000) as u64;
                let nanos = (m % 1_000_000_000) as u32;
                let f = follow_up_bytes(self.domain, self.id, self.seq, secs, nanos, 0);
                est.packet(self.addr, 320, &f, rx + 1_000_000);
            }
            self.k += 1;
            self.seq = self.seq.wrapping_add(1);
        }

        fn run(&mut self, est: &mut Estimator, seconds: i128) {
            let n = seconds * 1_000_000_000 / SYNC_NS;
            for _ in 0..n {
                self.step(est, true, true);
            }
        }
    }

    fn err_ns(est: &Estimator, sim: &Sim) -> i128 {
        // Compare at the time just after the last Sync, in the stepped clock.
        let l = sim.local_now();
        let now = est.now(l + sim.local_step).expect("locked");
        now.ptp_ns - Sim::truth(l)
    }

    #[test]
    fn locks_after_pairs_and_follows_the_rate() {
        let mut est = Estimator::new(0);
        let mut sim = Sim::new(ADDR, MASTER);
        assert!(est.now(L0).is_none());
        sim.run(&mut est, 8);
        let now = est.now(sim.local_now()).expect("locked");
        assert!((now.rate - 1.0000163).abs() < 2e-6, "rate {}", now.rate);
        assert!(
            err_ns(&est, &sim).abs() < 50_000,
            "err {}",
            err_ns(&est, &sim)
        );
        assert_eq!((now.gm, now.domain), (MASTER, 0));
        assert_eq!(est.stats().gm_addr, Some(ADDR));
        assert!(est.stats().pairs >= 60);
        sim.run(&mut est, 8);
        let now = est.now(sim.local_now()).expect("locked");
        assert!((now.rate - 1.0000163).abs() < 0.5e-6, "rate {}", now.rate);
    }

    #[test]
    fn a_sync_without_its_follow_up_is_dropped() {
        let mut est = Estimator::new(0);
        let mut sim = Sim::new(ADDR, MASTER);
        sim.run(&mut est, 1);
        let before = est.stats();
        sim.step(&mut est, false, false);
        // The next exchange has a different sequence; the lone Sync stays unused.
        sim.step(&mut est, false, true);
        let mid = est.stats();
        assert_eq!(mid.pairs, before.pairs + 1);
        // A Follow_Up from a different address pairs with nothing.
        let seq = sim.seq.wrapping_sub(2);
        let f = follow_up_bytes(0, MASTER, seq, 1, 0, 0);
        let t = sim.local_now() + sim.local_step;
        est.packet(ADDR2, 320, &f, t);
        assert_eq!(est.stats().pairs, mid.pairs);
        // The Sync waits about a second; a Follow_Up after that is too late,
        // and the Sync is counted as unmatched.
        est.packet(ADDR, 320, &f, t + 1_500_000_000);
        assert_eq!(est.stats().pairs, mid.pairs);
        assert_eq!(est.stats().unmatched_syncs, before.unmatched_syncs + 1);
    }

    #[test]
    fn other_domains_and_one_step_are_ignored() {
        let mut est = Estimator::new(0);
        let mut other = Sim::new(ADDR, MASTER);
        other.domain = 4;
        other.run(&mut est, 3);
        assert_eq!(est.stats().pairs, 0);
        assert!(est.now(other.local_now()).is_none());

        // One-step: the Sync does not say two-step, and the Follow_Up never comes.
        let mut est = Estimator::new(0);
        let a = announce_bytes(0, MASTER, 0, &ann(MASTER, 128));
        est.packet(ADDR, 320, &a, L0);
        for seq in 0..16u16 {
            let mut s = sync_bytes(0, MASTER, seq);
            s[6] = 0;
            let rx = L0 + seq as i128 * SYNC_NS;
            est.packet(ADDR, 319, &s, rx);
            let f = follow_up_bytes(0, MASTER, seq, 1, 0, 0);
            est.packet(ADDR, 320, &f, rx + 1_000_000);
        }
        assert_eq!(est.stats().pairs, 0);
        assert!(est.now(L0 + 2_000_000_000).is_none());
    }

    #[test]
    fn the_best_announce_is_followed() {
        let mut est = Estimator::new(0);
        let mut good = Sim::new(ADDR, MASTER);
        good.priority1 = 100;
        let mut poor = Sim::new(ADDR2, OTHER);
        poor.priority1 = 200;
        // The poor master runs 1 s ahead in its own timescale.
        for _ in 0..80 {
            let l = good.local_now();
            poor.k = good.k;
            poor.seq = good.seq;
            poor.send_pair(
                &mut est,
                l,
                M0 + good.k * SYNC_NS + 1_000_000_000,
                5_000,
                true,
                true,
            );
            good.step(&mut est, true, true);
        }
        let now = est.now(good.local_now()).expect("locked");
        assert_eq!(now.gm, MASTER);
        assert_eq!(est.stats().gm_addr, Some(ADDR));
        assert!(err_ns(&est, &good).abs() < 50_000);
    }

    #[test]
    fn a_better_master_takes_over_and_the_old_one_expires() {
        // A better master appears.
        let mut est = Estimator::new(0);
        let mut poor = Sim::new(ADDR2, OTHER);
        poor.priority1 = 200;
        poor.run(&mut est, 6);
        assert_eq!(est.now(poor.local_now()).unwrap().gm, OTHER);
        let gen_poor = est.generation();
        let mut good = Sim::new(ADDR, MASTER);
        good.priority1 = 100;
        good.k = poor.k;
        good.seq = poor.seq;
        for _ in 0..16 {
            poor.step(&mut est, true, true);
            good.step(&mut est, true, true);
        }
        assert_ne!(est.generation(), gen_poor);
        let gen_good = est.generation();
        for _ in 0..80 {
            poor.step(&mut est, true, true);
            good.step(&mut est, true, true);
        }
        assert_eq!(est.generation(), gen_good);
        assert_eq!(est.now(good.local_now()).unwrap().gm, MASTER);

        // The followed master falls silent; the other takes over after 3 intervals.
        for _ in 0..8 * 5 {
            poor.step(&mut est, true, true);
            good.k += 1;
            good.seq = good.seq.wrapping_add(1);
        }
        assert_ne!(est.generation(), gen_good);
        assert_eq!(est.stats().gm_addr, Some(ADDR2));
        // The window was discarded; it locks again from the survivor.
        for _ in 0..8 * 4 {
            poor.step(&mut est, true, true);
        }
        assert_eq!(est.now(poor.local_now()).unwrap().gm, OTHER);
    }

    #[test]
    fn five_seconds_without_a_pair_is_none() {
        let mut est = Estimator::new(0);
        let mut sim = Sim::new(ADDR, MASTER);
        sim.run(&mut est, 8);
        let clock = PtpClock::off();
        clock.publish(&est);
        let last = sim.local_now();
        assert!(est.now(last + 4_000_000_000).is_some());
        assert!(est.now(last + 6_000_000_000).is_none());
        let gen_locked = est.generation();
        est.tick(last + 6_000_000_000);
        assert_eq!(est.generation(), gen_locked + 1);
        est.tick(last + 7_000_000_000);
        assert_eq!(est.generation(), gen_locked + 1);
        assert!(est.now(last + 7_000_000_000).is_none());
        clock.publish(&est);
        assert!(clock.now().is_none());
        assert_eq!(clock.generation(), est.generation());
    }

    #[test]
    fn outliers_do_not_move_the_estimate() {
        let mut est = Estimator::new(0);
        let mut sim = Sim::new(ADDR, MASTER);
        sim.run(&mut est, 8);
        let t = sim.local_now();
        let before = est.now(t).unwrap().ptp_ns;
        let gen_before = est.generation();
        // One pair received 2 ms late.
        let (l, m) = (sim.local_now(), M0 + sim.k * SYNC_NS);
        sim.send_pair(&mut est, l, m, 2_000_000, false, true);
        let after = est.now(t).unwrap().ptp_ns;
        assert_eq!(est.generation(), gen_before);
        assert!((after - before).abs() < 5_000, "moved {}", after - before);
        // And it keeps tracking afterwards.
        sim.run(&mut est, 2);
        assert!(err_ns(&est, &sim).abs() < 50_000);
    }

    #[test]
    fn a_local_clock_step_relocks() {
        let mut est = Estimator::new(0);
        let mut sim = Sim::new(ADDR, MASTER);
        sim.run(&mut est, 10);
        let gen_before = est.generation();
        sim.local_step = 500_000_000;
        sim.run(&mut est, 8);
        assert_ne!(est.generation(), gen_before);
        assert!(
            err_ns(&est, &sim).abs() < 50_000,
            "err {}",
            err_ns(&est, &sim)
        );
        let now = est.now(sim.local_now() + sim.local_step).unwrap();
        assert!((now.rate - 1.0000163).abs() < 2e-6, "rate {}", now.rate);
    }

    #[test]
    fn a_single_pair_far_off_is_ignored() {
        let mut est = Estimator::new(0);
        let mut sim = Sim::new(ADDR, MASTER);
        sim.run(&mut est, 8);
        let t = sim.local_now();
        let before = est.now(t).unwrap().ptp_ns;
        let gen_before = est.generation();
        let (l, m) = (sim.local_now(), M0 + sim.k * SYNC_NS);
        sim.send_pair(&mut est, l, m, 50_000_000, false, true);
        assert_eq!(est.generation(), gen_before);
        assert_eq!(est.now(t).unwrap().ptp_ns, before);
        sim.run(&mut est, 1);
        assert_eq!(est.generation(), gen_before);
    }

    #[test]
    fn a_burst_of_disagreeing_bad_pairs_is_not_a_step() {
        let mut est = Estimator::new(0);
        let mut sim = Sim::new(ADDR, MASTER);
        sim.run(&mut est, 8);
        let t = sim.local_now();
        let before = est.now(t).unwrap().ptp_ns;
        let gen_before = est.generation();
        for late in [15_000_000, 40_000_000, 90_000_000] {
            let (l, m) = (sim.local_now(), M0 + sim.k * SYNC_NS);
            sim.send_pair(&mut est, l, m, late, false, true);
        }
        assert_eq!(est.generation(), gen_before);
        assert_eq!(est.now(t).unwrap().ptp_ns, before);
        sim.run(&mut est, 1);
        assert!(err_ns(&est, &sim).abs() < 50_000);
        assert_eq!(est.generation(), gen_before);
    }

    #[test]
    fn a_new_grandmaster_on_the_same_source_discards_the_window() {
        let mut est = Estimator::new(0);
        let mut sim = Sim::new(ADDR, MASTER);
        sim.run(&mut est, 6);
        let gen_before = est.generation();
        sim.gm = OTHER;
        let a = announce_bytes(0, MASTER, 99, &ann(OTHER, 128));
        est.packet(ADDR, 320, &a, sim.local_now());
        assert_ne!(est.generation(), gen_before);
        assert!(est.now(sim.local_now()).is_none());
        let gen_after = est.generation();
        // Unchanged repeats do not reset again; the clock locks afresh.
        sim.run(&mut est, 1);
        est.packet(ADDR, 320, &a, sim.local_now());
        sim.run(&mut est, 4);
        assert_eq!(est.generation(), gen_after);
        assert_eq!(est.now(sim.local_now()).unwrap().gm, OTHER);
    }

    #[test]
    fn a_short_announce_interval_expires_a_silent_master() {
        let mut est = Estimator::new(0);
        let mut a = announce_bytes(0, MASTER, 1, &ann(MASTER, 128));
        a[33] = (-2i8) as u8; // 250 ms; three intervals are 750 ms
        est.packet(ADDR, 320, &a, L0);
        assert_eq!(est.stats().gm_addr, Some(ADDR));
        let g = est.generation();
        est.tick(L0 + 700_000_000);
        assert_eq!(est.stats().gm_addr, Some(ADDR));
        est.tick(L0 + 800_000_000);
        assert_eq!(est.stats().gm_addr, None);
        assert_ne!(est.generation(), g);
    }
}
