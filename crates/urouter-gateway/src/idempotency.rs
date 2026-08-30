use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::shared_state::{RedisSharedState, SharedStateError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdempotencyClaim {
    Created(String),
    Reused(String),
    Conflict,
}

#[async_trait]
pub(crate) trait IdempotencyRepository: Send + Sync {
    async fn claim(
        &self,
        tenant_key: &str,
        key_hash: &str,
        request_hash: &str,
        candidate_request_id: &str,
        ttl_seconds: u64,
    ) -> Result<IdempotencyClaim, SharedStateError>;

    fn backend_name(&self) -> &'static str;
}

#[derive(Debug, Clone)]
struct IdempotencyEntry {
    request_hash: String,
    request_id: String,
    expires_at_unix_s: u64,
}

pub(crate) struct MemoryIdempotencyRepository {
    entries: RwLock<BTreeMap<String, IdempotencyEntry>>,
    capacity: usize,
}

impl MemoryIdempotencyRepository {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(BTreeMap::new()),
            capacity,
        })
    }
}

#[async_trait]
impl IdempotencyRepository for MemoryIdempotencyRepository {
    async fn claim(
        &self,
        tenant_key: &str,
        key_hash: &str,
        request_hash: &str,
        candidate_request_id: &str,
        ttl_seconds: u64,
    ) -> Result<IdempotencyClaim, SharedStateError> {
        let now = unix_seconds();
        let storage_key = scoped_key(tenant_key, key_hash);
        let mut entries = self.entries.write().await;
        entries.retain(|_, entry| entry.expires_at_unix_s > now);
        if let Some(existing) = entries.get_mut(&storage_key) {
            if existing.request_hash != request_hash {
                return Ok(IdempotencyClaim::Conflict);
            }
            existing.expires_at_unix_s = now.saturating_add(ttl_seconds);
            return Ok(IdempotencyClaim::Reused(existing.request_id.clone()));
        }
        if entries.len() >= self.capacity
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at_unix_s)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            storage_key,
            IdempotencyEntry {
                request_hash: request_hash.to_owned(),
                request_id: candidate_request_id.to_owned(),
                expires_at_unix_s: now.saturating_add(ttl_seconds),
            },
        );
        Ok(IdempotencyClaim::Created(candidate_request_id.to_owned()))
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }
}

pub(crate) struct RedisIdempotencyRepository {
    state: RedisSharedState,
}

impl RedisIdempotencyRepository {
    pub(crate) fn new(state: RedisSharedState) -> Arc<Self> {
        Arc::new(Self { state })
    }
}

#[async_trait]
impl IdempotencyRepository for RedisIdempotencyRepository {
    async fn claim(
        &self,
        tenant_key: &str,
        key_hash: &str,
        request_hash: &str,
        candidate_request_id: &str,
        ttl_seconds: u64,
    ) -> Result<IdempotencyClaim, SharedStateError> {
        self.state
            .claim_idempotency(
                tenant_key,
                key_hash,
                request_hash,
                candidate_request_id,
                ttl_seconds,
            )
            .await
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }
}

fn scoped_key(tenant_key: &str, key_hash: &str) -> String {
    format!("{tenant_key}\0{key_hash}")
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn contract(repository: &dyn IdempotencyRepository) {
        assert_eq!(
            repository
                .claim("tenant-a", "key-hash", "request-a", "req-1", 60)
                .await
                .unwrap(),
            IdempotencyClaim::Created("req-1".to_owned())
        );
        assert_eq!(
            repository
                .claim("tenant-a", "key-hash", "request-a", "req-2", 60)
                .await
                .unwrap(),
            IdempotencyClaim::Reused("req-1".to_owned())
        );
        assert_eq!(
            repository
                .claim("tenant-a", "key-hash", "request-b", "req-3", 60)
                .await
                .unwrap(),
            IdempotencyClaim::Conflict
        );
        assert_eq!(
            repository
                .claim("tenant-b", "key-hash", "request-a", "req-4", 60)
                .await
                .unwrap(),
            IdempotencyClaim::Created("req-4".to_owned())
        );
    }

    #[tokio::test]
    async fn memory_repository_passes_contract() {
        let repository = MemoryIdempotencyRepository::new(10);
        contract(repository.as_ref()).await;
    }
}
