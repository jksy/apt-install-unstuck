//! HTTP request handling.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use tokio::sync::mpsc;
use tokio_util::task::TaskTracker;

use crate::archive::{self, ArchiveRequest, FileClass, MirrorConfig};
use crate::cache::Cache;
use crate::fetch::{self, FetchConfig, FetchJob, Inflight, Outcome};
use crate::rate::{CHECK_INTERVAL, RateMonitor};
use crate::stats::Stats;

pub type Body = BoxBody<Bytes, std::io::Error>;

const STALE_WARNING: &str = "110 unstuck-proxy \"Response is Stale\"";
const CACHE_HEADER: &str = "x-unstuck-cache";

pub struct App {
    pub fetch: FetchConfig,
    /// Follows redirects; used for archive mirrors.
    pub mirror_client: reqwest::Client,
    /// Passes redirects through to the client.
    pub passthrough_client: reqwest::Client,
    pub mirrors: MirrorConfig,
    pub cache: Option<Cache>,
    pub stats: Stats,
    pub inflight: Mutex<HashMap<String, Arc<Inflight>>>,
    pub tracker: TaskTracker,
    pub temp_dir: PathBuf,
}

/// Fields for the one log line written per request.
struct LogLine {
    start: Instant,
    method: Method,
    key: String,
    result: &'static str,
    mirror: String,
    failovers: u32,
}

impl LogLine {
    fn new(method: &Method, key: String) -> Self {
        Self {
            start: Instant::now(),
            method: method.clone(),
            key,
            result: "error",
            mirror: "-".into(),
            failovers: 0,
        }
    }

    fn emit(&self, status: u16, bytes: u64, error: Option<&str>) {
        eprintln!(
            "unstuck-proxy: {} {} {} status={status} mirror={} failovers={} bytes={bytes} ms={}{}",
            self.result,
            self.method,
            self.key,
            self.mirror,
            self.failovers,
            self.start.elapsed().as_millis(),
            error.map(|e| format!(" error=\"{e}\"")).unwrap_or_default(),
        );
    }
}

fn full(status: StatusCode, msg: &'static str) -> Response<Body> {
    let mut r = Response::new(
        Full::new(Bytes::from_static(msg.as_bytes()))
            .map_err(|e| match e {})
            .boxed(),
    );
    *r.status_mut() = status;
    r
}

fn empty() -> Body {
    Empty::new().map_err(|e| match e {}).boxed()
}

/// Runs `produce` in its own task and exposes what it sends as a response body.
/// The callback receives the byte count and the result once the body is done.
fn channel_body<F, Fut>(produce: F) -> Body
where
    F: FnOnce(mpsc::Sender<std::io::Result<Bytes>>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(produce(tx));
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    BodyExt::boxed(StreamBody::new(stream.map_ok(Frame::data)))
}

/// Forwards a fallible byte stream, counting bytes.
async fn pump<S, E>(
    mut stream: S,
    tx: &mpsc::Sender<std::io::Result<Bytes>>,
) -> (u64, Result<(), String>)
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut n = 0u64;
    while let Some(item) = stream.next().await {
        match item {
            Ok(b) => {
                n += b.len() as u64;
                if tx.send(Ok(b)).await.is_err() {
                    return (n, Err("client went away".into()));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = tx.send(Err(std::io::Error::other(msg.clone()))).await;
                return (n, Err(msg));
            }
        }
    }
    (n, Ok(()))
}

pub async fn handle(app: Arc<App>, req: Request<Incoming>) -> Response<Body> {
    let method = req.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return full(
            StatusCode::METHOD_NOT_ALLOWED,
            "only GET and HEAD are supported\n",
        );
    }
    if req.uri().scheme().is_none() {
        return match req.uri().path() {
            "/_health" => full(StatusCode::OK, "ok\n"),
            // Current counters, for callers that keep the proxy running.
            "/_stats" => {
                let mut snapshot = app.stats.snapshot();
                snapshot.in_flight = Some(app.tracker.len());
                let json = serde_json::to_vec_pretty(&snapshot).unwrap_or_default();
                let mut r =
                    Response::new(Full::new(Bytes::from(json)).map_err(|e| match e {}).boxed());
                r.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                r
            }
            _ => full(
                StatusCode::BAD_REQUEST,
                "expected an absolute-form proxy request\n",
            ),
        };
    }
    match archive::match_request(req.uri()) {
        Some(ar) => handle_archive(app, req, ar).await,
        None => passthrough(app, req).await,
    }
}

