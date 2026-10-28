mod archive;
mod cache;
mod detect;
mod fetch;
mod proxy;
mod rate;
mod stats;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::task::TaskTracker;

use crate::archive::MirrorConfig;
use crate::cache::Cache;
use crate::fetch::FetchConfig;
use crate::proxy::App;

/// HTTP forward proxy that keeps apt downloads moving by switching Ubuntu
/// mirrors when one stalls, with an optional shared cache.
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Print the proxy apt should use for URL, or DIRECT. Meant for
    /// `Acquire::http::Proxy-Auto-Detect`; always exits 0.
    AptDetect {
        /// Address of the running proxy, e.g. 127.0.0.1:3142.
        #[arg(long)]
        proxy: String,
        url: String,
    },
}

#[derive(Args)]
struct ServeArgs {
    /// Address to listen on. Port 0 picks a free port.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    /// Write the address actually listened on to this file.
    #[arg(long)]
    port_file: Option<PathBuf>,
    /// Upstreams per archive, e.g. `ubuntu=origin,http://archive.ubuntu.com/ubuntu/;ubuntu-ports=...`.
    /// `origin` stands for the host in the request; a list replaces the archive's defaults.
    #[arg(long, default_value = "")]
    mirrors: String,
    /// `none`, `s3://bucket/prefix` or `file:///path`.
    #[arg(long, default_value = "none")]
    cache: String,
    /// Key prefix inside the cache, e.g. `ubuntu-24.04`.
    #[arg(long, default_value = "default")]
    cache_prefix: String,
    /// Minimum transfer rate in bytes/s (suffixes K and M are KiB and MiB).
    #[arg(long, default_value = "50K", value_parser = parse_rate)]
    min_rate: u64,
    /// Seconds below --min-rate before an upstream transfer is abandoned.
    #[arg(long, default_value_t = 20.0)]
    stall_timeout: f64,
    /// Seconds to wait for response headers from an upstream.
    #[arg(long, default_value_t = 15.0)]
    first_byte_timeout: f64,
    /// How many times to go through the mirror list.
    #[arg(long, default_value_t = 2)]
    rounds: u32,
    /// Write statistics as JSON to this file on shutdown.
    #[arg(long)]
    stats_file: Option<PathBuf>,
    /// Directory for in-progress downloads (defaults to the system temp dir).
    #[arg(long)]
    temp_dir: Option<PathBuf>,
}

fn parse_rate(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mul) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => {
            let mul = match s[i..].to_ascii_lowercase().as_str() {
                "k" | "kb" | "kib" => 1024,
                "m" | "mb" | "mib" => 1024 * 1024,
                "b" => 1,
                other => return Err(format!("unknown unit `{other}`")),
            };
            (&s[..i], mul)
        }
        None => (s, 1),
    };
    num.trim()
        .parse::<u64>()
        .map(|n| n * mul)
        .map_err(|e| format!("invalid rate `{s}`: {e}"))
}

fn secs(v: f64, name: &str) -> anyhow::Result<Duration> {
    Duration::try_from_secs_f64(v).with_context(|| format!("invalid --{name}"))
}

fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::AptDetect { proxy, url }) => {
            detect::run(&proxy, &url);
            Ok(())
        }
        None => tokio::runtime::Runtime::new()?.block_on(serve(cli.serve)),
    }
}

async fn serve(cli: ServeArgs) -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let fetch = FetchConfig {
        min_rate: cli.min_rate,
        stall_timeout: secs(cli.stall_timeout, "stall-timeout")?,
        first_byte_timeout: secs(cli.first_byte_timeout, "first-byte-timeout")?,
        rounds: cli.rounds.max(1),
    };
    let user_agent = concat!("unstuck-proxy/", env!("CARGO_PKG_VERSION"));
    // Bundled roots on top of the system's, so the proxy also starts in
    // containers without ca-certificates (mirrors are plain HTTP anyway).
    let roots = || {
        webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter_map(|c| reqwest::Certificate::from_der(c).ok())
    };
    let mirror_client = reqwest::Client::builder()
        .tls_certs_merge(roots())
        .no_proxy()
        .user_agent(user_agent)
        .connect_timeout(fetch.first_byte_timeout)
        .tcp_keepalive(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let passthrough_client = reqwest::Client::builder()
        .tls_certs_merge(roots())
        .no_proxy()
        .connect_timeout(fetch.first_byte_timeout)
        .tcp_keepalive(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let app = Arc::new(App {
        fetch,
        mirror_client,
        passthrough_client,
        mirrors: MirrorConfig::parse(&cli.mirrors)?,
        cache: Cache::open(&cli.cache, &cli.cache_prefix)?,
        stats: Default::default(),
        inflight: Default::default(),
        tracker: TaskTracker::new(),
        temp_dir: cli.temp_dir.clone().unwrap_or_else(std::env::temp_dir),
    });

    let listener = TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    let addr = listener.local_addr()?;
    if let Some(p) = &cli.port_file {
        write_atomic(p, addr.to_string().as_bytes())
            .with_context(|| format!("writing {}", p.display()))?;
    }
    eprintln!("unstuck-proxy: listening on {addr} (cache: {})", cli.cache);

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((s, _)) => s,
                    Err(e) => {
                        eprintln!("unstuck-proxy: accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let _ = stream.set_nodelay(true);
                let app = app.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let app = app.clone();
                        async move { Ok::<_, Infallible>(proxy::handle(app, req).await) }
                    });
                    let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
                });
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }
    drop(listener);

    app.tracker.close();
    if tokio::time::timeout(Duration::from_secs(30), app.tracker.wait())
        .await
        .is_err()
    {
        eprintln!("unstuck-proxy: gave up waiting for in-progress downloads and cache writes");
    }
    let snapshot = app.stats.snapshot();
    let json = serde_json::to_string_pretty(&snapshot)?;
    eprintln!(
        "unstuck-proxy: stopped: {}",
        serde_json::to_string(&snapshot)?
    );
    if let Some(p) = &cli.stats_file {
        write_atomic(p, json.as_bytes()).with_context(|| format!("writing {}", p.display()))?;
    }
    std::process::exit(0);
}
