//! Upstream fetching with mirror failover, shared by concurrent requests.
//!
//! One leader task per key downloads into a temporary file at the upstream's
//! pace; every client of that key (the first one included) streams from the
//! file as it grows. When a transfer stalls, the leader switches to the next
//! mirror, re-downloads from the start and discards the bytes it already has,
//! so clients see one uninterrupted body.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use hyper::StatusCode;
use hyper::header::{self, HeaderValue};
use md5::Md5;
use sha2::{Digest, Sha256, Sha512};
use tempfile::TempPath;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

use crate::archive::{FileClass, HashAlgo};
use crate::proxy::App;
use crate::rate::{CHECK_INTERVAL, RateMonitor};

pub struct FetchConfig {
    pub min_rate: u64,
    pub stall_timeout: Duration,
    pub first_byte_timeout: Duration,
    pub rounds: u32,
}

/// Response metadata taken from the first upstream that answered 200.
#[derive(Debug)]
pub struct Head {
    pub content_length: Option<u64>,
    pub last_modified: Option<HeaderValue>,
    pub content_type: Option<HeaderValue>,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Complete,
    /// Every upstream answered 404.
    NotFound,
    Failed(String),
}

#[derive(Default)]
pub struct FetchState {
    pub head: Option<Arc<Head>>,
    pub written: u64,
    pub done: Option<Outcome>,
    pub mirror: Option<String>,
    pub failovers: u32,
}

pub struct Inflight {
    pub state: watch::Sender<FetchState>,
    pub path: TempPath,
}

pub struct FetchJob {
    pub key: String,
    pub upstreams: Vec<String>,
    pub rel_encoded: String,
    pub class: FileClass,
    pub by_hash: Option<(HashAlgo, String)>,
    pub user_agent: Option<HeaderValue>,
    pub store_key: Option<object_store::path::Path>,
}

enum AttemptError {
    NotFound,
    /// The new upstream serves something different from what was already sent.
    Mismatch(String),
    Retry(String),
}

enum Hasher {
    Sha256(Sha256),
    Sha512(Sha512),
    Md5(Md5),
}

impl Hasher {
    fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Sha256 => Hasher::Sha256(Sha256::new()),
            HashAlgo::Sha512 => Hasher::Sha512(Sha512::new()),
            HashAlgo::Md5 => Hasher::Md5(Md5::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
            Hasher::Md5(h) => h.update(data),
        }
    }

    fn hex(self) -> String {
        match self {
            Hasher::Sha256(h) => hex::encode(h.finalize()),
            Hasher::Sha512(h) => hex::encode(h.finalize()),
            Hasher::Md5(h) => hex::encode(h.finalize()),
        }
    }
}

struct Progress {
    file: tokio::fs::File,
    written: u64,
    head: Option<Arc<Head>>,
    hasher: Option<Hasher>,
}

/// Returns the in-flight download for `job.key`, starting one if needed.
pub fn join_or_start(app: &Arc<App>, job: FetchJob) -> std::io::Result<Arc<Inflight>> {
    let mut map = app.inflight.lock().unwrap();
    if let Some(f) = map.get(&job.key) {
        return Ok(f.clone());
    }
    let (file, path) = tempfile::Builder::new()
        .prefix("unstuck-")
        .tempfile_in(&app.temp_dir)?
        .into_parts();
    let inflight = Arc::new(Inflight {
        state: watch::Sender::new(FetchState::default()),
        path,
    });
    map.insert(job.key.clone(), inflight.clone());
    let progress = Progress {
        file: tokio::fs::File::from_std(file),
        written: 0,
        head: None,
        hasher: job.by_hash.as_ref().map(|(algo, _)| Hasher::new(*algo)),
    };
    app.tracker
        .spawn(lead(app.clone(), inflight.clone(), job, progress));
    Ok(inflight)
}

