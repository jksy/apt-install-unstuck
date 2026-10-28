//! Object storage backed cache (`s3://bucket/prefix` or `file:///path`).

use std::sync::Arc;

use anyhow::{Context, bail};
use object_store::aws::AmazonS3Builder;
use object_store::buffered::BufWriter;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{GetResult, ObjectMeta, ObjectStore, ObjectStoreExt};
use tokio::io::AsyncWriteExt;

/// Namespace reserved for hosts that are not described in the archive table.
#[allow(dead_code)]
pub const HOST_NAMESPACE: &str = "host";

pub struct Cache {
    store: Arc<dyn ObjectStore>,
    base: Vec<String>,
}

impl Cache {
    /// Returns `None` for `none` (or an empty spec).
    pub fn open(spec: &str, key_prefix: &str) -> anyhow::Result<Option<Self>> {
        let (store, root): (Arc<dyn ObjectStore>, &str) = if spec.is_empty() || spec == "none" {
            return Ok(None);
        } else if let Some(rest) = spec.strip_prefix("s3://") {
            let (bucket, root) = rest.split_once('/').unwrap_or((rest, ""));
            if bucket.is_empty() {
                bail!("missing bucket name in `{spec}`");
            }
            let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
            let endpoint = ["AWS_ENDPOINT_URL_S3", "AWS_ENDPOINT_URL", "AWS_ENDPOINT"]
                .iter()
                .find_map(|k| std::env::var(k).ok());
            if endpoint.is_some_and(|e| e.starts_with("http://")) {
                builder = builder.with_allow_http(true);
            }
            (
                Arc::new(builder.build().context("configuring S3 cache")?),
                root,
            )
        } else if let Some(path) = spec.strip_prefix("file://") {
            std::fs::create_dir_all(path)
                .with_context(|| format!("creating cache directory {path}"))?;
            (Arc::new(LocalFileSystem::new_with_prefix(path)?), "")
        } else {
            bail!("unsupported cache `{spec}` (expected none, s3://bucket/prefix or file:///path)");
        };
        let base = root
            .split('/')
            .chain(key_prefix.split('/'))
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        Ok(Some(Self { store, base }))
    }

    /// `<prefix>/<archive>/<path below the archive root>`
    pub fn archive_key(&self, archive: &str, rel: &str) -> Path {
        debug_assert_ne!(archive, HOST_NAMESPACE);
        self.key([archive].into_iter().chain(rel.split('/')))
    }

    /// `<prefix>/host/<hostname>/<path>`, reserved for caching undefined hosts.
    #[allow(dead_code)]
    pub fn host_key(&self, host: &str, path: &str) -> Path {
        self.key([HOST_NAMESPACE, host].into_iter().chain(path.split('/')))
    }

    fn key<'a>(&'a self, parts: impl Iterator<Item = &'a str>) -> Path {
        Path::from_iter(
            self.base
                .iter()
                .map(String::as_str)
                .chain(parts)
                .filter(|s| !s.is_empty()),
        )
    }

    pub async fn get(&self, key: &Path) -> object_store::Result<Option<GetResult>> {
        match self.store.get(key).await {
            Ok(r) => Ok(Some(r)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn head(&self, key: &Path) -> object_store::Result<Option<ObjectMeta>> {
        match self.store.head(key).await {
            Ok(r) => Ok(Some(r)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Uploads a local file, using multipart upload for large files.
    pub async fn put_file(&self, key: &Path, file: &std::path::Path) -> anyhow::Result<()> {
        let mut src = tokio::fs::File::open(file).await?;
        let mut dst = BufWriter::new(self.store.clone(), key.clone());
        match tokio::io::copy(&mut src, &mut dst).await {
            Ok(_) => {
                dst.shutdown().await?;
                Ok(())
            }
            Err(e) => {
                let _ = dst.abort().await;
                Err(e.into())
            }
        }
    }
}
