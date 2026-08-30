use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::{DecisionRecord, RecordStore, shared_state::RedisSharedState};

#[derive(Debug, Clone)]
pub(crate) enum RecordDelete {
    Decisions(Vec<String>),
    Task {
        decision_ids: Vec<String>,
        task_key: String,
        before_generation: u64,
    },
    Tenant {
        decision_ids: Vec<String>,
        before_generation: u64,
    },
}

impl RecordDelete {
    fn decision_ids(&self) -> &[String] {
        match self {
            Self::Decisions(ids)
            | Self::Task {
                decision_ids: ids, ..
            }
            | Self::Tenant {
                decision_ids: ids, ..
            } => ids,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum RecordRepositoryError {
    #[error("local record backend failed: {0}")]
    Local(String),
    #[error(transparent)]
    Shared(#[from] crate::shared_state::SharedStateError),
}

#[async_trait]
pub(crate) trait DecisionRecordRepository: Send + Sync {
    async fn put(&self, record: DecisionRecord) -> Result<(), RecordRepositoryError>;
    async fn list(&self, tenant_key: &str) -> Result<Vec<DecisionRecord>, RecordRepositoryError>;
    async fn get(
        &self,
        tenant_key: &str,
        decision_id: &str,
    ) -> Result<Option<DecisionRecord>, RecordRepositoryError>;
    async fn delete(
        &self,
        tenant_key: &str,
        request: RecordDelete,
    ) -> Result<usize, RecordRepositoryError>;
    fn backend_name(&self) -> &'static str;
}

pub(crate) struct MemoryDecisionRecordRepository {
    local: RecordStore,
}

impl MemoryDecisionRecordRepository {
    pub(crate) fn new(local: RecordStore) -> Arc<Self> {
        Arc::new(Self { local })
    }
}

#[async_trait]
impl DecisionRecordRepository for MemoryDecisionRecordRepository {
    async fn put(&self, record: DecisionRecord) -> Result<(), RecordRepositoryError> {
        self.local.append(record).await;
        Ok(())
    }

    async fn list(&self, tenant_key: &str) -> Result<Vec<DecisionRecord>, RecordRepositoryError> {
        self.local.prune_expired().await.map_err(local_error)?;
        Ok(self
            .local
            .records
            .read()
            .await
            .iter()
            .filter(|record| record.tenant_key == tenant_key)
            .cloned()
            .collect())
    }

    async fn get(
        &self,
        tenant_key: &str,
        decision_id: &str,
    ) -> Result<Option<DecisionRecord>, RecordRepositoryError> {
        self.local.prune_expired().await.map_err(local_error)?;
        Ok(self
            .local
            .records
            .read()
            .await
            .iter()
            .find(|record| record.tenant_key == tenant_key && record.decision_id == decision_id)
            .cloned())
    }

    async fn delete(
        &self,
        tenant_key: &str,
        request: RecordDelete,
    ) -> Result<usize, RecordRepositoryError> {
        delete_local(&self.local, tenant_key, &request).await
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }
}

pub(crate) struct RedisDecisionRecordRepository {
    shared: RedisSharedState,
    local: RecordStore,
    capacity: usize,
}

impl RedisDecisionRecordRepository {
    pub(crate) fn new(shared: RedisSharedState, local: RecordStore, capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            shared,
            local,
            capacity,
        })
    }
}

#[async_trait]
impl DecisionRecordRepository for RedisDecisionRecordRepository {
    async fn put(&self, record: DecisionRecord) -> Result<(), RecordRepositoryError> {
        self.shared.put_record(&record, self.capacity).await?;
        self.local.append(record).await;
        Ok(())
    }

    async fn list(&self, tenant_key: &str) -> Result<Vec<DecisionRecord>, RecordRepositoryError> {
        self.shared
            .records(tenant_key)
            .await
            .map_err(RecordRepositoryError::from)
    }

    async fn get(
        &self,
        tenant_key: &str,
        decision_id: &str,
    ) -> Result<Option<DecisionRecord>, RecordRepositoryError> {
        self.shared
            .record(tenant_key, decision_id)
            .await
            .map_err(RecordRepositoryError::from)
    }

    async fn delete(
        &self,
        tenant_key: &str,
        request: RecordDelete,
    ) -> Result<usize, RecordRepositoryError> {
        let shared_deleted = self
            .shared
            .delete_records(tenant_key, request.decision_ids())
            .await?;
        let local_deleted = delete_local(&self.local, tenant_key, &request).await?;
        Ok(shared_deleted.max(local_deleted))
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }
}

async fn delete_local(
    local: &RecordStore,
    tenant_key: &str,
    request: &RecordDelete,
) -> Result<usize, RecordRepositoryError> {
    local
        .delete_matching(|record| match request {
            RecordDelete::Decisions(ids) => {
                record.tenant_key == tenant_key && ids.contains(&record.decision_id)
            }
            RecordDelete::Task {
                task_key,
                before_generation,
                ..
            } => {
                record.tenant_key == tenant_key
                    && record.task_key.as_ref() == Some(task_key)
                    && record.task_generation < *before_generation
            }
            RecordDelete::Tenant {
                before_generation, ..
            } => record.tenant_key == tenant_key && record.tenant_generation < *before_generation,
        })
        .await
        .map_err(local_error)
}

fn local_error(error: crate::GatewayError) -> RecordRepositoryError {
    RecordRepositoryError::Local(error.message)
}
