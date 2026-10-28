//! `apt-detect`: hook for apt's `Acquire::http::Proxy-Auto-Detect`.
//!
//! apt runs it (as the `_apt` user) with the URL it is about to fetch and
//! uses the line printed on stdout as the proxy. Requests for archives in the
//! table go to the running proxy; everything else, and everything while the
//! proxy is not answering, goes `DIRECT`. It must always exit 0: apt treats a
//! failing hook as a fatal error.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use hyper::Uri;

use crate::archive;

const DIRECT: &str = "DIRECT";

pub fn run(proxy: &str, url: &str) {
    println!("{}", decide(proxy, url));
}

fn decide(proxy: &str, url: &str) -> String {
    let Ok(uri) = url.parse::<Uri>() else {
        return DIRECT.into();
    };
    if archive::match_request(&uri).is_none() || !healthy(proxy) {
        return DIRECT.into();
    }
    format!("http://{proxy}")
}

fn healthy(proxy: &str) -> bool {
    let Ok(addr) = proxy.parse::<SocketAddr>() else {
        return false;
    };
    let timeout = Duration::from_secs(2);
    let Ok(mut s) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    if s.write_all(b"GET /_health HTTP/1.0\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    let mut n = 0;
    while n < buf.len() {
        match s.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1) == Some("200")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_archive_and_unreachable_go_direct() {
        // Nothing listens on port 9; archive URLs fall back to DIRECT too.
        let dead = "127.0.0.1:9";
        assert_eq!(
            decide(
                dead,
                "http://archive.ubuntu.com/ubuntu/dists/noble/InRelease"
            ),
            "DIRECT"
        );
        assert_eq!(
            decide(
                dead,
                "http://ppa.launchpadcontent.net/x/ppa/ubuntu/dists/noble/InRelease"
            ),
            "DIRECT"
        );
        assert_eq!(decide(dead, "not a url"), "DIRECT");
        assert_eq!(
            decide("garbage", "http://archive.ubuntu.com/ubuntu/pool/x.deb"),
            "DIRECT"
        );
    }
}
