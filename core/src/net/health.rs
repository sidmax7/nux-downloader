//! Link health (`ARCHITECTURE.md` §6.4): a pure circuit breaker. A link
//! that keeps failing at connect level is taken out of scheduling for a
//! growing backoff, then given one trial connection; success closes the
//! circuit. This stops a dead tether from burning every block's retry
//! budget, which the per-block avoid rule alone can't do.

use crate::config::EngineConfig;
use std::time::{Duration, Instant};

/// Where a link stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Up,
    /// Skipped by the scheduler until `until`; after that, one trial
    /// attempt at a time is allowed (half-open).
    Degraded {
        until: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkHealth {
    state: LinkState,
    /// Consecutive connect-level failures since the last success.
    failures: u32,
    /// How many times in a row the link has been (re)degraded — the
    /// backoff exponent.
    degrades: u32,
}

impl Default for LinkHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkHealth {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: LinkState::Up,
            failures: 0,
            degrades: 0,
        }
    }

    #[must_use]
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// How many attempts the scheduler may have running on this link at
    /// once: `link_max` when healthy, none while the backoff runs, and a
    /// single trial once it has expired.
    #[must_use]
    pub fn slot_cap(&self, link_max: u32, now: Instant) -> u32 {
        match self.state {
            LinkState::Up => link_max,
            LinkState::Degraded { until } if now < until => 0,
            LinkState::Degraded { .. } => link_max.min(1),
        }
    }

    /// A connect-level failure. Returns the instant the link becomes
    /// eligible for a trial if this failure (re)degraded it.
    pub fn record_failure(&mut self, now: Instant, config: &EngineConfig) -> Option<Instant> {
        self.failures += 1;
        let half_open = matches!(self.state, LinkState::Degraded { until } if now >= until);
        let already_backing_off =
            matches!(self.state, LinkState::Degraded { until } if now < until);
        if already_backing_off {
            // A stale failure from an attempt that was in flight when the
            // link degraded; the backoff already covers it.
            return None;
        }
        if half_open || self.failures >= config.link_degrade_after_failures {
            let backoff = degrade_backoff(config, self.degrades);
            self.degrades += 1;
            let until = now + backoff;
            self.state = LinkState::Degraded { until };
            return Some(until);
        }
        None
    }

    /// Bytes arrived on this link: it works. Closes the circuit.
    pub fn record_success(&mut self) {
        *self = Self::new();
    }
}

/// `min(min × 2^n, max)` (§6.4: 5 s → 60 s).
fn degrade_backoff(config: &EngineConfig, degrades: u32) -> Duration {
    let scaled = config
        .link_degrade_backoff_min
        .saturating_mul(1u32 << degrades.min(16));
    scaled.min(config.link_degrade_backoff_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EngineConfig {
        EngineConfig::default()
    }

    #[test]
    fn a_new_link_is_up_with_all_its_slots() {
        let health = LinkHealth::new();
        assert_eq!(health.state(), LinkState::Up);
        assert_eq!(health.slot_cap(8, Instant::now()), 8);
    }

    #[test]
    fn two_failures_are_tolerated_the_third_degrades() {
        let now = Instant::now();
        let mut health = LinkHealth::new();
        assert!(health.record_failure(now, &config()).is_none());
        assert!(health.record_failure(now, &config()).is_none());
        let until = health.record_failure(now, &config());
        assert_eq!(until, Some(now + Duration::from_secs(5)));
        assert_eq!(health.slot_cap(8, now), 0);
    }

    #[test]
    fn a_success_resets_the_failure_count() {
        let now = Instant::now();
        let mut health = LinkHealth::new();
        health.record_failure(now, &config());
        health.record_failure(now, &config());
        health.record_success();
        assert!(health.record_failure(now, &config()).is_none());
        assert!(health.record_failure(now, &config()).is_none());
    }

    #[test]
    fn after_the_backoff_one_trial_is_allowed() {
        let now = Instant::now();
        let mut health = LinkHealth::new();
        for _ in 0..3 {
            health.record_failure(now, &config());
        }
        let later = now + Duration::from_secs(5);
        assert_eq!(health.slot_cap(8, later), 1);
        assert_eq!(health.slot_cap(1, later), 1);
    }

    #[test]
    fn a_failed_trial_degrades_again_with_a_longer_backoff() {
        let now = Instant::now();
        let mut health = LinkHealth::new();
        for _ in 0..3 {
            health.record_failure(now, &config());
        }
        let trial_at = now + Duration::from_secs(5);
        let until = health.record_failure(trial_at, &config());
        assert_eq!(until, Some(trial_at + Duration::from_secs(10)));
        assert_eq!(health.slot_cap(8, trial_at), 0);
    }

    #[test]
    fn the_backoff_is_capped() {
        let mut now = Instant::now();
        let mut health = LinkHealth::new();
        for _ in 0..3 {
            health.record_failure(now, &config());
        }
        let mut last = Duration::ZERO;
        for _ in 0..10 {
            let LinkState::Degraded { until } = health.state() else {
                panic!("should be degraded");
            };
            now = until;
            let next = health
                .record_failure(now, &config())
                .unwrap_or_else(|| panic!("half-open failure must re-degrade"));
            last = next - now;
        }
        assert_eq!(last, Duration::from_secs(60));
    }

    #[test]
    fn a_successful_trial_brings_the_link_back_up() {
        let now = Instant::now();
        let mut health = LinkHealth::new();
        for _ in 0..3 {
            health.record_failure(now, &config());
        }
        health.record_success();
        assert_eq!(health.state(), LinkState::Up);
        assert_eq!(health.slot_cap(8, now), 8);
        // …and the next degrade starts from the short backoff again.
        for _ in 0..3 {
            health.record_failure(now, &config());
        }
        assert_eq!(
            health.state(),
            LinkState::Degraded {
                until: now + Duration::from_secs(5)
            }
        );
    }

    #[test]
    fn a_stale_failure_during_the_backoff_changes_nothing() {
        let now = Instant::now();
        let mut health = LinkHealth::new();
        for _ in 0..3 {
            health.record_failure(now, &config());
        }
        let before = health;
        assert!(
            health
                .record_failure(now + Duration::from_secs(1), &config())
                .is_none()
        );
        assert_eq!(health.state(), before.state());
    }
}