async fn handle_archive(
    app: Arc<App>,
    req: Request<Incoming>,
    ar: ArchiveRequest,
) -> Response<Body> {
    let method = req.method().clone();
    let store_key = app
        .cache
        .as_ref()
        .map(|c| c.archive_key(ar.def.name, &ar.rel));
    let key = format!("{}/{}", ar.def.name, ar.rel);
    let mut log = LogLine::new(&method, key.clone());
    let upstreams = app.mirrors.resolve(&ar);

    if ar.class == FileClass::Immutable
        && let (Some(cache), Some(sk)) = (&app.cache, &store_key)
    {
        log.result = "hit";
        if method == Method::HEAD {
            match cache.head(sk).await {
                Ok(Some(meta)) => {
                    app.stats.hits.fetch_add(1, Relaxed);
                    log.emit(200, 0, None);
                    let mut r = Response::new(empty());
                    r.headers_mut()
                        .insert(header::CONTENT_LENGTH, meta.size.into());
                    r.headers_mut()
                        .insert(CACHE_HEADER, HeaderValue::from_static("hit"));
                    return r;
                }
                Ok(None) => {}
                Err(e) => eprintln!("unstuck-proxy: cache lookup failed for {key}: {e}"),
            }
        } else {
            match cache.get(sk).await {
                Ok(Some(obj)) => {
                    app.stats.hits.fetch_add(1, Relaxed);
                    return serve_cached(app, obj, log, None);
                }
                Ok(None) => {}
                Err(e) => eprintln!("unstuck-proxy: cache lookup failed for {key}: {e}"),
            }
        }
    }

    log.result = "miss";
    if method == Method::HEAD {
        return head_archive(app, &ar, upstreams, store_key, log).await;
    }

    // Upstream is always asked for the whole file so the cache stays fresh;
    // the client's condition is evaluated here instead.
    let if_modified_since = match ar.class {
        FileClass::Immutable => None,
        _ => req.headers().get(header::IF_MODIFIED_SINCE).cloned(),
    };
    let job = FetchJob {
        key: key.clone(),
        upstreams,
        rel_encoded: ar.rel_encoded.clone(),
        class: ar.class,
        by_hash: archive::by_hash(&ar.rel),
        user_agent: req.headers().get(header::USER_AGENT).cloned(),
        store_key: if ar.class == FileClass::Uncached {
            None
        } else {
            store_key.clone()
        },
    };
    let inflight = match fetch::join_or_start(&app, job) {
        Ok(f) => f,
        Err(e) => {
            app.stats.errors.fetch_add(1, Relaxed);
            log.emit(500, 0, Some(&e.to_string()));
            return full(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot create temporary file\n",
            );
        }
    };
    match fetch::wait_head(&inflight).await {
        Ok(head) => {
            app.stats.misses.fetch_add(1, Relaxed);
            let mut r = Response::new(empty());
            let h = r.headers_mut();
            if let Some(l) = head.content_length {
                h.insert(header::CONTENT_LENGTH, l.into());
            }
            if let Some(v) = &head.content_type {
                h.insert(header::CONTENT_TYPE, v.clone());
            }
            if let Some(v) = &head.last_modified {
                h.insert(header::LAST_MODIFIED, v.clone());
            }
            h.insert(CACHE_HEADER, HeaderValue::from_static("miss"));
            if not_modified(if_modified_since.as_ref(), head.last_modified.as_ref()) {
                // The download keeps running in the background to refresh the cache.
                *r.status_mut() = StatusCode::NOT_MODIFIED;
                r.headers_mut().remove(header::CONTENT_LENGTH);
                let s = inflight.state.borrow();
                log.mirror = s.mirror.clone().unwrap_or_else(|| "-".into());
                log.failovers = s.failovers;
                log.emit(304, 0, None);
                return r;
            }
            *r.body_mut() = channel_body(move |tx| async move {
                let (n, res) = fetch::follow(inflight.clone(), tx).await;
                {
                    let s = inflight.state.borrow();
                    log.mirror = s.mirror.clone().unwrap_or_else(|| "-".into());
                    log.failovers = s.failovers;
                }
                log.emit(200, n, res.err().as_deref());
            });
            r
        }
        Err(outcome) => {
            {
                let s = inflight.state.borrow();
                log.mirror = s.mirror.clone().unwrap_or_else(|| "-".into());
                log.failovers = s.failovers;
            }
            match outcome {
                Outcome::NotFound => {
                    app.stats.misses.fetch_add(1, Relaxed);
                    log.emit(404, 0, None);
                    full(StatusCode::NOT_FOUND, "not found on any mirror\n")
                }
                Outcome::Failed(msg) => stale_or_502(app, &ar, store_key, log, &msg).await,
                Outcome::Complete => unreachable!("a completed download always has a head"),
            }
        }
    }
}

