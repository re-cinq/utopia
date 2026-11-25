//! BlobStore: the seam for storing and fetching raw file bytes.
//!
//! Content-addressed: the key is the sha256 of the file content, and the interface has no notion of a "path". Locally it is a flat
//! directory before bucketing; object storage uses an object key; any KV store can implement it. Idempotence, deduplication and immutability
//! (when content changes its fingerprint changes, and the old version is never overwritten, which is the material basis of "version replay has material")
//! all come for free from "the content is its address".
//!
//! Today the only implementation is local disk (data/files/{sha256}). Object storage or a network drive later
//! (P5 connectors, multi-instance deployments sharing storage) needs only a new implementation; the callers of ingest/upload/parse/replay
//! do not change a line. The config entry UTOPIA_BLOB_BACKEND is reserved; only "local" is accepted for now.

use std::path::PathBuf;

#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Idempotent write: if the same fingerprint already exists, skip.
    async fn put(&self, sha256: &str, bytes: &[u8]) -> anyhow::Result<()>;
    async fn get(&self, sha256: &str) -> anyhow::Result<Vec<u8>>;
    #[allow(dead_code)] // #[allow(dead_code)] // Interface completeness: a future consumer of the replay/GC path
    async fn exists(&self, sha256: &str) -> anyhow::Result<bool>;
    /// Real delete (#268, second half): call only after the store confirms that no one still references this fingerprint. Idempotent: not existing also counts as success
    async fn delete(&self, sha256: &str) -> anyhow::Result<()>;
}

/// Local disk implementation: `{dir}/{sha256}` stored flat (byte-for-byte identical to the historical behavior).
pub struct LocalBlobStore {
    dir: PathBuf,
}

impl LocalBlobStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

#[async_trait::async_trait]
impl BlobStore for LocalBlobStore {
    async fn put(&self, sha256: &str, bytes: &[u8]) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let path = self.dir.join(sha256);
        if !path.exists() {
            tokio::fs::write(&path, bytes).await?;
        }
        Ok(())
    }

    async fn get(&self, sha256: &str) -> anyhow::Result<Vec<u8>> {
        Ok(tokio::fs::read(self.dir.join(sha256)).await?)
    }

    async fn exists(&self, sha256: &str) -> anyhow::Result<bool> {
        Ok(self.dir.join(sha256).exists())
    }

    async fn delete(&self, sha256: &str) -> anyhow::Result<()> {
        match tokio::fs::remove_file(self.dir.join(sha256)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
