//! Archive definitions and request classification.
//!
//! Everything that is specific to a repository layout lives in the `ARCHIVES`
//! table so that other distributions can be supported by adding entries.

use std::collections::HashMap;

use anyhow::{Context, bail};
use hyper::Uri;
use percent_encoding::percent_decode_str;

/// Placeholder in a mirror list for the host the client originally asked for.
pub const ORIGIN: &str = "origin";

pub enum HostMatch {
    Exact(&'static str),
    /// Matches any host ending with this suffix (include the leading dot).
    Suffix(&'static str),
}

impl HostMatch {
    fn matches(&self, host: &str) -> bool {
        match self {
            HostMatch::Exact(h) => host == *h,
            HostMatch::Suffix(s) => host.ends_with(s) && host.len() > s.len(),
        }
    }
}

/// A rule evaluated against the path relative to the archive root.
pub enum PathRule {
    Prefix(&'static str),
    Contains(&'static str),
}

impl PathRule {
    fn matches(&self, rel: &str) -> bool {
        match self {
            PathRule::Prefix(p) => rel.starts_with(p),
            PathRule::Contains(c) => rel.contains(c),
        }
    }
}

pub struct ArchiveDef {
    /// Archive name. Also the cache key namespace, so it must not be `host`.
    pub name: &'static str,
    pub hosts: &'static [HostMatch],
    /// Path prefix of the archive root on the origin host, with slashes on both ends.
    pub path_prefix: &'static str,
    /// Upstreams used when `--mirrors` does not mention this archive.
    pub default_mirrors: &'static [&'static str],
    /// Files whose content never changes for a given path.
    pub immutable: &'static [PathRule],
    /// Files that change over time and are only served from cache when every upstream fails.
    pub mutable: &'static [PathRule],
}

const DEBIAN_LIKE_IMMUTABLE: &[PathRule] =
    &[PathRule::Prefix("pool/"), PathRule::Contains("/by-hash/")];
const DEBIAN_LIKE_MUTABLE: &[PathRule] = &[PathRule::Prefix("dists/")];

pub static ARCHIVES: &[ArchiveDef] = &[
    ArchiveDef {
        name: "ubuntu",
        hosts: &[
            HostMatch::Exact("archive.ubuntu.com"),
            HostMatch::Suffix(".archive.ubuntu.com"),
            HostMatch::Exact("security.ubuntu.com"),
        ],
        path_prefix: "/ubuntu/",
        default_mirrors: &[
            ORIGIN,
            "http://archive.ubuntu.com/ubuntu/",
            "http://mirrors.edge.kernel.org/ubuntu/",
        ],
        immutable: DEBIAN_LIKE_IMMUTABLE,
        mutable: DEBIAN_LIKE_MUTABLE,
    },
    ArchiveDef {
        name: "ubuntu-ports",
        hosts: &[
            HostMatch::Exact("ports.ubuntu.com"),
            HostMatch::Suffix(".ports.ubuntu.com"),
        ],
        path_prefix: "/ubuntu-ports/",
        default_mirrors: &[
            ORIGIN,
            "http://ports.ubuntu.com/ubuntu-ports/",
            "http://mirrors.edge.kernel.org/ubuntu-ports/",
        ],
        immutable: DEBIAN_LIKE_IMMUTABLE,
        mutable: DEBIAN_LIKE_MUTABLE,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileClass {
    Immutable,
    Mutable,
    /// Inside a known archive but neither rule matched: failover only, never cached.
    Uncached,
}

pub struct ArchiveRequest {
    pub def: &'static ArchiveDef,
    /// `http://<authority><path_prefix>` of the original request.
    pub origin_base: String,
    /// Path below the archive root, as sent by the client (percent-encoded).
    pub rel_encoded: String,
    /// Decoded path below the archive root; used for cache keys.
    pub rel: String,
    pub class: FileClass,
}

/// Returns the archive a request belongs to, or `None` for plain passthrough.
pub fn match_request(uri: &Uri) -> Option<ArchiveRequest> {
    if uri.scheme_str() != Some("http") || uri.query().is_some() {
        return None;
    }
    let authority = uri.authority()?;
    let host = authority.host().to_ascii_lowercase();
    let path = uri.path();
    let def = ARCHIVES
        .iter()
        .find(|d| d.hosts.iter().any(|h| h.matches(&host)) && path.starts_with(d.path_prefix))?;
    let rel_encoded = &path[def.path_prefix.len()..];
    let rel = percent_decode_str(rel_encoded)
        .decode_utf8()
        .ok()?
        .into_owned();
    if !is_safe_rel(&rel) {
        return None;
    }
    let class = if def.immutable.iter().any(|r| r.matches(&rel)) {
        FileClass::Immutable
    } else if def.mutable.iter().any(|r| r.matches(&rel)) {
        FileClass::Mutable
    } else {
        FileClass::Uncached
    };
    Some(ArchiveRequest {
        def,
        origin_base: format!(
            "http://{}{}",
            authority.as_str().to_ascii_lowercase(),
            def.path_prefix
        ),
        rel_encoded: rel_encoded.to_string(),
        rel,
        class,
    })
}

fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.ends_with('/')
        && rel
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashAlgo {
    Sha256,
    Sha512,
    Md5,
}

/// Extracts the expected digest from a `.../by-hash/<ALGO>/<hex>` path.
pub fn by_hash(rel: &str) -> Option<(HashAlgo, String)> {
    let mut segs = rel.split('/');
    segs.by_ref().find(|s| *s == "by-hash")?;
    let algo = match segs.next()? {
        "SHA256" => HashAlgo::Sha256,
        "SHA512" => HashAlgo::Sha512,
        "MD5Sum" => HashAlgo::Md5,
        _ => return None,
    };
    let hex = segs.next()?;
    if segs.next().is_some() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((algo, hex.to_ascii_lowercase()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Upstream {
    Origin,
    Url(String),
}

/// Upstream lists per archive, built from the table defaults and `--mirrors`.
pub struct MirrorConfig {
    lists: HashMap<&'static str, Vec<Upstream>>,
}

impl MirrorConfig {
    /// Parses `ubuntu=URL,URL;ubuntu-ports=origin,URL`. A list given for an
    /// archive replaces its defaults; `origin` marks where the requested host goes.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let mut lists: HashMap<&'static str, Vec<Upstream>> = ARCHIVES
            .iter()
            .map(|d| {
                (
                    d.name,
                    d.default_mirrors
                        .iter()
                        .map(|m| parse_upstream(m))
                        .collect(),
                )
            })
            .collect();
        for entry in spec.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            let (name, urls) = entry.split_once('=').with_context(|| {
                format!("mirror entry `{entry}` must look like archive=URL,URL")
            })?;
            let def = ARCHIVES
                .iter()
                .find(|d| d.name == name.trim())
                .with_context(|| format!("unknown archive `{}` in --mirrors", name.trim()))?;
            let list: Vec<Upstream> = urls
                .split(',')
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(parse_upstream)
                .collect();
            for u in &list {
                if let Upstream::Url(url) = u
                    && !(url.starts_with("http://") || url.starts_with("https://"))
                {
                    bail!("mirror `{url}` must be an http:// or https:// URL");
                }
            }
            if list.is_empty() {
                bail!("no mirrors given for archive `{}`", def.name);
            }
            lists.insert(def.name, list);
        }
        Ok(Self { lists })
    }

    /// Base URLs (ending with `/`) to try for a request, in order, without duplicates.
    pub fn resolve(&self, req: &ArchiveRequest) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for u in self.lists.get(req.def.name).into_iter().flatten() {
            let base = match u {
                Upstream::Origin => req.origin_base.clone(),
                Upstream::Url(url) => url.clone(),
            };
            if !out.contains(&base) {
                out.push(base);
            }
        }
        out
    }
}

fn parse_upstream(s: &str) -> Upstream {
    if s == ORIGIN {
        Upstream::Origin
    } else if s.ends_with('/') {
        Upstream::Url(s.to_string())
    } else {
        Upstream::Url(format!("{s}/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(url: &str) -> Option<ArchiveRequest> {
        match_request(&url.parse().unwrap())
    }

    #[test]
    fn classifies_ubuntu_paths() {
        let r = req("http://azure.archive.ubuntu.com/ubuntu/pool/main/a/apt/apt_2.7_amd64.deb")
            .unwrap();
        assert_eq!(r.def.name, "ubuntu");
        assert_eq!(r.class, FileClass::Immutable);
        assert_eq!(r.origin_base, "http://azure.archive.ubuntu.com/ubuntu/");
        let r = req("http://security.ubuntu.com/ubuntu/dists/noble-security/InRelease").unwrap();
        assert_eq!(r.class, FileClass::Mutable);
        let r = req(
            "http://archive.ubuntu.com/ubuntu/dists/noble/main/binary-amd64/by-hash/SHA256/abcd",
        )
        .unwrap();
        assert_eq!(r.class, FileClass::Immutable);
        let r = req("http://archive.ubuntu.com/ubuntu/ls-lR.gz").unwrap();
        assert_eq!(r.class, FileClass::Uncached);
        let r = req("http://ports.ubuntu.com/ubuntu-ports/pool/main/x.deb").unwrap();
        assert_eq!(r.def.name, "ubuntu-ports");
    }

    #[test]
    fn decodes_and_rejects_paths() {
        let r = req("http://archive.ubuntu.com/ubuntu/pool/main/g/gcc/libstdc%2b%2b6_1_amd64.deb")
            .unwrap();
        assert_eq!(r.rel, "pool/main/g/gcc/libstdc++6_1_amd64.deb");
        assert!(req("http://archive.ubuntu.com/ubuntu/pool/../etc/passwd").is_none());
        assert!(req("http://archive.ubuntu.com/ubuntu/pool//x").is_none());
        assert!(req("http://archive.ubuntu.com/ubuntu/pool/x?y=1").is_none());
        assert!(req("http://example.com/ubuntu/pool/x").is_none());
        assert!(req("http://ports.ubuntu.com/ubuntu/pool/x").is_none());
        assert!(req("http://evilarchive.ubuntu.com.example/ubuntu/pool/x").is_none());
    }

    #[test]
    fn parses_by_hash() {
        assert_eq!(
            by_hash("dists/noble/main/binary-amd64/by-hash/SHA256/ABcd"),
            Some((HashAlgo::Sha256, "abcd".into()))
        );
        assert_eq!(
            by_hash("dists/noble/by-hash/MD5Sum/00"),
            Some((HashAlgo::Md5, "00".into()))
        );
        assert_eq!(by_hash("dists/noble/by-hash/SHA1/00"), None);
        assert_eq!(by_hash("pool/main/x.deb"), None);
    }

    #[test]
    fn resolves_mirrors() {
        let r = req("http://archive.ubuntu.com/ubuntu/pool/x.deb").unwrap();
        let cfg = MirrorConfig::parse("").unwrap();
        assert_eq!(
            cfg.resolve(&r),
            vec![
                "http://archive.ubuntu.com/ubuntu/",
                "http://mirrors.edge.kernel.org/ubuntu/"
            ]
        );
        let cfg =
            MirrorConfig::parse("ubuntu=http://a/ubuntu, origin ;ubuntu-ports=http://b/").unwrap();
        assert_eq!(
            cfg.resolve(&r),
            vec!["http://a/ubuntu/", "http://archive.ubuntu.com/ubuntu/"]
        );
        let cfg = MirrorConfig::parse("ubuntu=http://a/ubuntu/").unwrap();
        assert_eq!(cfg.resolve(&r), vec!["http://a/ubuntu/"]);
        assert!(MirrorConfig::parse("debian=http://a/").is_err());
        assert!(MirrorConfig::parse("ubuntu=ftp://a/").is_err());
    }
}