/// Serves an older cached copy of a mutable file when every upstream failed.
async fn stale_or_502(
    app: Arc<App>,
    ar: &ArchiveRequest,
    store_key: Option<object_store::path::Path>,
    mut log: LogLine,
    msg: &str,
) -> Response<Body> {
    if ar.class == FileClass::Mutable
        && let (Some(cache), Some(sk)) = (&app.cache, &store_key)
    {
        match cache.get(sk).await {
            Ok(Some(obj)) => {
                app.stats.stale.fetch_add(1, Relaxed);
                log.result = "stale";
                eprintln!(
                    "unstuck-proxy: serving stale {} because all upstreams failed: {msg}",
                    log.key
                );
                return serve_cached(app, obj, log, Some(STALE_WARNING));
            }
            Ok(None) => {}
            Err(e) => eprintln!("unstuck-proxy: cache lookup failed for {}: {e}", log.key),
        }
    }
    app.stats.errors.fetch_add(1, Relaxed);
    log.result = "error";
    log.emit(502, 0, Some(msg));
    full(StatusCode::BAD_GATEWAY, "all upstreams failed\n")
}

fn serve_cached(
    app: Arc<App>,
    obj: object_store::GetResult,
    log: LogLine,
    warning: Option<&'static str>,
) -> Response<Body> {
    let mut r = Response::new(empty());
    let h = r.headers_mut();
    h.insert(header::CONTENT_LENGTH, obj.meta.size.into());
    if let Ok(v) = HeaderValue::from_str(&http_date(&obj.meta)) {
        h.insert(header::LAST_MODIFIED, v);
    }
    let label = if warning.is_some() { "stale" } else { "hit" };
    h.insert(CACHE_HEADER, HeaderValue::from_static(label));
    if let Some(w) = warning {
        h.insert(header::WARNING, HeaderValue::from_static(w));
    }
    *r.body_mut() = channel_body(move |tx| async move {
        let (n, res) = pump(obj.into_stream(), &tx).await;
        app.stats.bytes_from_cache.fetch_add(n, Relaxed);
        log.emit(200, n, res.err().as_deref());
    });
    r
}