async fn lead(app: Arc<App>, inflight: Arc<Inflight>, job: FetchJob, mut p: Progress) {
    let outcome = download(&app, &inflight, &job, &mut p).await;
    let mut save = matches!(outcome, Outcome::Complete) && job.store_key.is_some();
    if save {
        let length = p.head.as_ref().and_then(|h| h.content_length);
        if length != Some(p.written) {
            eprintln!(
                "unstuck-proxy: not caching {}: length {length:?} != {} bytes",
                job.key, p.written
            );
            save = false;
        } else if let (Some((_, expected)), Some(hasher)) = (&job.by_hash, p.hasher.take()) {
            let actual = hasher.hex();
            if &actual != expected {
                eprintln!(
                    "unstuck-proxy: not caching {}: hash mismatch (got {actual})",
                    job.key
                );
                save = false;
            }
        }
    }
    inflight.state.send_modify(|s| s.done = Some(outcome));

    if save && let (Some(cache), Some(key)) = (&app.cache, &job.store_key) {
        match cache.put_file(key, &inflight.path).await {
            Ok(()) => {
                app.stats.cache_writes.fetch_add(1, Relaxed);
            }
            Err(e) => {
                app.stats.cache_write_errors.fetch_add(1, Relaxed);
                eprintln!("unstuck-proxy: cache write failed for {}: {e:#}", job.key);
            }
        }
    }
    let mut map = app.inflight.lock().unwrap();
    if map.get(&job.key).is_some_and(|f| Arc::ptr_eq(f, &inflight)) {
        map.remove(&job.key);
    }
}

async fn download(app: &App, inflight: &Inflight, job: &FetchJob, p: &mut Progress) -> Outcome {
    let cfg = &app.fetch;
    let mut tried = 0u32;
    let mut last_error = String::from("no upstreams configured");
    for round in 0..cfg.rounds.max(1) {
        if round > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let mut not_found = 0;
        for base in &job.upstreams {
            if tried > 0 {
                app.stats.failovers.fetch_add(1, Relaxed);
                inflight.state.send_modify(|s| s.failovers += 1);
            }
            tried += 1;
            inflight
                .state
                .send_modify(|s| s.mirror = Some(base.clone()));
            let url = format!("{base}{}", job.rel_encoded);
            match attempt(app, inflight, job, &url, p).await {
                Ok(()) => {
                    app.stats.mirror(base, true);
                    return Outcome::Complete;
                }
                Err(e) => {
                    app.stats.mirror(base, false);
                    match e {
                        AttemptError::NotFound => {
                            not_found += 1;
                            last_error = format!("{url}: 404");
                        }
                        AttemptError::Mismatch(msg) => {
                            eprintln!("unstuck-proxy: {url}: {msg}; aborting {}", job.key);
                            return Outcome::Failed(msg);
                        }
                        AttemptError::Retry(msg) => {
                            eprintln!("unstuck-proxy: {url}: {msg}");
                            last_error = format!("{url}: {msg}");
                        }
                    }
                }
            }
        }
        if not_found == job.upstreams.len() && p.head.is_none() {
            return Outcome::NotFound;
        }
    }
    Outcome::Failed(last_error)
}

