use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::Mutex,
};
use urouter_contracts::VectorRef;

const FLOAT_BYTES: u64 = 4;
const FLOAT_BYTES_USIZE: usize = 4;

#[derive(Debug)]
struct WriterState {
    day: u64,
    shard_index: u32,
}

#[derive(Debug)]
pub struct VectorSideStore {
    root: PathBuf,
    maximum_shard_bytes: u64,
    writer: Mutex<WriterState>,
    tombstones: Mutex<HashSet<String>>,
}

impl VectorSideStore {
    pub async fn open(
        root: impl Into<PathBuf>,
        maximum_shard_bytes: u64,
    ) -> Result<Arc<Self>, VectorStoreError> {
        if maximum_shard_bytes < FLOAT_BYTES {
            return Err(VectorStoreError::InvalidShardLimit);
        }
        let root = root.into();
        fs::create_dir_all(&root).await?;
        let tombstones = match fs::read_to_string(root.join("tombstones.jsonl")).await {
            Ok(contents) => contents
                .lines()
                .filter_map(|line| serde_json::from_str::<String>(line).ok())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Arc::new(Self {
            root,
            maximum_shard_bytes,
            writer: Mutex::new(WriterState {
                day: unix_day(),
                shard_index: 0,
            }),
            tombstones: Mutex::new(tombstones),
        }))
    }

    pub async fn append(&self, vector: &[f32]) -> Result<VectorRef, VectorStoreError> {
        if vector.is_empty() {
            return Err(VectorStoreError::EmptyVector);
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(VectorStoreError::NonFiniteVector);
        }
        let dimensions = u32::try_from(vector.len()).map_err(|_| VectorStoreError::TooLarge)?;
        let bytes = vector
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        let byte_len = u64::try_from(bytes.len()).map_err(|_| VectorStoreError::TooLarge)?;
        if byte_len > self.maximum_shard_bytes {
            return Err(VectorStoreError::TooLarge);
        }

        let mut writer = self.writer.lock().await;
        let day = unix_day();
        if writer.day != day {
            writer.day = day;
            writer.shard_index = 0;
        }
        let (relative, path, offset) = loop {
            let relative = format!("{day:08}/{:05}.f32", writer.shard_index);
            let path = self.root.join(&relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            let offset = fs::metadata(&path)
                .await
                .map_or(0, |metadata| metadata.len());
            if offset.saturating_add(byte_len) <= self.maximum_shard_bytes {
                break (relative, path, offset);
            }
            writer.shard_index = writer
                .shard_index
                .checked_add(1)
                .ok_or(VectorStoreError::TooLarge)?;
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok(VectorRef {
            shard: relative.replace('\\', "/"),
            offset_bytes: offset,
            dimensions,
            sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
        })
    }

    pub async fn read(&self, reference: &VectorRef) -> Result<Vec<f32>, VectorStoreError> {
        validate_relative_shard(&reference.shard)?;
        if self
            .tombstones
            .lock()
            .await
            .contains(&reference_key(reference))
        {
            return Err(VectorStoreError::Tombstoned);
        }
        if reference.dimensions == 0 || !reference.offset_bytes.is_multiple_of(FLOAT_BYTES) {
            return Err(VectorStoreError::InvalidReference);
        }
        let byte_len = u64::from(reference.dimensions)
            .checked_mul(FLOAT_BYTES)
            .ok_or(VectorStoreError::TooLarge)?;
        let mut bytes =
            vec![0_u8; usize::try_from(byte_len).map_err(|_| VectorStoreError::TooLarge)?];
        let mut file = OpenOptions::new()
            .read(true)
            .open(self.root.join(&reference.shard))
            .await?;
        file.seek(std::io::SeekFrom::Start(reference.offset_bytes))
            .await?;
        file.read_exact(&mut bytes).await?;
        let actual_hash = format!("sha256:{:x}", Sha256::digest(&bytes));
        if actual_hash != reference.sha256 {
            return Err(VectorStoreError::HashMismatch);
        }
        Ok(bytes
            .chunks_exact(FLOAT_BYTES_USIZE)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect())
    }

    pub async fn tombstone(&self, reference: &VectorRef) -> Result<bool, VectorStoreError> {
        validate_relative_shard(&reference.shard)?;
        let key = reference_key(reference);
        let mut tombstones = self.tombstones.lock().await;
        if tombstones.contains(&key) {
            return Ok(false);
        }
        let encoded = serde_json::to_vec(&key).map_err(VectorStoreError::Serialize)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("tombstones.jsonl"))
            .await?;
        file.write_all(&encoded).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        tombstones.insert(key);
        Ok(true)
    }
}

fn reference_key(reference: &VectorRef) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        reference.shard, reference.offset_bytes, reference.dimensions, reference.sha256
    )
}

fn unix_day() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400
}

fn validate_relative_shard(shard: &str) -> Result<(), VectorStoreError> {
    let path = Path::new(shard);
    if shard.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
        })
    {
        return Err(VectorStoreError::InvalidReference);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum VectorStoreError {
    #[error("vector shard size must be at least four bytes")]
    InvalidShardLimit,
    #[error("vector must not be empty")]
    EmptyVector,
    #[error("vector contains a non-finite value")]
    NonFiniteVector,
    #[error("vector or shard index is too large")]
    TooLarge,
    #[error("vector reference is invalid")]
    InvalidReference,
    #[error("vector content hash does not match its reference")]
    HashMismatch,
    #[error("vector reference has been deleted")]
    Tombstoned,
    #[error("failed to encode vector tombstone")]
    Serialize(serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "urouter-vector-store-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn append_read_and_rotation_preserve_exact_float_bits() {
        let root = test_root();
        let store = VectorSideStore::open(&root, 16).await.unwrap();
        let first = store.append(&[1.0, -2.5]).await.unwrap();
        let second = store.append(&[3.25, 4.5, 5.75]).await.unwrap();
        assert_eq!(store.read(&first).await.unwrap(), vec![1.0, -2.5]);
        assert_eq!(store.read(&second).await.unwrap(), vec![3.25, 4.5, 5.75]);
        assert_ne!(first.shard, second.shard);
        fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_nonfinite_vectors_and_path_traversal() {
        let root = test_root();
        let store = VectorSideStore::open(&root, 64).await.unwrap();
        assert!(matches!(
            store.append(&[f32::NAN]).await,
            Err(VectorStoreError::NonFiniteVector)
        ));
        let invalid = VectorRef {
            shard: "../secret.f32".to_owned(),
            offset_bytes: 0,
            dimensions: 1,
            sha256: "sha256:invalid".to_owned(),
        };
        assert!(matches!(
            store.read(&invalid).await,
            Err(VectorStoreError::InvalidReference)
        ));
        fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn tombstone_is_idempotent_and_survives_restart() {
        let root = test_root();
        let store = VectorSideStore::open(&root, 64).await.unwrap();
        let reference = store.append(&[1.0, 2.0]).await.unwrap();
        assert!(store.tombstone(&reference).await.unwrap());
        assert!(!store.tombstone(&reference).await.unwrap());
        assert!(matches!(
            store.read(&reference).await,
            Err(VectorStoreError::Tombstoned)
        ));
        drop(store);

        let reopened = VectorSideStore::open(&root, 64).await.unwrap();
        assert!(matches!(
            reopened.read(&reference).await,
            Err(VectorStoreError::Tombstoned)
        ));
        fs::remove_dir_all(root).await.unwrap();
    }
}
