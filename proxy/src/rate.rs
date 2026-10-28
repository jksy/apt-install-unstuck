//! Sliding-window transfer rate monitor used for stall detection.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How often callers should call [`RateMonitor::stalled`] while waiting for data.
pub const CHECK_INTERVAL: Duration = Duration::from_millis(250);

pub struct RateMonitor {
    window: Duration,
    min_rate: u64,
    start: Instant,
    total: u64,
    samples: VecDeque<(Instant, u64)>,
}

impl RateMonitor {
    pub fn new(window: Duration, min_rate: u64) -> Self {
        let start = Instant::now();
        Self {
            window,
            min_rate,
            start,
            total: 0,
            samples: VecDeque::from([(start, 0)]),
        }
    }

    pub fn add(&mut self, n: usize) {
        self.total += n as u64;
    }

    /// True when the average rate over the last `window` is below `min_rate`.
    /// Never fires before a full window has elapsed.
    pub fn stalled(&mut self) -> bool {
        self.stalled_at(Instant::now())
    }

    fn stalled_at(&mut self, now: Instant) -> bool {
        if self.min_rate == 0 || self.window.is_zero() {
            return false;
        }
        self.samples.push_back((now, self.total));
        if now.duration_since(self.start) < self.window {
            return false;
        }
        let cutoff = now - self.window;
        while self.samples.len() >= 2 && self.samples[1].0 <= cutoff {
            self.samples.pop_front();
        }
        let bytes = self.total - self.samples[0].1;
        (bytes as f64) < self.min_rate as f64 * self.window.as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_stall_only_after_window() {
        let mut m = RateMonitor::new(Duration::from_secs(2), 100);
        let t0 = m.start;
        m.add(50);
        assert!(!m.stalled_at(t0 + Duration::from_secs(1)));
        assert!(m.stalled_at(t0 + Duration::from_secs(2)));
        m.add(1000);
        assert!(!m.stalled_at(t0 + Duration::from_secs(3)));
        // The 1000 bytes leave the window after another two seconds.
        assert!(m.stalled_at(t0 + Duration::from_secs(6)));
    }

    #[test]
    fn disabled_with_zero_rate() {
        let mut m = RateMonitor::new(Duration::from_secs(1), 0);
        assert!(!m.stalled_at(m.start + Duration::from_secs(10)));
    }
}
