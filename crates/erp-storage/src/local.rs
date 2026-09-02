//! Files on this machine's disk.
//!
//! # What it is for
//!
//! Two things, and they are the same shape: a development machine with no
//! object store, and **a tenant who keeps their own documents** (D15). The
//! second is not a fallback — for a business that will not put its records on
//! somebody else's hardware, it is the reason they can buy this at all.
//!
//! # What it is not for
//!
//! More than one process, unless they share a filesystem. Two API pods on two
//! machines with two local roots will each hold half a tenant's files and
//! neither will know. That is a deployment decision rather than a bug in this
//! file, and it is why the engine is recorded on every stored file: a tenant
//! that outgrows this can be moved, one file at a time, with the log saying
//! exactly which ones have moved.

use std::path::{Path, PathBuf};

use crate::{Storage, StorageError, check_key};

/// A directory, and everything under it.
#[derive(Debug, Clone)]
pub struct Local {
    root: PathBuf,
}

impl Local {
    /// Files under this directory. Created on first write, not here — a
    /// constructor that touches the disk cannot be called from a test that has
    /// not decided what it wants yet.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Where a key lands, once it has been proved to be one.
    ///
    /// **`check_key` first, always.** It is what makes joining a caller's string
    /// onto a root directory safe, and the one line between this and
    /// `../../etc`.
    fn path(&self, key: &str) -> Result<PathBuf, StorageError> {
        check_key(key)?;
        Ok(self.root.join(key))
    }
}

#[async_trait::async_trait]
impl Storage for Local {
    fn engine(&self) -> &'static str {
        "local"
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StorageError> {
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| unavailable(&e))?;
        }

        // **Written beside and renamed.** A crash halfway through a write would
        // otherwise leave a file whose checksum does not match its record —
        // which `fetch` would correctly refuse for ever, over bytes that were
        // never fully written. A rename on the same filesystem is atomic.
        let scratch = with_suffix(&path, ".part");
        tokio::fs::write(&scratch, bytes)
            .await
            .map_err(|e| unavailable(&e))?;
        tokio::fs::rename(&scratch, &path)
            .await
            .map_err(|e| unavailable(&e))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let path = self.path(key)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StorageError::NoSuchFile),
            Err(e) => Err(unavailable(&e)),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let path = self.path(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // Already gone is success: deleting twice is the same world either
            // way, and a caller cleaning up after a failed upload should not
            // have to care which half succeeded.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(unavailable(&e)),
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn unavailable(error: &std::io::Error) -> StorageError {
    StorageError::Unavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fetch, store};

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("erp-storage-{name}-{}", std::process::id()));
        path
    }

    #[tokio::test]
    async fn bytes_go_in_and_come_back() {
        let root = scratch("roundtrip");
        let local = Local::at(&root);

        let stored = store(&local, "invoice/INV-1/a.txt", b"hello", "text/plain")
            .await
            .expect("stores");
        assert_eq!(stored.engine, "local");
        assert_eq!(stored.size, 5);

        let back = fetch(&local, &stored).await.expect("fetches");
        assert_eq!(back, b"hello");

        local.delete("invoice/INV-1/a.txt").await.expect("deletes");
        assert_eq!(
            local.get("invoice/INV-1/a.txt").await,
            Err(StorageError::NoSuchFile)
        );
        // And again, because deleting twice is the same world either way.
        local.delete("invoice/INV-1/a.txt").await.expect("deletes");

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    /// **The failure a checksum exists to catch.**
    ///
    /// Something wrote over the file. `fetch` must refuse rather than hand back
    /// what it found.
    #[tokio::test]
    async fn a_file_that_came_back_different_is_refused() {
        let root = scratch("corrupt");
        let local = Local::at(&root);

        let stored = store(&local, "a/b.txt", b"the real thing", "text/plain")
            .await
            .expect("stores");
        local
            .put("a/b.txt", b"something else")
            .await
            .expect("overwrites");

        assert!(matches!(
            fetch(&local, &stored).await,
            Err(StorageError::Corrupt { .. })
        ));

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn a_key_that_climbs_out_is_refused_before_it_touches_the_disk() {
        let local = Local::at(scratch("traversal"));
        assert!(matches!(
            local.put("../escaped.txt", b"x").await,
            Err(StorageError::NotAKey(_))
        ));
        assert!(matches!(
            local.get("../../etc/passwd").await,
            Err(StorageError::NotAKey(_))
        ));
    }

    #[tokio::test]
    async fn a_file_over_the_limit_is_refused() {
        let local = Local::at(scratch("big"));
        let big = vec![0u8; crate::MAX_BYTES + 1];
        assert_eq!(
            store(&local, "a/b.bin", &big, "application/octet-stream").await,
            Err(StorageError::TooLarge)
        );
    }
}
