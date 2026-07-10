//! Content-addressed media blobs (§13). The [`BlobStore`] port stores raw bytes
//! named by their **BLAKE3** hash — dedup is by construction (identical bytes →
//! one hash → one stored object). The in-memory impl lives here (tokio-free, for
//! core + tests); the filesystem CAS impl is in `weftd` (it needs async fs I/O).
//!
//! Metadata is minimal for the M-media-0 spike (`mime`, `bytes`); dimensions,
//! duration, and refcount-to-retention land in M-media-1.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::StoreError;

/// A blob's BLAKE3 content hash as lowercase hex (64 chars) — its stable name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobHash(String);

impl BlobHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Validate + normalize a hex hash (64 hex chars); `None` if malformed.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| BlobHash(s.to_ascii_lowercase()))
    }
}

impl std::fmt::Display for BlobHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The content hash of `bytes` (BLAKE3). The single hashing point both backends
/// share, so a blob's name never depends on which store held it.
pub fn blob_hash(bytes: &[u8]) -> BlobHash {
    BlobHash(blake3::hash(bytes).to_hex().to_string())
}

/// Minimal per-object metadata the [`BlobStore`] itself tracks (M-media-0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    pub mime: String,
    pub bytes: u64,
}

/// A stored blob's full record in the [`MediaStore`](crate::MediaStore)
/// (M-media-1b): content type, size, image dimensions, and a linked
/// server-generated **thumbnail** blob hash (images only). `created_ms` is the
/// GC grace anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRecord {
    pub hash: String,
    pub mime: String,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Hash of the derived thumbnail blob, if one was generated.
    pub thumb: Option<String>,
    pub created_ms: u64,
}

/// Inclusive HTTP-style byte range (`bytes=start-end`) applied to a blob.
fn slice_range(data: &[u8], range: Option<(u64, u64)>) -> Vec<u8> {
    match range {
        None => data.to_vec(),
        Some((start, end)) => {
            let start = (start as usize).min(data.len());
            // `end` is inclusive; clamp to the buffer.
            let end = (end as usize).saturating_add(1).min(data.len());
            data.get(start..end).unwrap_or(&[]).to_vec()
        }
    }
}

/// Content-addressed blob storage (§13). `put` is idempotent: identical bytes
/// dedup to one object. Fetches are byte-range-capable (ranged video, §13).
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Store `bytes` under their BLAKE3 hash; returns the hash. Storing bytes
    /// that already exist is a no-op that returns the same hash (dedup).
    async fn put(&self, mime: &str, bytes: &[u8]) -> Result<BlobHash, StoreError>;

    /// Fetch a blob (or an inclusive byte range of it); `None` if absent.
    async fn get(
        &self,
        hash: &BlobHash,
        range: Option<(u64, u64)>,
    ) -> Result<Option<Vec<u8>>, StoreError>;

    /// Metadata for a stored blob, if present.
    async fn stat(&self, hash: &BlobHash) -> Result<Option<BlobMeta>, StoreError>;

    /// Whether the blob exists (default: derived from `stat`).
    async fn exists(&self, hash: &BlobHash) -> Result<bool, StoreError> {
        Ok(self.stat(hash).await?.is_some())
    }

    /// Delete a blob's bytes (GC of an orphaned blob, §13). Idempotent.
    async fn delete(&self, hash: &BlobHash) -> Result<(), StoreError>;
}

/// In-memory content-addressed store (tests + memory deployments).
#[derive(Default)]
pub struct MemBlobStore {
    blobs: Mutex<HashMap<BlobHash, (BlobMeta, Vec<u8>)>>,
}

#[async_trait]
impl BlobStore for MemBlobStore {
    async fn put(&self, mime: &str, bytes: &[u8]) -> Result<BlobHash, StoreError> {
        let hash = blob_hash(bytes);
        self.blobs
            .lock()
            .expect("blob lock")
            .entry(hash.clone())
            .or_insert_with(|| {
                (
                    BlobMeta {
                        mime: mime.to_string(),
                        bytes: bytes.len() as u64,
                    },
                    bytes.to_vec(),
                )
            });
        Ok(hash)
    }

    async fn get(
        &self,
        hash: &BlobHash,
        range: Option<(u64, u64)>,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .blobs
            .lock()
            .expect("blob lock")
            .get(hash)
            .map(|(_, data)| slice_range(data, range)))
    }

    async fn stat(&self, hash: &BlobHash) -> Result<Option<BlobMeta>, StoreError> {
        Ok(self
            .blobs
            .lock()
            .expect("blob lock")
            .get(hash)
            .map(|(meta, _)| meta.clone()))
    }

    async fn delete(&self, hash: &BlobHash) -> Result<(), StoreError> {
        self.blobs.lock().expect("blob lock").remove(hash);
        Ok(())
    }
}

/// The shared behavioral contract every [`BlobStore`] backend must satisfy —
/// run against `MemBlobStore` here and `FsBlobStore` in `weftd` (house rule:
/// one contract, every backend).
pub async fn blob_store_contract<S: BlobStore>(store: &S) {
    let data = b"the quick brown fox".to_vec();
    let hash = store.put("text/plain", &data).await.unwrap();

    // Content addressing: same bytes → same hash, deterministic.
    assert_eq!(hash, blob_hash(&data));
    assert_eq!(store.put("text/plain", &data).await.unwrap(), hash); // dedup

    // Round-trip + metadata.
    assert_eq!(
        store.get(&hash, None).await.unwrap().as_deref(),
        Some(&data[..])
    );
    assert!(store.exists(&hash).await.unwrap());
    let meta = store.stat(&hash).await.unwrap().unwrap();
    assert_eq!(meta.mime, "text/plain");
    assert_eq!(meta.bytes, data.len() as u64);

    // Inclusive range fetch (bytes 4-8 = "quick").
    assert_eq!(
        store.get(&hash, Some((4, 8))).await.unwrap().as_deref(),
        Some(&b"quick"[..])
    );
    // Range past the end clamps, never panics.
    assert_eq!(
        store.get(&hash, Some((16, 999))).await.unwrap().as_deref(),
        Some(&b"fox"[..])
    );

    // Absent blob.
    let absent = blob_hash(b"never stored");
    assert_eq!(store.get(&absent, None).await.unwrap(), None);
    assert!(!store.exists(&absent).await.unwrap());

    // Delete (GC) removes the bytes; idempotent.
    store.delete(&hash).await.unwrap();
    assert!(!store.exists(&hash).await.unwrap());
    assert_eq!(store.get(&hash, None).await.unwrap(), None);
    store.delete(&hash).await.unwrap(); // no-op on absent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mem_blob_store_satisfies_contract() {
        blob_store_contract(&MemBlobStore::default()).await;
    }

    #[test]
    fn blob_hash_parse_validates() {
        assert!(BlobHash::parse("abc").is_none());
        assert!(BlobHash::parse(&"g".repeat(64)).is_none()); // non-hex
        let hex = blob_hash(b"x").to_string();
        assert_eq!(BlobHash::parse(&hex).unwrap().as_str(), hex);
    }
}
