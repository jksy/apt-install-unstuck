mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::{FakeMirror, Mode, Proxy, content, dead_base};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEB: &str = "pool/main/t/test/test_1.0_amd64.deb";
const IN_RELEASE: &str = "dists/noble/InRelease";

fn deb_url() -> String {
    format!("http://archive.ubuntu.com/ubuntu/{DEB}")
}

fn files(entries: &[(&str, &Bytes)]) -> HashMap<String, Bytes> {
    entries
        .iter()
        .map(|(rel, data)| (format!("/ubuntu/{rel}"), (*data).clone()))
        .collect()
}

fn mirrors(bases: &[String]) -> String {
    format!("ubuntu={}", bases.join(","))
}

/// Fast stall detection so the tests finish quickly.
const FAST: &[&str] = &[
    "--min-rate",
    "100K",
    "--stall-timeout",
    "1",
    "--first-byte-timeout",
    "2",
];

fn args<'a>(extra: &'a [&'a str]) -> Vec<&'a str> {
    FAST.iter().copied().chain(extra.iter().copied()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_mirror_fails_over_and_content_is_intact() {
    let data = content(400 * 1024, 1);
    let slow = FakeMirror::start(Mode::Slow(20 * 1024), files(&[(DEB, &data)])).await;
    let good = FakeMirror::start(Mode::Normal, files(&[(DEB, &data)])).await;
    let m = mirrors(&[slow.base("/ubuntu/"), good.base("/ubuntu/")]);
    let proxy = Proxy::start(&args(&["--mirrors", &m]));

    let started = Instant::now();
    let resp = proxy.client().get(deb_url()).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-unstuck-cache"], "miss");
    let body = resp.bytes().await.unwrap();
    assert_eq!(body, data);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(slow.hits(), 1);
    assert_eq!(good.hits(), 1);

    let stats = proxy.stop();
    assert_eq!(stats["failovers"], 1);
    assert_eq!(stats["misses"], 1);
    assert_eq!(stats["mirrors"][good.base("/ubuntu/")]["success"], 1);
    assert_eq!(stats["mirrors"][slow.base("/ubuntu/")]["failure"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_to_mirror_without_range_support_keeps_content_intact() {
    let data = content(300 * 1024, 2);
    let hang = FakeMirror::start(Mode::HangAfterHeaders, files(&[(DEB, &data)])).await;
    let slow = FakeMirror::start(Mode::Slow(30 * 1024), files(&[(DEB, &data)])).await;
    let norange = FakeMirror::start(Mode::NoRange, files(&[(DEB, &data)])).await;
    let m = mirrors(&[
        hang.base("/ubuntu/"),
        slow.base("/ubuntu/"),
        norange.base("/ubuntu/"),
    ]);
    let proxy = Proxy::start(&args(&["--mirrors", &m]));

    let body = proxy
        .client()
        .get(deb_url())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.len(), data.len());
    assert_eq!(body, data);
    assert_eq!((hang.hits(), slow.hits(), norange.hits()), (1, 1, 1));
    assert_eq!(proxy.stop()["failovers"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn not_found_on_first_mirror_falls_back_to_second() {
    let data = content(10_000, 3);
    let empty = FakeMirror::start(Mode::NotFound, HashMap::new()).await;
    let good = FakeMirror::start(Mode::Normal, files(&[(DEB, &data)])).await;
    let m = mirrors(&[empty.base("/ubuntu/"), good.base("/ubuntu/")]);
    let proxy = Proxy::start(&args(&["--mirrors", &m]));
    let client = proxy.client();

    let resp = client.get(deb_url()).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), data);

    // Missing everywhere: a plain 404, without extra rounds.
    let resp = client
        .get("http://archive.ubuntu.com/ubuntu/pool/main/m/missing.deb")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(good.hits(), 2);
    proxy.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn file_cache_serves_immutable_files_without_upstream() {
    let cache = tempfile::tempdir().unwrap();
    let cache_url = format!("file://{}", cache.path().display());
    let data = content(200_000, 4);
    let good = FakeMirror::start(Mode::Normal, files(&[(DEB, &data)])).await;
    let m = mirrors(&[good.base("/ubuntu/")]);

    let proxy = Proxy::start(&args(&[
        "--mirrors",
        &m,
        "--cache",
        &cache_url,
        "--cache-prefix",
        "test-24.04",
    ]));
    let client = proxy.client();
    assert_eq!(
        client
            .get(deb_url())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        data
    );
    let stats = proxy.stop();
    assert_eq!(stats["cache_writes"], 1);
    assert_eq!(good.hits(), 1);
    let stored = cache.path().join("test-24.04/ubuntu").join(DEB);
    assert_eq!(std::fs::read(&stored).unwrap(), data);

    // Same mirrors: served from the cache, the mirror sees nothing.
    let proxy = Proxy::start(&args(&[
        "--mirrors",
        &m,
        "--cache",
        &cache_url,
        "--cache-prefix",
        "test-24.04",
    ]));
    let client = proxy.client();
    let resp = client.get(deb_url()).send().await.unwrap();
    assert_eq!(resp.headers()["x-unstuck-cache"], "hit");
    assert_eq!(resp.bytes().await.unwrap(), data);
    let resp = client.head(deb_url()).send().await.unwrap();
    assert_eq!(
        resp.headers()["content-length"],
        data.len().to_string().as_str()
    );
    assert_eq!(good.hits(), 1);
    let stats = proxy.stop();
    assert_eq!(stats["hits"], 2);
    assert_eq!(stats["bytes_from_cache"], data.len());
    assert_eq!(stats["bytes_from_upstream"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn mutable_files_fall_back_to_stale_copy_when_all_mirrors_fail() {
    let cache = tempfile::tempdir().unwrap();
    let cache_url = format!("file://{}", cache.path().display());
    let release = content(5_000, 5);
    let good = FakeMirror::start(Mode::Normal, files(&[(IN_RELEASE, &release)])).await;
    let url = format!("http://archive.ubuntu.com/ubuntu/{IN_RELEASE}");

    let m = mirrors(&[good.base("/ubuntu/")]);
    let proxy = Proxy::start(&args(&["--mirrors", &m, "--cache", &cache_url]));
    let resp = proxy.client().get(&url).send().await.unwrap();
    assert!(resp.headers().get("warning").is_none());
    assert_eq!(resp.bytes().await.unwrap(), release);
    proxy.stop();

    let dead = mirrors(&[dead_base("/ubuntu/"), dead_base("/ubuntu/")]);
    let proxy = Proxy::start(&args(&[
        "--mirrors",
        &dead,
        "--cache",
        &cache_url,
        "--rounds",
        "1",
    ]));
    let client = proxy.client();
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-unstuck-cache"], "stale");
    assert!(
        resp.headers()["warning"]
            .to_str()
            .unwrap()
            .starts_with("110 ")
    );
    assert_eq!(resp.bytes().await.unwrap(), release);

    // Immutable files never use stale-if-error; uncached ones fail.
    let resp = client.get(deb_url()).send().await.unwrap();
    assert_eq!(resp.status(), 502);
    let stats = proxy.stop();
    assert_eq!(stats["stale"], 1);
    assert_eq!(stats["errors"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn incomplete_or_corrupt_downloads_are_not_cached() {
    let cache = tempfile::tempdir().unwrap();
    let cache_url = format!("file://{}", cache.path().display());
    let data = content(100_000, 6);
    let digest = hex::encode(Sha256::digest(&data));
    let good_hash = format!("dists/noble/main/binary-amd64/by-hash/SHA256/{digest}");
    let bad_hash = format!(
        "dists/noble/main/binary-amd64/by-hash/SHA256/{}",
        "0".repeat(64)
    );

    let truncating = FakeMirror::start(Mode::Truncate(40_000), files(&[(DEB, &data)])).await;
    let good = FakeMirror::start(
        Mode::Normal,
        files(&[(&good_hash, &data), (&bad_hash, &data)]),
    )
    .await;
    let m = format!(
        "ubuntu={},{}",
        truncating.base("/ubuntu/"),
        good.base("/ubuntu/")
    );
    // Only the truncating mirror has the .deb, so the transfer cannot complete.
    let proxy = Proxy::start(&args(&[
        "--mirrors",
        &m,
        "--cache",
        &cache_url,
        "--rounds",
        "1",
    ]));
    let client = proxy.client();

    let resp = client.get(deb_url()).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.bytes().await.is_err(),
        "a truncated body must not look complete"
    );

    for path in [&good_hash, &bad_hash] {
        let url = format!("http://archive.ubuntu.com/ubuntu/{path}");
        let body = client
            .get(&url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(body, data);
    }
    let stats = proxy.stop();
    assert_eq!(stats["cache_writes"], 1);

    let root = cache.path().join("default/ubuntu");
    assert!(!root.join(DEB).exists());
    assert!(root.join(&good_hash).exists());
    assert!(!root.join(&bad_hash).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_share_one_upstream_fetch() {
    let data = content(500_000, 7);
    let mirror = FakeMirror::start(
        Mode::Delay(Duration::from_millis(500)),
        files(&[(DEB, &data)]),
    )
    .await;
    let m = mirrors(&[mirror.base("/ubuntu/")]);
    let proxy = Proxy::start(&args(&["--mirrors", &m]));

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = proxy.client();
        tasks.push(tokio::spawn(async move {
            client
                .get(deb_url())
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), data);
    }
    assert_eq!(mirror.hits(), 1);
    let stats = proxy.stop();
    assert_eq!(stats["misses"], 8);
    assert_eq!(stats["bytes_from_upstream"], data.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn other_hosts_are_relayed_without_caching() {
    let cache = tempfile::tempdir().unwrap();
    let cache_url = format!("file://{}", cache.path().display());
    let data = content(1_000, 8);
    let mirror = FakeMirror::start(Mode::Normal, files(&[("pool/x.deb", &data)])).await;
    let proxy = Proxy::start(&args(&["--cache", &cache_url]));

    let resp = proxy
        .client()
        .get(format!("{}pool/x.deb", mirror.base("/ubuntu/")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-unstuck-cache").is_none());
    assert_eq!(resp.bytes().await.unwrap(), data);
    let stats = proxy.stop();
    assert_eq!(stats["passthrough"], 1);
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn health_and_unsupported_methods() {
    let proxy = Proxy::start(&[]);
    let direct = proxy.client_without_proxy();
    let resp = direct
        .get(format!("{}/_health", proxy.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = direct
        .get(format!("{}/_stats", proxy.url()))
        .send()
        .await
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(stats["hits"], 0);

    let resp = proxy.client().post(deb_url()).send().await.unwrap();
    assert_eq!(resp.status(), 405);

    let mut s = tokio::net::TcpStream::connect(&proxy.addr).await.unwrap();
    s.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 12];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"HTTP/1.1 405");
    proxy.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn if_modified_since_gets_304_for_unchanged_mutable_files() {
    let release = content(3_000, 9);
    let deb = content(3_000, 10);
    let mirror =
        FakeMirror::start(Mode::Normal, files(&[(IN_RELEASE, &release), (DEB, &deb)])).await;
    let m = mirrors(&[mirror.base("/ubuntu/")]);
    let proxy = Proxy::start(&args(&["--mirrors", &m]));
    let client = proxy.client();
    let url = format!("http://archive.ubuntu.com/ubuntu/{IN_RELEASE}");

    let resp = client
        .get(&url)
        .header("if-modified-since", common::LAST_MODIFIED)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);
    assert_eq!(resp.headers()["last-modified"], common::LAST_MODIFIED);
    assert!(resp.bytes().await.unwrap().is_empty());

    let older = "Mon, 01 Jan 2024 00:00:00 GMT";
    let resp = client
        .get(&url)
        .header("if-modified-since", older)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), release);

    // Immutable files ignore the condition.
    let resp = client
        .get(deb_url())
        .header("if-modified-since", common::LAST_MODIFIED)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), deb);
    proxy.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn apt_detect_sends_only_archives_to_a_healthy_proxy() {
    let detect = |proxy: &str, url: &str| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_unstuck-proxy"))
            .args(["apt-detect", "--proxy", proxy, url])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let archive = "http://azure.archive.ubuntu.com/ubuntu/dists/noble/InRelease";
    let ppa = "http://ppa.launchpadcontent.net/git-core/ppa/ubuntu/dists/noble/InRelease";

    let proxy = Proxy::start(&[]);
    let addr = proxy.addr.clone();
    assert_eq!(detect(&addr, archive), format!("http://{addr}"));
    assert_eq!(
        detect(
            &addr,
            "http://ports.ubuntu.com/ubuntu-ports/pool/main/x.deb"
        ),
        format!("http://{addr}")
    );
    assert_eq!(detect(&addr, ppa), "DIRECT");
    proxy.stop();
    assert_eq!(detect(&addr, archive), "DIRECT");
}