/// Formats an object_store timestamp as an HTTP date.
fn http_date(meta: &object_store::ObjectMeta) -> String {
    let secs = u64::try_from(meta.last_modified.timestamp()).unwrap_or(0);
    httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

async fn head_archive(
    app: Arc<App>,
    ar: &ArchiveRequest,
    upstreams: Vec<String>,
    store_key: Option<object_store::path::Path>,
    mut log: LogLine,
) -> Response<Body> {
    let mut not_found = 0;
    let mut last_error = String::new();
    for round in 0..app.fetch.rounds.max(1) {
        not_found = 0;
        for base in &upstreams {
            let url = format!("{base}{}", ar.rel_encoded);
            log.mirror = base.clone();
            let res = tokio::time::timeout(
                app.fetch.first_byte_timeout,
                app.mirror_client.head(&url).send(),
            )
            .await;
            match res {
                Ok(Ok(resp)) if resp.status() == StatusCode::OK => {
                    app.stats.mirror(base, true);
                    app.stats.misses.fetch_add(1, Relaxed);
                    log.emit(200, 0, None);
                    let mut r = Response::new(empty());
                    for name in [
                        header::CONTENT_LENGTH,
                        header::CONTENT_TYPE,
                        header::LAST_MODIFIED,
                    ] {
                        if let Some(v) = resp.headers().get(&name) {
                            r.headers_mut().insert(name, v.clone());
                        }
                    }
                    r.headers_mut()
                        .insert(CACHE_HEADER, HeaderValue::from_static("miss"));
                    return r;
                }
                Ok(Ok(resp)) if resp.status() == StatusCode::NOT_FOUND => not_found += 1,
                Ok(Ok(resp)) => last_error = format!("status {}", resp.status()),
                Ok(Err(e)) => last_error = e.to_string(),
                Err(_) => last_error = "first-byte-timeout".into(),
            }
            app.stats.mirror(base, false);
            log.failovers += 1;
        }
        if not_found == upstreams.len() {
            break;
        }
        if round + 1 < app.fetch.rounds {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
    log.failovers = log.failovers.saturating_sub(1);
    if not_found == upstreams.len() {
        app.stats.misses.fetch_add(1, Relaxed);
        log.emit(404, 0, None);
        return full(StatusCode::NOT_FOUND, "");
    }
    if ar.class == FileClass::Mutable
        && let (Some(cache), Some(sk)) = (&app.cache, &store_key)
        && let Ok(Some(meta)) = cache.head(sk).await
    {
        app.stats.stale.fetch_add(1, Relaxed);
        log.result = "stale";
        log.emit(200, 0, None);
        let mut r = Response::new(empty());
        r.headers_mut()
            .insert(header::CONTENT_LENGTH, meta.size.into());
        r.headers_mut()
            .insert(header::WARNING, HeaderValue::from_static(STALE_WARNING));
        r.headers_mut()
            .insert(CACHE_HEADER, HeaderValue::from_static("stale"));
        return r;
    }
    app.stats.errors.fetch_add(1, Relaxed);
    log.result = "error";
    log.emit(502, 0, Some(&last_error));
    full(StatusCode::BAD_GATEWAY, "")
}

/// True when the client's copy (If-Modified-Since) is at least as new as upstream's.
fn not_modified(
    if_modified_since: Option<&HeaderValue>,
    last_modified: Option<&HeaderValue>,
) -> bool {
    let parse =
        |v: Option<&HeaderValue>| v.and_then(|v| httpdate::parse_http_date(v.to_str().ok()?).ok());
    match (parse(if_modified_since), parse(last_modified)) {
        (Some(since), Some(modified)) => modified <= since,
        _ => false,
    }
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

fn copy_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from {
        if !is_hop_by_hop(name) {
            to.append(name.clone(), value.clone());
        }
    }
}

/// Relays a request for a host outside the archive table. No caching or
/// failover; a stalled transfer is cut so the client can retry.
async fn passthrough(app: Arc<App>, req: Request<Incoming>) -> Response<Body> {
    let method = req.method().clone();
    let url = req.uri().to_string();
    let mut log = LogLine::new(&method, url.clone());
    log.result = "passthrough";
    if let Some(a) = req.uri().authority() {
        log.mirror = a.to_string();
    }
    app.stats.passthrough.fetch_add(1, Relaxed);
    let mut headers = HeaderMap::new();
    copy_headers(req.headers(), &mut headers);
    let send = app
        .passthrough_client
        .request(method.clone(), &url)
        .headers(headers)
        .send();
    let resp = match tokio::time::timeout(app.fetch.first_byte_timeout, send).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            log.emit(502, 0, Some(&e.to_string()));
            return full(StatusCode::BAD_GATEWAY, "upstream request failed\n");
        }
        Err(_) => {
            log.emit(504, 0, Some("first-byte-timeout"));
            return full(StatusCode::GATEWAY_TIMEOUT, "upstream did not respond\n");
        }
    };
    let status = resp.status();
    let mut r = Response::new(empty());
    *r.status_mut() = status;
    copy_headers(resp.headers(), r.headers_mut());
    if method == Method::HEAD {
        log.emit(status.as_u16(), 0, None);
        return r;
    }
    let (window, min_rate) = (app.fetch.stall_timeout, app.fetch.min_rate);
    *r.body_mut() = channel_body(move |tx| async move {
        let mut body = resp.bytes_stream();
        let mut monitor = RateMonitor::new(window, min_rate);
        let mut ticker = tokio::time::interval(CHECK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut n = 0u64;
        let err = loop {
            tokio::select! {
                c = body.next() => match c {
                    None => break None,
                    Some(Ok(b)) => {
                        monitor.add(b.len());
                        n += b.len() as u64;
                        app.stats.bytes_from_upstream.fetch_add(b.len() as u64, Relaxed);
                        if tx.send(Ok(b)).await.is_err() {
                            break Some("client went away".to_string());
                        }
                    }
                    Some(Err(e)) => break Some(e.to_string()),
                },
                _ = ticker.tick() => if monitor.stalled() {
                    break Some("stalled".to_string());
                },
            }
        };
        if let Some(e) = &err {
            let _ = tx.send(Err(std::io::Error::other(e.clone()))).await;
        }
        log.emit(status.as_u16(), n, err.as_deref());
    });
    r
}
