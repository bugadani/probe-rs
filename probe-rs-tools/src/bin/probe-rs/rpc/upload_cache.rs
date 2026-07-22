//! Content-addressed client upload cache for remote RPC sessions.
//!
//! Remote uploads are keyed by `(canonical source path, content hash)` so
//! changed bytes at the same path produce a new upload, while identical
//! content can be reused within bounded memory.

use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// SHA-256 digest of a local file's contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContentHash([u8; 32]);

impl ContentHash {
    pub(crate) fn from_bytes(data: &[u8]) -> Self {
        let digest = Sha256::digest(data);
        Self(digest.into())
    }
}

/// A resolved local upload: canonical path, content identity, and the path
/// the RPC server should read (remote temp path, or the local path in-process).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedUpload {
    pub(crate) canonical_path: PathBuf,
    pub(crate) content_hash: ContentHash,
    pub(crate) remote_path: PathBuf,
}

impl ResolvedUpload {
    pub(crate) fn server_path(&self) -> &Path {
        &self.remote_path
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct UploadCacheKey {
    canonical_path: PathBuf,
    content_hash: ContentHash,
}

/// Bounded cache of prior remote uploads.
#[derive(Debug, Default)]
pub(crate) struct UploadCache {
    entries: HashMap<UploadCacheKey, PathBuf>,
    max_entries: usize,
}

impl UploadCache {
    pub(crate) fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: max_entries.max(1),
        }
    }

    pub(crate) fn lookup(
        &self,
        canonical_path: &Path,
        content_hash: ContentHash,
    ) -> Option<PathBuf> {
        self.entries
            .get(&UploadCacheKey {
                canonical_path: canonical_path.to_path_buf(),
                content_hash,
            })
            .cloned()
    }

    /// Record a successful upload. Replaces any prior entry for the same
    /// canonical path with a different hash.
    pub(crate) fn insert(
        &mut self,
        canonical_path: PathBuf,
        content_hash: ContentHash,
        remote_path: PathBuf,
    ) {
        self.entries.retain(|key, _| {
            key.canonical_path != canonical_path || key.content_hash == content_hash
        });

        let key = UploadCacheKey {
            canonical_path: canonical_path.clone(),
            content_hash,
        };
        self.entries.insert(key.clone(), remote_path);

        while self.entries.len() > self.max_entries {
            let Some(victim) = self.entries.keys().find(|k| *k != &key).cloned() else {
                break;
            };
            self.entries.remove(&victim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(bytes: &[u8]) -> ContentHash {
        ContentHash::from_bytes(bytes)
    }

    #[test]
    fn changed_content_at_same_path_uses_distinct_keys() {
        let mut cache = UploadCache::with_capacity(16);
        let path = PathBuf::from("/tmp/firmware.elf");

        cache.insert(path.clone(), hash(b"v1"), PathBuf::from("/remote/v1"));
        assert_eq!(
            cache.lookup(&path, hash(b"v1")),
            Some(PathBuf::from("/remote/v1"))
        );
        assert_eq!(cache.lookup(&path, hash(b"v2")), None);

        cache.insert(path.clone(), hash(b"v2"), PathBuf::from("/remote/v2"));
        assert_eq!(
            cache.lookup(&path, hash(b"v2")),
            Some(PathBuf::from("/remote/v2"))
        );
        assert_eq!(cache.lookup(&path, hash(b"v1")), None);
    }

    #[test]
    fn insert_does_not_remove_unrelated_paths() {
        let mut cache = UploadCache::with_capacity(16);
        let elf = PathBuf::from("/tmp/firmware.elf");
        let svd = PathBuf::from("/tmp/chip.svd");

        cache.insert(elf.clone(), hash(b"elf"), PathBuf::from("/remote/elf"));
        cache.insert(svd.clone(), hash(b"svd"), PathBuf::from("/remote/svd"));

        assert_eq!(
            cache.lookup(&elf, hash(b"elf")),
            Some(PathBuf::from("/remote/elf"))
        );
        assert_eq!(
            cache.lookup(&svd, hash(b"svd")),
            Some(PathBuf::from("/remote/svd"))
        );
    }

    #[test]
    fn cache_is_bounded() {
        let mut cache = UploadCache::with_capacity(2);

        cache.insert(PathBuf::from("/a"), hash(b"1"), PathBuf::from("/remote/1"));
        cache.insert(PathBuf::from("/b"), hash(b"2"), PathBuf::from("/remote/2"));
        cache.insert(PathBuf::from("/c"), hash(b"3"), PathBuf::from("/remote/3"));

        assert_eq!(cache.entries.len(), 2);
        assert_eq!(
            cache.lookup(Path::new("/c"), hash(b"3")),
            Some(PathBuf::from("/remote/3"))
        );
    }

    #[test]
    fn prior_entry_survives_when_failed_upload_skips_insert() {
        let mut cache = UploadCache::with_capacity(16);
        let path = PathBuf::from("/tmp/firmware.elf");

        cache.insert(path.clone(), hash(b"v1"), PathBuf::from("/remote/v1"));
        // A failed upload for changed content must not replace the prior entry.
        assert_eq!(
            cache.lookup(&path, hash(b"v1")),
            Some(PathBuf::from("/remote/v1"))
        );
        assert_eq!(cache.lookup(&path, hash(b"v2")), None);
    }
}
