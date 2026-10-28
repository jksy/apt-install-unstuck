//! Test helpers: a fake mirror server and a handle to the proxy binary.

#![allow(dead_code)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

pub const LAST_MODIFIED: &str = "Tue, 01 Oct 2024 00:00:00 GMT";

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    /// Serves files, honouring `Range: bytes=N-`.
    Normal,
    /// Sends the body at the given rate in bytes per second.
    Slow(u64),
    /// Sends headers, then nothing.
    HangAfterHeaders,
    /// Sends this many bytes, then drops the connection.
    Truncate(usize),
    /// Answers 404 for everything.
    NotFound,
    /// Ignores Range and always answers 200 with the whole file.
    NoRange,
    /// Waits before answering normally.
    Delay(Duration),
}

type Body = UnsyncBoxBody<Bytes, std::io::Error>;

pub struct FakeMirror {
    pub addr: SocketAddr,
    hits: Arc<AtomicUsize>,
}

impl FakeMirror {
    /// `files` maps absolute request paths (e.g. `/ubuntu/pool/x.deb`) to contents.
    pub async fn start(mode: Mode, files: HashMap<String, Bytes>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let files = Arc::new(files);
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let files = files.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let files = files.clone();
                        async move { Ok::<_, Infallible>(respond(mode, &files, req).await) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        Self { addr, hits }
    }

    /// Base URL of an archive on this mirror, e.g. `http://127.0.0.1:1234/ubuntu/`.
    pub fn base(&self, archive_path: &str) -> String {
        format!("http://{}{archive_path}", self.addr)
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn full_body(b: Bytes) -> Body {
    Full::new(b).map_err(|e| match e {}).boxed_unsync()
}

fn stream_body<S>(s: S) -> Body
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    BodyExt::boxed_unsync(StreamBody::new(s.map(|r| r.map(Frame::data))))
}

async fn respond<B>(mode: Mode, files: &HashMap<String, Bytes>, req: Request<B>) -> Response<Body> {
    if let Mode::Delay(d) = mode {
        tokio::time::sleep(d).await;
    }
    let data = match (mode, files.get(req.uri().path())) {
        (Mode::NotFound, _) | (_, None) => {
            let mut r = Response::new(full_body(Bytes::new()));
            *r.status_mut() = StatusCode::NOT_FOUND;
            return r;
        }
        (_, Some(d)) => d.clone(),
    };
    let total = data.len();
    let range_start = req
        .headers()
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.strip_suffix('-'))
        .and_then(|v| v.parse::<usize>().ok());
    let builder = Response::builder()
        .header("last-modified", LAST_MODIFIED)
        .header("content-type", "application/octet-stream");
    let head = req.method() == hyper::Method::HEAD;
    match mode {
        Mode::Normal | Mode::Delay(_) if range_start.is_some_and(|s| s < total) => {
            let s = range_start.unwrap();
            builder
                .status(206)
                .header("content-length", total - s)
                .header("content-range", format!("bytes {s}-{}/{total}", total - 1))
                .body(full_body(if head { Bytes::new() } else { data.slice(s..) }))
                .unwrap()
        }
        _ if head => builder
            .header("content-length", total)
            .body(full_body(Bytes::new()))
            .unwrap(),
        Mode::Slow(rate) => {
            let step = (rate / 10).max(1) as usize;
            let chunks: Vec<Bytes> = (0..total)
                .step_by(step)
                .map(|i| data.slice(i..(i + step).min(total)))
                .collect();
            let s = futures_util::stream::iter(chunks).then(|c| async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(c)
            });
            builder
                .header("content-length", total)
                .body(stream_body(s))
                .unwrap()
        }
        Mode::HangAfterHeaders => builder
            .header("content-length", total)
            .body(stream_body(futures_util::stream::pending()))
            .unwrap(),
        Mode::Truncate(n) => {
            let first = data.slice(..n.min(total));
            let s = futures_util::stream::iter([Some(first), None]).then(|c| async move {
                match c {
                    Some(c) => Ok(c),
                    None => {
                        // Let the first part reach the client before dropping the connection.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Err(std::io::Error::other("truncated on purpose"))
                    }
                }
            });
            builder
                .header("content-length", total)
                .body(stream_body(s))
                .unwrap()
        }
        _ => builder
            .header("content-length", total)
            .body(full_body(data))
            .unwrap(),
    }
}

/// Deterministic pseudo-random content.
pub fn content(len: usize, seed: u64) -> Bytes {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect::<Vec<u8>>()
        .into()
}

/// A port with nothing listening on it.
pub fn dead_base(archive_path: &str) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}{archive_path}")
}

pub struct Proxy {
    child: Option<Child>,
    pub addr: String,
    dir: tempfile::TempDir,
}

impl Proxy {
    pub fn start(args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port_file = dir.path().join("port");
        let child = Command::new(env!("CARGO_BIN_EXE_unstuck-proxy"))
            .arg("--port-file")
            .arg(&port_file)
            .arg("--stats-file")
            .arg(dir.path().join("stats.json"))
            .arg("--temp-dir")
            .arg(dir.path())
            .args(args)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let addr = loop {
            if let Ok(s) = std::fs::read_to_string(&port_file) {
                break s.trim().to_string();
            }
            assert!(Instant::now() < deadline, "proxy did not start");
            std::thread::sleep(Duration::from_millis(20));
        };
        Self {
            child: Some(child),
            addr,
            dir,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn client(&self) -> reqwest::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::http(self.url()).unwrap())
            .build()
            .unwrap()
    }

    pub fn client_without_proxy(&self) -> reqwest::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    /// Sends SIGTERM, waits for exit and returns the stats file.
    pub fn stop(mut self) -> serde_json::Value {
        let mut child = self.child.take().unwrap();
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
        let status = child.wait().unwrap();
        assert!(status.success(), "proxy exited with {status}");
        let path: PathBuf = self.dir.path().join("stats.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
