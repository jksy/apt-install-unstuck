//! Counters written to `--stats-file` on shutdown, and the per-mirror health
//! that decides which mirror is tried first.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use serde::Serialize;

#[derive(Default)]
pub struct Stats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub stale: AtomicU64,
    pub passthrough: AtomicU64,
    pub errors: AtomicU64,
    pub failovers: AtomicU64,
    pub bytes_from_cache: AtomicU64,
    pub bytes_from_upstream: AtomicU64,
    pub cache_writes: AtomicU64,
    pub cache_write_errors: AtomicU64,
    mirrors: Mutex<BTreeMap<String, MirrorStats>>,
}

/// Transfers smaller than this say more about latency than about throughput.
const MIN_RATE_SAMPLE: u64 = 256 * 1024;

/// A mirror whose observed rate is below this multiple of `--min-rate` is
/// tried after mirrors that have not been seen to be slow.
const SLOW_FACTOR: f64 = 2.0;

#[derive(Default, Clone, Serialize)]
pub struct MirrorStats {
    pub success: u64,
    pub failure: u64,
    /// Stalls, timeouts and errors that made the proxy give up on this mirror.
    /// Mirrors with fewer are tried first for the rest of the process.
    pub penalty: u64,
    /// Moving average of the transfer rate in bytes/s, from larger downloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<u64>,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub hits: u64,
    pub misses: u64,
    pub stale: u64,
    pub passthrough: u64,
    pub errors: u64,
    pub failovers: u64,
    pub bytes_from_cache: u64,
    pub bytes_from_upstream: u64,
    pub cache_writes: u64,
    pub cache_write_errors: u64,
    pub mirrors: BTreeMap<String, MirrorStats>,
    /// Downloads and cache writes still running (only reported by `/_stats`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<usize>,
}

impl Stats {
    pub fn mirror(&self, base: &str, ok: bool) {
        let mut m = self.mirrors.lock().unwrap();
        let e = m.entry(base.to_string()).or_default();
        if ok {
            e.success += 1;
        } else {
            e.failure += 1;
        }
    }

    /// Records that an attempt on `base` was abandoned for a reason that says
    /// something about the mirror (not a 404).
    pub fn penalize(&self, base: &str) {
        let mut m = self.mirrors.lock().unwrap();
        m.entry(base.to_string()).or_default().penalty += 1;
    }

    /// Records the rate of a completed transfer of `bytes` over `elapsed`.
    pub fn observe_rate(&self, base: &str, bytes: u64, elapsed: Duration) {
        if bytes < MIN_RATE_SAMPLE || elapsed.is_zero() {
            return;
        }
        let sample = bytes as f64 / elapsed.as_secs_f64();
        let mut m = self.mirrors.lock().unwrap();
        let e = m.entry(base.to_string()).or_default();
        e.rate = Some(match e.rate {
            Some(old) => (old as f64 + sample) / 2.0,
            None => sample,
        } as u64);
    }

    /// Orders `upstreams` for a new request: fewest penalties first, then
    /// mirrors not known to be slow, then the faster of the slow ones. Ties
    /// keep the configured order.
    pub fn rank(&self, mut upstreams: Vec<String>, min_rate: u64) -> Vec<String> {
        let m = self.mirrors.lock().unwrap();
        let slow_below = min_rate as f64 * SLOW_FACTOR;
        upstreams.sort_by_cached_key(|base| {
            let e = m.get(base);
            let penalty = e.map_or(0, |e| e.penalty);
            let slow_rate = e.and_then(|e| e.rate).filter(|&r| (r as f64) < slow_below);
            // `None` (not slow) sorts before any slow rate; faster slow mirrors first.
            (penalty, slow_rate.map(std::cmp::Reverse))
        });
        upstreams
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            hits: self.hits.load(Relaxed),
            misses: self.misses.load(Relaxed),
            stale: self.stale.load(Relaxed),
            passthrough: self.passthrough.load(Relaxed),
            errors: self.errors.load(Relaxed),
            failovers: self.failovers.load(Relaxed),
            bytes_from_cache: self.bytes_from_cache.load(Relaxed),
            bytes_from_upstream: self.bytes_from_upstream.load(Relaxed),
            cache_writes: self.cache_writes.load(Relaxed),
            cache_write_errors: self.cache_write_errors.load(Relaxed),
            mirrors: self.mirrors.lock().unwrap().clone(),
            in_flight: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bases(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn penalized_mirrors_go_last() {
        let s = Stats::default();
        assert_eq!(
            s.rank(bases(&["a", "b", "c"]), 1000),
            bases(&["a", "b", "c"])
        );
        s.penalize("a");
        assert_eq!(
            s.rank(bases(&["a", "b", "c"]), 1000),
            bases(&["b", "c", "a"])
        );
        s.penalize("b");
        s.penalize("b");
        assert_eq!(
            s.rank(bases(&["a", "b", "c"]), 1000),
            bases(&["c", "a", "b"])
        );
    }

    #[test]
    fn slow_mirrors_go_after_unknown_ones() {
        let s = Stats::default();
        let secs = Duration::from_secs(1);
        s.observe_rate("a", 1500 * 1024, secs);
        s.observe_rate("b", 1200 * 1024, secs);
        s.observe_rate("c", 5000 * 1024, secs);
        // Small transfers are ignored.
        s.observe_rate("d", 1024, secs);
        let min = 1000 * 1024;
        assert_eq!(
            s.rank(bases(&["a", "b", "c", "d"]), min),
            bases(&["c", "d", "a", "b"])
        );
        // A penalty still weighs more than speed.
        s.penalize("c");
        assert_eq!(
            s.rank(bases(&["a", "b", "c", "d"]), min),
            bases(&["d", "a", "b", "c"])
        );
    }

    #[test]
    fn rate_is_a_moving_average() {
        let s = Stats::default();
        s.observe_rate("a", 1000 * 1024, Duration::from_secs(1));
        s.observe_rate("a", 3000 * 1024, Duration::from_secs(1));
        assert_eq!(s.snapshot().mirrors["a"].rate, Some(2000 * 1024));
    }
}
