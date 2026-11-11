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

    /// The rate a settle switch would move to, if one is needed.
    ///
    /// A switch is needed when no active client is at the output rate. It
    /// then goes to the nearest rate of the oldest active client.
    fn settle_target(&self) -> Option<u32> {
        if self.clients.iter().any(|c| c.rate == self.base) {
            return None;
        }
        let target = self.nearest(self.clients.first()?.rate);
        (target != self.base).then_some(target)
    }

    /// Keeps a pending switch only while it is still needed.
    fn drop_unneeded_switch(&mut self) {
        if self.settle_target().is_none() {
            self.switch_at = None;
        }
    }

    /// A client starts or resumes playing at `rate`.
    ///
    /// A start for an id that is already active only updates its rate. It
    /// keeps its place in the order.
    pub fn client_started(&mut self, id: ClientId, rate: u32, _now: Duration) {
        if let Some(c) = self.clients.iter_mut().find(|c| c.id == id) {
            c.rate = rate;
        } else {
            if self.clients.is_empty() {
                self.base = self.nearest(rate);
            }
            self.clients.push(Active { id, rate });
        }
        self.drop_unneeded_switch();
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
        if self.settle_target().is_some() {
            self.switch_at.get_or_insert(now + SETTLE);
        } else {
            self.switch_at = None;
        }
    }

    /// The client's stream is DoP. The output runs at its rate until it stops.
    ///
    /// A DoP stream at a rate the card does not support does not take effect.
    /// The output rate stays as it was, so the engine keeps that stream muted
    /// because its rate does not match.
    pub fn dop_started(&mut self, id: ClientId) {
        if self.rate_of(id).is_some_and(|r| self.is_supported(r)) {
            self.dop = Some(id);
        }
    }

    fn is_supported(&self, rate: u32) -> bool {
        self.supported.is_empty() || self.supported.contains(&rate)
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
        if let Some(target) = self.settle_target() {
            self.base = target;
        }
    }

    pub fn set_supported(&mut self, rates: Vec<u32>) {
        self.supported = rates;
        self.base = self.nearest(self.base);
        self.drop_unneeded_switch();
    }

    /// The output rate.
    pub fn current(&self) -> u32 {
        self.dop
            .and_then(|id| self.rate_of(id))
            .filter(|r| self.is_supported(*r))
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
    fn an_unsupported_rate_uses_the_nearest_and_resamples() {
        let mut p = policy();
        p.client_started(1, 88200, T0);
        assert_eq!(p.current(), 96000);
        assert!(p.needs_resampling(1));
    }

    #[test]
    fn a_rate_halfway_between_two_supported_ones_uses_the_lower() {
        // 46050 is 1950 from both 44100 and 48000.
        let mut p = policy();
        p.client_started(1, 46050, T0);
        assert_eq!(p.current(), 44100);
        assert_eq!(p.nearest(46050), 44100);
        assert_eq!(p.nearest(46051), 48000);
        // With no rates known, every rate is taken as it is.
        assert_eq!(RatePolicy::new(48000, Vec::new()).nearest(46050), 46050);
    }

    #[test]
    fn new_supported_rates_move_the_output_to_the_nearest_at_once() {
        let mut p = RatePolicy::new(48000, Vec::new());
        p.client_started(1, 88200, T0);
        assert_eq!(p.current(), 88200);
        p.set_supported(RATES.to_vec());
        assert_eq!(p.current(), 96000);
        assert!(p.needs_resampling(1));
        // A rate that is still supported stays.
        p.set_supported(vec![48000, 96000]);
        assert_eq!(p.current(), 96000);
    }

    #[test]
    fn new_supported_rates_drop_a_switch_that_is_no_longer_needed() {
        let mut p = RatePolicy::new(48000, vec![44100, 96000]);
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_stopped(1, T0);
        // The card now takes 96000 only, so the output is there already.
        p.set_supported(vec![96000]);
        assert_eq!(p.current(), 96000);
        assert!(p.switch_at.is_none());
        // And a DoP rate the card no longer takes does not hold the output.
        p.dop_started(2);
        p.set_supported(vec![44100]);
        assert_eq!(p.current(), 44100);
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

    #[test]
    fn three_clients_first_stops_and_the_others_share_their_rate() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_started(3, 96000, T0);
        p.client_stopped(1, T0);
        p.tick(T0 + ms(499));
        assert_eq!(p.current(), 44100);
        p.tick(T0 + ms(500));
        assert_eq!(p.current(), 96000);
        assert!(!p.needs_resampling(2));
        assert!(!p.needs_resampling(3));
    }

    #[test]
    fn the_oldest_remaining_client_sets_the_rate() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_started(3, 48000, T0);
        p.client_stopped(1, T0);
        p.tick(T0 + ms(500));
        assert_eq!(p.current(), 96000);
        assert!(!p.needs_resampling(2));
        assert!(p.needs_resampling(3));
    }

    #[test]
    fn a_remaining_client_at_the_output_rate_schedules_no_switch() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_started(3, 44100, T0);
        p.client_stopped(2, T0);
        p.tick(T0 + ms(5000));
        assert_eq!(p.current(), 44100);
    }

    #[test]
    fn a_client_at_the_output_rate_arriving_during_settle_cancels_the_switch() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_stopped(1, T0);
        p.client_started(3, 44100, T0 + ms(100));
        p.tick(T0 + ms(600));
        assert_eq!(p.current(), 44100);
    }

    #[test]
    fn a_duplicate_start_does_not_skip_the_settle_time() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_stopped(1, T0);
        p.client_started(2, 96000, T0 + ms(100));
        assert_eq!(p.current(), 44100);
        p.tick(T0 + ms(500));
        assert_eq!(p.current(), 96000);
    }

    #[test]
    fn dop_at_an_unsupported_rate_leaves_the_rate_alone() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 88200, T0);
        p.dop_started(2);
        assert_eq!(p.current(), 44100);
    }

    #[test]
    fn a_stop_for_an_unknown_id_changes_nothing() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_stopped(9, T0);
        p.tick(T0 + ms(5000));
        assert_eq!(p.current(), 44100);
        assert!(p.needs_resampling(2));
    }

    #[test]
    fn the_lone_client_stopping_during_settle_leaves_the_rate_unchanged() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 96000, T0);
        p.client_stopped(1, T0);
        p.client_stopped(2, T0 + ms(100));
        p.tick(T0 + ms(5000));
        assert_eq!(p.current(), 44100);
    }

    #[test]
    fn the_dop_client_stopping_while_others_remain_restores_the_normal_rules() {
        let mut p = policy();
        p.client_started(1, 44100, T0);
        p.client_started(2, 192000, T0);
        p.dop_started(2);
        p.client_stopped(2, T0);
        assert_eq!(p.current(), 44100);
        assert!(!p.needs_resampling(1));
    }
}
