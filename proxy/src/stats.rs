//! Counters written to `--stats-file` on shutdown.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

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

#[derive(Default, Clone, Serialize)]
pub struct MirrorStats {
    pub success: u64,
    pub failure: u64,
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
