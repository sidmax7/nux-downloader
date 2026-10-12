//! Rate limiting (`ARCHITECTURE.md` §11). One [`Limiter`] per scope —
//! global, per package, per link — and a transfer waits on each
//! applicable one, in turn, before it writes a buffer.
//!
//! A limiter is a small GCRA scheduler on tokio's clock: each request for
//! `n` bytes is given the earliest instant it may proceed, and the limiter
//! remembers when the bytes already granted will have "drained". That is
//! exact for any request size (no cell rounding, no rejected oversize
//! buffers) and, because it uses tokio time, testable with paused time.
//! A limiter allows a burst of 100 ms of its rate. Changing a limit
//! applies to the next request; sleepers already scheduled finish their
//! (short) wait.

use crate::model::{ByteRate, LinkId, PackageId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

const BURST: Duration = Duration::from_millis(100);

struct State {
    rate: Option<ByteRate>,
    /// When everything granted so far has drained at `rate`.
    drained_at: Instant,
}

pub struct Limiter {
    state: Mutex<State>,
}

impl Limiter {
    #[must_use]
    pub fn new(rate: Option<ByteRate>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                rate,
                drained_at: Instant::now(),
            }),
        })
    }

    /// Replaces the limit (`None` = unlimited). Debt owed under the old
    /// rate is forgiven.
    pub fn set_rate(&self, rate: Option<ByteRate>) {
        if let Ok(mut state) = self.state.lock() {
            state.rate = rate;
            state.drained_at = Instant::now();
        }
    }

    #[must_use]
    pub fn rate(&self) -> Option<ByteRate> {
        self.state.lock().ok().and_then(|s| s.rate)
    }

    /// Reserves `bytes` and returns when they may be written, or `None`
    /// to go ahead now.
    fn reserve(&self, bytes: u64, now: Instant) -> Option<Instant> {
        let mut state = self.state.lock().ok()?;
        let rate = state.rate?.bytes_per_sec();
        let nanos = u128::from(bytes) * 1_000_000_000 / u128::from(rate);
        let cost = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        let start = state.drained_at.max(now);
        state.drained_at = start.checked_add(cost).unwrap_or(start);
        let allowed = state.drained_at.checked_sub(BURST).unwrap_or(now);
        (allowed > now).then_some(allowed)
    }

    /// Waits until `bytes` may be written.
    pub async fn acquire(&self, bytes: usize) {
        if let Some(until) = self.reserve(bytes as u64, Instant::now()) {
            tokio::time::sleep_until(until).await;
        }
    }
}

/// The limiters one transfer attempt must pass, in order.
#[derive(Clone, Default)]
pub struct Throttle(Vec<Arc<Limiter>>);

impl Throttle {
    #[must_use]
    pub fn unlimited() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn new(limiters: Vec<Arc<Limiter>>) -> Self {
        Self(limiters)
    }

    pub async fn acquire(&self, bytes: usize) {
        for limiter in &self.0 {
            limiter.acquire(bytes).await;
        }
    }
}

/// Every limiter the engine runs, by scope. Shared between the driver
/// (which builds a [`Throttle`] for each attempt) and the `Engine` API
/// (which changes limits live).
pub struct Limits {
    global: Arc<Limiter>,
    packages: Mutex<HashMap<PackageId, Arc<Limiter>>>,
    links: Mutex<HashMap<LinkId, Arc<Limiter>>>,
}

