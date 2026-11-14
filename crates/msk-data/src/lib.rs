//! Shared objects and views. M0 provides an in-memory content-addressed store;
//! the kernel-side object registry will add chunking, labels, and TTLs.

use std::collections::HashMap;
use std::sync::Mutex;

use msk_core::{DataLabel, ObjectRef};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub trait ObjectStore: Send + Sync {
    fn put(&self, bytes: &[u8], label: DataLabel) -> Result<ObjectRef, DataError>;
    fn get(&self, id: &str) -> Result<Vec<u8>, DataError>;
    fn stat(&self, id: &str) -> Result<ObjectRef, DataError>;
}

struct Stored {
    meta: ObjectRef,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub struct MemoryObjectStore {
    inner: Mutex<HashMap<String, Stored>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ObjectStore for MemoryObjectStore {
    fn put(&self, bytes: &[u8], label: DataLabel) -> Result<ObjectRef, DataError> {
        let id = Uuid::new_v4().to_string();
        let meta = ObjectRef {
            id: id.clone(),
            kind: "blob".into(),
            size: Some(bytes.len() as u64),
            label,
            hash: Some(sha256_hex(bytes)),
            generation: 0,
        };
        self.inner.lock().expect("object store poisoned").insert(
            id,
            Stored {
                meta: meta.clone(),
                bytes: bytes.to_vec(),
            },
        );
        Ok(meta)
    }

    fn get(&self, id: &str) -> Result<Vec<u8>, DataError> {
        self.inner
            .lock()
            .expect("object store poisoned")
            .get(id)
            .map(|stored| stored.bytes.clone())
            .ok_or_else(|| DataError::NotFound(id.into()))
    }

    fn stat(&self, id: &str) -> Result<ObjectRef, DataError> {
        self.inner
            .lock()
            .expect("object store poisoned")
            .get(id)
            .map(|stored| stored.meta.clone())
            .ok_or_else(|| DataError::NotFound(id.into()))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_stat() {
        let store = MemoryObjectStore::new();
        let meta = store.put(b"hello", DataLabel::Challenge).unwrap();
        assert_eq!(meta.size, Some(5));
        assert_eq!(meta.label, DataLabel::Challenge);
        assert_eq!(store.get(&meta.id).unwrap(), b"hello");
        assert_eq!(store.stat(&meta.id).unwrap().hash, meta.hash);
        assert!(store.get("missing").is_err());
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
