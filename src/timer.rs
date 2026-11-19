//! The vigil: a focus timer owned by the core. Pure state over `Instant`s so
//! it can be tested without waiting; the core drives it with a 1 s tick.

use std::time::{Duration, Instant};

pub const DEFAULT_VIGIL: Duration = Duration::from_secs(25 * 60);

#[derive(Clone, Debug)]
pub struct Vigil {
    total: Duration,
    /// Time accumulated in earlier running stretches (before the last pause).
    banked: Duration,
    /// Set while running.
    since: Option<Instant>,
}

impl Vigil {
    pub fn new(total: Duration) -> Self {
        Self { total, banked: Duration::ZERO, since: None }
    }

    pub fn total(&self) -> Duration {
        self.total
    }

    pub fn running(&self) -> bool {
        self.since.is_some()
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        let live = self.since.map(|s| now.saturating_duration_since(s)).unwrap_or_default();
        (self.banked + live).min(self.total)
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.total - self.elapsed(now)
    }

    /// Start or resume. Starting a finished vigil begins a fresh one.
    pub fn start(&mut self, now: Instant) {
        if self.remaining(now).is_zero() {
            self.banked = Duration::ZERO;
        }
        if self.since.is_none() {
            self.since = Some(now);
        }
    }

    pub fn pause(&mut self, now: Instant) {
        if self.since.is_some() {
            self.banked = self.elapsed(now);
            self.since = None;
        }
    }

    pub fn reset(&mut self) {
        self.banked = Duration::ZERO;
        self.since = None;
    }

    /// Call on every tick. Returns true exactly once, when a running vigil
    /// reaches zero; the vigil then stops.
    pub fn poll_finished(&mut self, now: Instant) -> bool {
        if self.running() && self.remaining(now).is_zero() {
            self.banked = self.total;
            self.since = None;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Duration = Duration::from_secs(60);

    #[test]
    fn runs_pauses_resumes() {
        let t0 = Instant::now();
        let mut v = Vigil::new(25 * M);
        assert_eq!(v.remaining(t0), 25 * M);
        v.start(t0);
        assert_eq!(v.remaining(t0 + 10 * M), 15 * M);
        v.pause(t0 + 10 * M);
        // paused: time passing does not count
        assert_eq!(v.remaining(t0 + 20 * M), 15 * M);
        v.start(t0 + 20 * M);
        assert_eq!(v.remaining(t0 + 25 * M), 10 * M);
    }

    #[test]
    fn finishes_once_and_stops() {
        let t0 = Instant::now();
        let mut v = Vigil::new(M);
        v.start(t0);
        assert!(!v.poll_finished(t0 + M / 2));
        assert!(v.poll_finished(t0 + M + Duration::from_secs(3)));
        assert!(!v.running());
        assert!(!v.poll_finished(t0 + 2 * M));
        assert_eq!(v.remaining(t0 + 5 * M), Duration::ZERO);
        // starting again begins a fresh vigil
        v.start(t0 + 5 * M);
        assert_eq!(v.remaining(t0 + 5 * M), M);
    }

    #[test]
    fn reset_clears() {
        let t0 = Instant::now();
        let mut v = Vigil::new(M);
        v.start(t0);
        v.reset();
        assert!(!v.running());
        assert_eq!(v.remaining(t0 + M), M);
    }

    #[test]
    fn double_start_does_not_restart() {
        let t0 = Instant::now();
        let mut v = Vigil::new(10 * M);
        v.start(t0);
        v.start(t0 + 4 * M);
        assert_eq!(v.remaining(t0 + 4 * M), 6 * M);
    }
}