impl Limits {
    #[must_use]
    pub fn new(global: Option<ByteRate>) -> Arc<Self> {
        Arc::new(Self {
            global: Limiter::new(global),
            packages: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_global(&self, rate: Option<ByteRate>) {
        self.global.set_rate(rate);
    }

    #[must_use]
    pub fn global(&self) -> Option<ByteRate> {
        self.global.rate()
    }

    pub fn set_package(&self, package: PackageId, rate: Option<ByteRate>) {
        if let Ok(mut map) = self.packages.lock() {
            match map.get(&package) {
                Some(limiter) => limiter.set_rate(rate),
                None => {
                    map.insert(package, Limiter::new(rate));
                }
            }
        }
    }

    pub fn forget_package(&self, package: PackageId) {
        if let Ok(mut map) = self.packages.lock() {
            map.remove(&package);
        }
    }

    pub fn set_link(&self, link: LinkId, rate: Option<ByteRate>) {
        if let Ok(mut map) = self.links.lock() {
            match map.get(&link) {
                Some(limiter) => limiter.set_rate(rate),
                None => {
                    map.insert(link, Limiter::new(rate));
                }
            }
        }
    }

    /// Registers a link seen by a scan with the limit it was discovered
    /// with, unless it already has one (a user's choice survives rescans).
    pub fn ensure_link(&self, link: LinkId, discovered: Option<ByteRate>) {
        if let Ok(mut map) = self.links.lock() {
            map.entry(link).or_insert_with(|| Limiter::new(discovered));
        }
    }

    #[must_use]
    pub fn link(&self, link: LinkId) -> Option<ByteRate> {
        self.links.lock().ok()?.get(&link)?.rate()
    }

    /// What an attempt for `package` on `link` must pass.
    #[must_use]
    pub fn throttle_for(&self, package: PackageId, link: LinkId) -> Throttle {
        let mut limiters = vec![self.global.clone()];
        if let Ok(map) = self.packages.lock()
            && let Some(limiter) = map.get(&package)
        {
            limiters.push(limiter.clone());
        }
        if let Ok(map) = self.links.lock()
            && let Some(limiter) = map.get(&link)
        {
            limiters.push(limiter.clone());
        }
        Throttle(limiters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(n: u64) -> Option<ByteRate> {
        ByteRate::new(n)
    }

    /// Pushes `total` bytes through `throttle` in `chunk`-sized buffers
    /// and returns the elapsed (paused) time.
    async fn drain(throttle: &Throttle, total: usize, chunk: usize) -> Duration {
        let start = Instant::now();
        let mut left = total;
        while left > 0 {
            let n = left.min(chunk);
            throttle.acquire(n).await;
            left -= n;
        }
        start.elapsed()
    }

    fn within(actual: Duration, expected: Duration, percent: f64) {
        let a = actual.as_secs_f64();
        let e = expected.as_secs_f64();
        assert!(
            (a - e).abs() <= e * percent / 100.0,
            "took {a:.3}s, expected {e:.3}s ±{percent}%"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn throughput_is_within_five_percent_of_the_limit() {
        for (limit, chunk) in [
            (1 << 20, 16 * 1024),
            (100 * 1024, 64 * 1024),
            (50 << 20, 256 * 1024),
        ] {
            let limits = Limits::new(rate(limit));
            let throttle = limits.throttle_for(PackageId::new(1), LinkId::new(1));
            let total = (limit * 10) as usize;
            let took = drain(&throttle, total, chunk).await;
            // The burst allows 100 ms of data up front.
            within(took, Duration::from_secs(10) - BURST, 5.0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_never_waits() {
        let limits = Limits::new(None);
        let throttle = limits.throttle_for(PackageId::new(1), LinkId::new(1));
        assert_eq!(drain(&throttle, 1 << 30, 1 << 20).await, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_tightest_scope_wins() {
        let limits = Limits::new(rate(10 << 20));
        limits.set_package(PackageId::new(1), rate(1 << 20));
        limits.set_link(LinkId::new(1), rate(4 << 20));
        let throttle = limits.throttle_for(PackageId::new(1), LinkId::new(1));
        let took = drain(&throttle, 5 << 20, 32 * 1024).await;
        within(took, Duration::from_secs(5) - BURST, 5.0);

        // Another package on the same link is held only by link and global.
        let other = limits.throttle_for(PackageId::new(2), LinkId::new(1));
        let took = drain(&other, 8 << 20, 32 * 1024).await;
        within(took, Duration::from_secs(2) - BURST, 5.0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_shared_limit_is_split_between_concurrent_transfers() {
        let limits = Limits::new(rate(2 << 20));
        let a = limits.throttle_for(PackageId::new(1), LinkId::new(1));
        let b = limits.throttle_for(PackageId::new(2), LinkId::new(2));
        let start = Instant::now();
        let (ta, tb) = tokio::join!(drain(&a, 4 << 20, 32 * 1024), drain(&b, 4 << 20, 32 * 1024));
        // 8 MiB through 2 MiB/s: about 4 s for both.
        within(start.elapsed(), Duration::from_secs(4) - BURST, 5.0);
        assert!(ta.as_secs_f64() > 3.0 && tb.as_secs_f64() > 3.0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_limit_can_change_live() {
        let limits = Limits::new(rate(1 << 20));
        let throttle = limits.throttle_for(PackageId::new(1), LinkId::new(1));
        let slow = drain(&throttle, 2 << 20, 32 * 1024).await;
        within(slow, Duration::from_secs(2) - BURST, 5.0);

        limits.set_global(rate(4 << 20));
        let faster = drain(&throttle, 2 << 20, 32 * 1024).await;
        within(faster, Duration::from_millis(500) - BURST, 10.0);

        limits.set_global(None);
        assert_eq!(drain(&throttle, 100 << 20, 1 << 20).await, Duration::ZERO);
        assert_eq!(limits.global(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_larger_than_the_burst_just_waits_for_it() {
        // 10 KiB/s has a burst of ~1 KiB; a 64 KiB buffer is not rejected.
        let limits = Limits::new(rate(10 * 1024));
        let throttle = limits.throttle_for(PackageId::new(1), LinkId::new(1));
        let took = drain(&throttle, 64 * 1024, 64 * 1024).await;
        within(took, Duration::from_millis(6400) - BURST, 5.0);
    }

    #[test]
    fn links_keep_a_user_set_limit_across_rescans() {
        let limits = Limits::new(None);
        let link = LinkId::new(3);
        limits.ensure_link(link, None);
        limits.set_link(link, rate(5000));
        limits.ensure_link(link, rate(99));
        assert_eq!(limits.link(link), rate(5000));
        limits.forget_package(PackageId::new(1));
    }
}
