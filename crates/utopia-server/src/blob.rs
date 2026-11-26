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
    #[allow(dead_code)] // Interface completeness: a future consumer of the replay/GC path
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
        // A full disk once left empty blobs under the right name; skipping on mere existence made them permanent, so only a file of the right length is skipped
        let intact = matches!(tokio::fs::metadata(&path).await, Ok(meta) if meta.len() == bytes.len() as u64);
        if !intact {
            let partial = self
                .dir
                .join(format!("{sha256}.{}.tmp", uuid::Uuid::now_v7()));
            tokio::fs::write(&partial, bytes).await?;
            tokio::fs::rename(&partial, &path).await?;
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

#[cfg(test)]
mod tests {
    use super::{BlobStore, LocalBlobStore};

    const SHA: &str = "abc123";

    fn store() -> anyhow::Result<(tempfile::TempDir, LocalBlobStore)> {
        let dir = tempfile::tempdir()?;
        let store = LocalBlobStore::new(dir.path().to_path_buf());
        Ok((dir, store))
    }

    #[tokio::test]
    async fn put_keeps_an_existing_file_of_the_right_length() -> anyhow::Result<()> {
        let (dir, store) = store()?;
        std::fs::write(dir.path().join(SHA), b"sentinel!!")?;

        store.put(SHA, b"Port Ellen").await?;

        assert_eq!(store.get(SHA).await?, b"sentinel!!");
        Ok(())
    }

    #[tokio::test]
    async fn put_replaces_an_empty_file_with_the_full_bytes() -> anyhow::Result<()> {
        let (dir, store) = store()?;
        std::fs::write(dir.path().join(SHA), b"")?;

        store.put(SHA, b"Port Ellen").await?;

        assert_eq!(store.get(SHA).await?, b"Port Ellen");
        Ok(())
    }

    #[tokio::test]
    async fn put_replaces_a_truncated_file_with_the_full_bytes() -> anyhow::Result<()> {
        let (dir, store) = store()?;
        std::fs::write(dir.path().join(SHA), b"Port")?;

        store.put(SHA, b"Port Ellen").await?;

        assert_eq!(store.get(SHA).await?, b"Port Ellen");
        Ok(())
    }

    #[tokio::test]
    async fn put_leaves_only_the_blob_in_the_directory() -> anyhow::Result<()> {
        let (dir, store) = store()?;
        std::fs::write(dir.path().join(SHA), b"Port")?;

        store.put(SHA, b"Port Ellen").await?;

        let names: Vec<String> = std::fs::read_dir(dir.path())?
            .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()?;
        assert_eq!(names, vec![SHA.to_string()]);
        Ok(())
    }
}
