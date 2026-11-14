//! Shared objects and views. M0 provides an in-memory content-addressed store;
//! the kernel-side object registry will add chunking, labels, and TTLs.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use msk_core::{DataLabel, ObjectRef};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("invalid object id: {0}")]
    InvalidId(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub trait ObjectStore: Send + Sync {
    fn put(&self, bytes: &[u8], label: DataLabel) -> Result<ObjectRef, DataError>;
    fn get(&self, id: &str) -> Result<Vec<u8>, DataError>;
    fn stat(&self, id: &str) -> Result<ObjectRef, DataError>;
}

/// Content-addressed on-disk store. Object IDs are SHA-256 hex strings, so
/// identical outputs deduplicate and survive kernel restarts.
pub struct FileObjectStore {
    root: PathBuf,
}

impl FileObjectStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, DataError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("objects"))?;
        fs::create_dir_all(root.join("meta"))?;
        Ok(Self { root })
    }

    fn object_path(&self, id: &str) -> Result<PathBuf, DataError> {
        validate_id(id)?;
        Ok(self.root.join("objects").join(&id[..2]).join(id))
    }

    fn meta_path(&self, id: &str) -> Result<PathBuf, DataError> {
        validate_id(id)?;
        Ok(self.root.join("meta").join(format!("{id}.json")))
    }
}

impl ObjectStore for FileObjectStore {
    fn put(&self, bytes: &[u8], label: DataLabel) -> Result<ObjectRef, DataError> {
        let hash = sha256_hex(bytes);
        let reference = ObjectRef {
            id: hash.clone(),
            kind: "blob".into(),
            size: Some(bytes.len() as u64),
            label,
            hash: Some(hash.clone()),
            generation: 0,
        };
        write_atomic(&self.object_path(&hash)?, bytes)?;
        write_atomic(
            &self.meta_path(&hash)?,
            &serde_json::to_vec_pretty(&reference)?,
        )?;
        Ok(reference)
    }

    fn get(&self, id: &str) -> Result<Vec<u8>, DataError> {
        match fs::read(self.object_path(id)?) {
            Ok(bytes) => Ok(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(DataError::NotFound(id.into()))
            }
            Err(err) => Err(err.into()),
        }
    }

    fn stat(&self, id: &str) -> Result<ObjectRef, DataError> {
        match fs::read(self.meta_path(id)?) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(DataError::NotFound(id.into()))
            }
            Err(err) => Err(err.into()),
        }
    }
}

fn validate_id(id: &str) -> Result<(), DataError> {
    if id.len() != 64 || !id.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(DataError::InvalidId(id.into()));
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DataError> {
    let parent = path
        .parent()
        .ok_or_else(|| DataError::InvalidId(path.display().to_string()))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".tmp-{}", Uuid::new_v4()));
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)?;
    Ok(())
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

    #[test]
    fn file_store_persists_objects() {
        let root = std::env::temp_dir().join(format!("msk-data-{}", Uuid::new_v4()));
        let store = FileObjectStore::open(&root).unwrap();
        let meta = store.put(b"hello", DataLabel::Derived).unwrap();
        assert_eq!(store.get(&meta.id).unwrap(), b"hello");

        drop(store);
        let reopened = FileObjectStore::open(&root).unwrap();
        assert_eq!(reopened.get(&meta.id).unwrap(), b"hello");
        assert_eq!(reopened.stat(&meta.id).unwrap().label, DataLabel::Derived);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_store_handles_binary_and_rejects_bad_ids() {
        let root = std::env::temp_dir().join(format!("msk-data-{}", Uuid::new_v4()));
        let store = FileObjectStore::open(&root).unwrap();
        let binary = [0u8, 159, 146, 150];
        let meta = store.put(&binary, DataLabel::Challenge).unwrap();
        assert_eq!(store.get(&meta.id).unwrap(), binary);
        assert!(matches!(store.get(".."), Err(DataError::InvalidId(_))));
        let _ = fs::remove_dir_all(root);
    }
}