async fn attempt(
    app: &App,
    inflight: &Inflight,
    job: &FetchJob,
    url: &str,
    p: &mut Progress,
) -> Result<(), AttemptError> {
    let cfg = &app.fetch;
    let mut req = app.mirror_client.get(url);
    if let Some(ua) = &job.user_agent {
        req = req.header(header::USER_AGENT, ua.clone());
    }
    let resp = match tokio::time::timeout(cfg.first_byte_timeout, req.send()).await {
        Err(_) => {
            return Err(AttemptError::Retry(
                "no response headers before first-byte-timeout".into(),
            ));
        }
        Ok(Err(e)) => return Err(AttemptError::Retry(format!("request failed: {e}"))),
        Ok(Ok(r)) => r,
    };
    match resp.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => return Err(AttemptError::NotFound),
        s => return Err(AttemptError::Retry(format!("status {s}"))),
    }
    let length = resp.content_length();
    let last_modified = resp.headers().get(header::LAST_MODIFIED).cloned();
    match &p.head {
        None => {
            let head = Arc::new(Head {
                content_length: length,
                last_modified,
                content_type: resp.headers().get(header::CONTENT_TYPE).cloned(),
            });
            p.head = Some(head.clone());
            inflight.state.send_modify(|s| s.head = Some(head));
        }
        Some(head) => {
            if head.content_length.is_none() || head.content_length != length {
                return Err(AttemptError::Mismatch(format!(
                    "length {length:?} differs from {:?}",
                    head.content_length
                )));
            }
            if job.class == FileClass::Mutable
                && let (Some(a), Some(b)) = (&head.last_modified, &last_modified)
                && a != b
            {
                return Err(AttemptError::Mismatch(format!(
                    "Last-Modified {b:?} differs from {a:?}"
                )));
            }
        }
    }

    // Bytes of this response that were already delivered by a previous upstream.
    let mut skip = p.written;
    let mut received = 0u64;
    let mut monitor = RateMonitor::new(cfg.stall_timeout, cfg.min_rate);
    let mut ticker = tokio::time::interval(CHECK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut body = resp.bytes_stream();
    loop {
        let chunk = tokio::select! {
            c = body.next() => c,
            _ = ticker.tick() => {
                if monitor.stalled() {
                    return Err(AttemptError::Retry(format!(
                        "stalled: below {} B/s for {:?} after {received} bytes",
                        cfg.min_rate, cfg.stall_timeout
                    )));
                }
                continue;
            }
        };
        let mut chunk: Bytes = match chunk {
            None => break,
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                return Err(AttemptError::Retry(format!(
                    "body error after {received} bytes: {e}"
                )));
            }
        };
        monitor.add(chunk.len());
        received += chunk.len() as u64;
        app.stats
            .bytes_from_upstream
            .fetch_add(chunk.len() as u64, Relaxed);
        if skip > 0 {
            let n = skip.min(chunk.len() as u64) as usize;
            skip -= n as u64;
            chunk = chunk.slice(n..);
            if chunk.is_empty() {
                continue;
            }
        }
        if length.is_some_and(|l| p.written + chunk.len() as u64 > l) {
            return Err(AttemptError::Mismatch(
                "upstream sent more than Content-Length".into(),
            ));
        }
        if let Some(h) = &mut p.hasher {
            h.update(&chunk);
        }
        let write = async {
            p.file.write_all(&chunk).await?;
            p.file.flush().await
        };
        if let Err(e) = write.await {
            return Err(AttemptError::Mismatch(format!(
                "writing temporary file: {e}"
            )));
        }
        p.written += chunk.len() as u64;
        let written = p.written;
        inflight.state.send_modify(|s| s.written = written);
    }
    match length {
        Some(l) if p.written < l => Err(AttemptError::Retry(format!(
            "connection closed at {}/{l} bytes",
            p.written
        ))),
        None if skip > 0 => Err(AttemptError::Mismatch(
            "shorter than the data already sent".into(),
        )),
        _ => Ok(()),
    }
}

/// Waits until the download has response headers (or has finished without any).
pub async fn wait_head(inflight: &Inflight) -> Result<Arc<Head>, Outcome> {
    let mut rx = inflight.state.subscribe();
    loop {
        {
            let s = rx.borrow_and_update();
            if let Some(h) = &s.head {
                return Ok(h.clone());
            }
            if let Some(d) = &s.done {
                return Err(d.clone());
            }
        }
        if rx.changed().await.is_err() {
            return Err(Outcome::Failed("download task ended".into()));
        }
    }
}

/// Streams the temporary file to `tx` as it grows. Returns the number of
/// bytes sent and whether the body was delivered completely.
pub async fn follow(
    inflight: Arc<Inflight>,
    tx: mpsc::Sender<std::io::Result<Bytes>>,
) -> (u64, Result<(), String>) {
    let mut rx = inflight.state.subscribe();
    let mut file = match tokio::fs::File::open(&*inflight.path).await {
        Ok(f) => f,
        Err(e) => return (0, Err(format!("opening temporary file: {e}"))),
    };
    let mut pos = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let (written, done) = {
            let s = rx.borrow_and_update();
            (s.written, s.done.clone())
        };
        if pos < written {
            let want = (written - pos).min(buf.len() as u64) as usize;
            let n = match file.read(&mut buf[..want]).await {
                Ok(0) => return (pos, Err("temporary file ended early".into())),
                Ok(n) => n,
                Err(e) => return (pos, Err(format!("reading temporary file: {e}"))),
            };
            pos += n as u64;
            if tx
                .send(Ok(Bytes::copy_from_slice(&buf[..n])))
                .await
                .is_err()
            {
                return (pos, Err("client went away".into()));
            }
            continue;
        }
        match done {
            Some(Outcome::Complete) => return (pos, Ok(())),
            Some(Outcome::Failed(msg)) => {
                let _ = tx.send(Err(std::io::Error::other(msg.clone()))).await;
                return (pos, Err(msg));
            }
            Some(Outcome::NotFound) => {
                let _ = tx.send(Err(std::io::Error::other("not found"))).await;
                return (pos, Err("not found".into()));
            }
            None => {
                if rx.changed().await.is_err() {
                    return (pos, Err("download task ended".into()));
                }
            }
        }
    }
}
