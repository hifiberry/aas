//! Sample rate rules. A pure state machine: no I/O, time is an argument.

use std::time::Duration;

/// Identifies a client to the rate rules.
pub type ClientId = u64;

/// How long a lone remaining client waits before the output follows its rate.
pub const SETTLE: Duration = Duration::from_millis(500);

struct Active {
    id: ClientId,
    rate: u32,
}

/// Decides the output sample rate from the active clients.
pub struct RatePolicy {
    supported: Vec<u32>,
    /// The rate set by the ordinary rules, ignoring DoP.
    base: u32,
    clients: Vec<Active>,
    dop: Option<ClientId>,
    /// When a switch to the remaining client's rate is due.
    switch_at: Option<Duration>,
}

impl RatePolicy {
    pub fn new(default_rate: u32, supported: Vec<u32>) -> Self {
        RatePolicy {
            supported,
            base: default_rate,
            clients: Vec::new(),
            dop: None,
            switch_at: None,
        }
    }

    /// The nearest supported rate, or the rate itself if none are known.
    fn nearest(&self, rate: u32) -> u32 {
        self.supported
            .iter()
            .copied()
            .min_by_key(|s| (s.abs_diff(rate), *s))
            .unwrap_or(rate)
    }

    fn rate_of(&self, id: ClientId) -> Option<u32> {
        self.clients.iter().find(|c| c.id == id).map(|c| c.rate)
    }

    /// A client starts or resumes playing at `rate`.
    pub fn client_started(&mut self, id: ClientId, rate: u32, _now: Duration) {
        self.clients.retain(|c| c.id != id);
        if self.clients.is_empty() {
            self.base = self.nearest(rate);
        }
        self.clients.push(Active { id, rate });
        self.switch_at = None;
    }

    /// A client stops or pauses.
    pub fn client_stopped(&mut self, id: ClientId, now: Duration) {
        let before = self.clients.len();
        self.clients.retain(|c| c.id != id);
        if self.dop == Some(id) {
            self.dop = None;
        }
        if self.clients.len() == before {
            return;
        }
        match self.clients.as_slice() {
            [] => self.switch_at = None,
            [only] if self.nearest(only.rate) != self.base => {
                self.switch_at = Some(now + SETTLE);
            }
            _ => self.switch_at = None,
        }
    }

    /// The client's stream is DoP. The output runs at its rate until it stops.
    pub fn dop_started(&mut self, id: ClientId) {
        if self.rate_of(id).is_some() {
            self.dop = Some(id);
        }
    }

    pub fn dop_stopped(&mut self, id: ClientId) {
        if self.dop == Some(id) {
            self.dop = None;
        }
    }

    /// Applies a due settle switch.
    pub fn tick(&mut self, now: Duration) {
        let Some(at) = self.switch_at else { return };
        if now < at {
            return;
        }
        self.switch_at = None;
        if let [only] = self.clients.as_slice() {
            self.base = self.nearest(only.rate);
        }
    }

    pub fn set_supported(&mut self, rates: Vec<u32>) {
        self.supported = rates;
        self.base = self.nearest(self.base);
    }

    /// The output rate.
    pub fn current(&self) -> u32 {
        self.dop
            .and_then(|id| self.rate_of(id))
            .unwrap_or(self.base)
    }

    /// Whether the client's audio must be resampled to the output rate.
    pub fn needs_resampling(&self, id: ClientId) -> bool {
        self.rate_of(id).is_some_and(|r| r != self.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::from_secs(10);
    const RATES: [u32; 4] = [44100, 48000, 96000, 192000];

    fn policy() -> RatePolicy {
        RatePolicy::new(48000, RATES.to_vec())
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_default_applies_only_until_the_first_client() {
        let mut p = policy();
        assert_eq!(p.current(), 48000);
        p.client_started(1, 44100, T0);
        assert_eq!(p.current(), 44100);
    }

    #[test]
    fn one_client_sets_the_rate() {
        let mut p = policy();
        p.client_started(1, 96000, T0);
        assert_eq!(p.current(), 96000);
        assert!(!p.needs_resampling(1));
    }

    #[test]
    fn two_clients_at_the_same_rate_need_no_resampling() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 44100, T0);
        assert_eq!(p.current(), 44100);
        assert!(!p.needs_resampling(1));
        assert!(!p.needs_resampling(2));
    }

    #[test]
    fn with_different_rates_the_first_wins() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        assert_eq!(p.current(), 44100);
        assert!(!p.needs_resampling(1));
        assert!(p.needs_resampling(2));
    }

    #[test]
    fn the_rate_stays_when_idle() {
        let mut p = policy();
        p.client_started(1, 96000, T0);
        p.client_stopped(1, T0 + ms(100));
        p.tick(T0 + ms(5000));
        assert_eq!(p.current(), 96000);
    }

    #[test]
    fn the_remaining_client_takes_over_after_the_settle_time() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_stopped(1, T0);
        p.tick(T0 + ms(499));
        assert_eq!(p.current(), 44100);
        p.tick(T0 + ms(500));
        assert_eq!(p.current(), 96000);
        assert!(!p.needs_resampling(2));
    }

    #[test]
    fn a_client_arriving_during_settle_cancels_the_switch() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_stopped(1, T0);
        p.client_started(3, 48000, T0 + ms(100));
        p.tick(T0 + ms(600));
        assert_eq!(p.current(), 44100);
    }

    #[test]
    fn an_unsupported_rate_uses_the_nearest_and_resamples() {
        let mut p = policy();
        p.client_started(1, 88200, T0);
        assert_eq!(p.current(), 96000);
        assert!(p.needs_resampling(1));
    }

    #[test]
    fn a_paused_client_resuming_alongside_another_is_resampled() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_stopped(1, T0);
        p.client_started(1, 44100, T0 + ms(10));
        p.tick(T0 + ms(1000));
        assert_eq!(p.current(), 44100);
        assert!(!p.needs_resampling(1));
        assert!(p.needs_resampling(2));
    }

    #[test]
    fn a_lone_client_restarting_at_a_new_rate_sets_it_at_once() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_stopped(1, T0 + ms(10));
        p.client_started(1, 96000, T0 + ms(20));
        assert_eq!(p.current(), 96000);
        assert!(!p.needs_resampling(1));
    }

    #[test]
    fn dop_forces_its_rate_over_the_first_client() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 192000, T0);
        p.dop_started(2);
        assert_eq!(p.current(), 192000);
        assert!(p.needs_resampling(1));
        assert!(!p.needs_resampling(2));
    }

    #[test]
    fn the_earlier_rate_rules_resume_after_dop_stops() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 192000, T0);
        p.dop_started(2);
        p.dop_stopped(2);
        assert_eq!(p.current(), 44100);
        assert!(!p.needs_resampling(1));
        assert!(p.needs_resampling(2));
    }
}
