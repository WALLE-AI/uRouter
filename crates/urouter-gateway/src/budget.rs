use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use redis::{Script, aio::ConnectionManager};
use sha2::{Digest, Sha256};
use thiserror::Error;

const RESERVE: &str = r"
local existing = redis.call('HGET', KEYS[2], ARGV[1])
if existing then return {2, existing} end
local total = tonumber(redis.call('GET', KEYS[1]) or '0')
if tonumber(ARGV[3]) > 0 and total + tonumber(ARGV[2]) > tonumber(ARGV[3]) then
  return {0, tostring(total)}
end
redis.call('HSET', KEYS[2], ARGV[1], ARGV[2])
redis.call('INCRBY', KEYS[1], ARGV[2])
redis.call('EXPIRE', KEYS[1], ARGV[4])
redis.call('EXPIRE', KEYS[2], ARGV[4])
return {1, ARGV[2]}
";

const SETTLE: &str = r"
local previous = redis.call('HGET', KEYS[2], ARGV[1])
if not previous then return 0 end
redis.call('INCRBY', KEYS[1], tonumber(ARGV[2]) - tonumber(previous))
redis.call('HSET', KEYS[2], ARGV[1], ARGV[2])
return 1
";

const RELEASE: &str = r"
local previous = redis.call('HGET', KEYS[2], ARGV[1])
if not previous then return 0 end
redis.call('HDEL', KEYS[2], ARGV[1])
redis.call('DECRBY', KEYS[1], previous)
return 1
";

#[derive(Debug, Clone)]
pub(crate) struct BudgetPermit {
    tenant_key: String,
    request_id: String,
    period: u64,
}

pub(crate) enum BudgetReservation {
    Granted(BudgetPermit),
    Rejected,
}

#[derive(Debug, Error)]
pub(crate) enum BudgetError {
    #[error("budget state lock is poisoned")]
    LockPoisoned,
    #[error("budget state backend failed: {0}")]
    Backend(#[from] redis::RedisError),
}

#[async_trait]
pub(crate) trait BudgetRepository: Send + Sync {
    async fn reserve(
        &self,
        tenant_key: &str,
        request_id: &str,
        estimated_nano_usd: u64,
    ) -> Result<BudgetReservation, BudgetError>;
    async fn settle(&self, permit: &BudgetPermit, actual_nano_usd: u64) -> Result<(), BudgetError>;
    async fn release(&self, permit: BudgetPermit) -> Result<(), BudgetError>;
    fn backend_name(&self) -> &'static str;
    fn limit_nano_usd(&self) -> u64;
}

pub(crate) enum BudgetAdmission {
    Granted(BudgetLease),
    Rejected,
}

pub(crate) struct BudgetLease {
    repository: Arc<dyn BudgetRepository>,
    permit: Option<BudgetPermit>,
}

impl BudgetLease {
    pub(crate) async fn acquire(
        repository: Arc<dyn BudgetRepository>,
        tenant_key: &str,
        request_id: &str,
        estimated_nano_usd: u64,
    ) -> Result<BudgetAdmission, BudgetError> {
        match repository
            .reserve(tenant_key, request_id, estimated_nano_usd)
            .await?
        {
            BudgetReservation::Granted(permit) => Ok(BudgetAdmission::Granted(Self {
                repository,
                permit: Some(permit),
            })),
            BudgetReservation::Rejected => Ok(BudgetAdmission::Rejected),
        }
    }

    pub(crate) async fn settle(&self, actual_nano_usd: u64) -> Result<(), BudgetError> {
        if let Some(permit) = &self.permit {
            self.repository.settle(permit, actual_nano_usd).await?;
        }
        Ok(())
    }

    pub(crate) async fn release(mut self) -> Result<(), BudgetError> {
        if let Some(permit) = self.permit.take() {
            self.repository.release(permit).await?;
        }
        Ok(())
    }
}

fn current_period(period_seconds: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / period_seconds.max(1)
}

#[derive(Default)]
struct MemoryBudgetState {
    totals: BTreeMap<(String, u64), u64>,
    reservations: BTreeMap<(String, u64, String), u64>,
}

pub(crate) struct MemoryBudgetRepository {
    limit_nano_usd: u64,
    period_seconds: u64,
    state: Mutex<MemoryBudgetState>,
}

impl MemoryBudgetRepository {
    pub(crate) fn new(limit_nano_usd: u64, period_seconds: u64) -> Arc<Self> {
        Arc::new(Self {
            limit_nano_usd,
            period_seconds: period_seconds.max(1),
            state: Mutex::new(MemoryBudgetState::default()),
        })
    }
}

#[async_trait]
impl BudgetRepository for MemoryBudgetRepository {
    async fn reserve(
        &self,
        tenant_key: &str,
        request_id: &str,
        estimated_nano_usd: u64,
    ) -> Result<BudgetReservation, BudgetError> {
        let period = current_period(self.period_seconds);
        if self.limit_nano_usd == 0 {
            return Ok(BudgetReservation::Granted(BudgetPermit {
                tenant_key: tenant_key.to_owned(),
                request_id: request_id.to_owned(),
                period,
            }));
        }
        let mut state = self.state.lock().map_err(|_| BudgetError::LockPoisoned)?;
        let reservation_key = (tenant_key.to_owned(), period, request_id.to_owned());
        if state.reservations.contains_key(&reservation_key) {
            return Ok(BudgetReservation::Granted(BudgetPermit {
                tenant_key: tenant_key.to_owned(),
                request_id: request_id.to_owned(),
                period,
            }));
        }
        let total_key = (tenant_key.to_owned(), period);
        let total = state.totals.get(&total_key).copied().unwrap_or(0);
        if self.limit_nano_usd > 0 && total.saturating_add(estimated_nano_usd) > self.limit_nano_usd
        {
            return Ok(BudgetReservation::Rejected);
        }
        state
            .reservations
            .insert(reservation_key, estimated_nano_usd);
        state
            .totals
            .insert(total_key, total.saturating_add(estimated_nano_usd));
        Ok(BudgetReservation::Granted(BudgetPermit {
            tenant_key: tenant_key.to_owned(),
            request_id: request_id.to_owned(),
            period,
        }))
    }

    async fn settle(&self, permit: &BudgetPermit, actual_nano_usd: u64) -> Result<(), BudgetError> {
        if self.limit_nano_usd == 0 {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| BudgetError::LockPoisoned)?;
        let reservation_key = (
            permit.tenant_key.clone(),
            permit.period,
            permit.request_id.clone(),
        );
        let Some(previous) = state.reservations.insert(reservation_key, actual_nano_usd) else {
            return Ok(());
        };
        let total = state
            .totals
            .entry((permit.tenant_key.clone(), permit.period))
            .or_default();
        *total = total
            .saturating_sub(previous)
            .saturating_add(actual_nano_usd);
        Ok(())
    }

    async fn release(&self, permit: BudgetPermit) -> Result<(), BudgetError> {
        if self.limit_nano_usd == 0 {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| BudgetError::LockPoisoned)?;
        let reservation_key = (permit.tenant_key.clone(), permit.period, permit.request_id);
        let Some(previous) = state.reservations.remove(&reservation_key) else {
            return Ok(());
        };
        let total_key = (permit.tenant_key, permit.period);
        if let Some(total) = state.totals.get_mut(&total_key) {
            *total = total.saturating_sub(previous);
            if *total == 0 {
                state.totals.remove(&total_key);
            }
        }
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }
    fn limit_nano_usd(&self) -> u64 {
        self.limit_nano_usd
    }
}

pub(crate) struct RedisBudgetRepository {
    connection: ConnectionManager,
    prefix: String,
    limit_nano_usd: u64,
    period_seconds: u64,
}

impl RedisBudgetRepository {
    pub(crate) async fn connect(
        url: &str,
        prefix: String,
        limit_nano_usd: u64,
        period_seconds: u64,
    ) -> Result<Arc<Self>, BudgetError> {
        let client = redis::Client::open(url)?;
        Ok(Arc::new(Self {
            connection: ConnectionManager::new(client).await?,
            prefix,
            limit_nano_usd,
            period_seconds: period_seconds.max(1),
        }))
    }

    fn keys(&self, tenant_key: &str, period: u64) -> [String; 2] {
        let digest = format!("{:x}", Sha256::digest(tenant_key.as_bytes()));
        [
            format!("{}:budget:{period}:total:{digest}", self.prefix),
            format!("{}:budget:{period}:reservations:{digest}", self.prefix),
        ]
    }
}

#[async_trait]
impl BudgetRepository for RedisBudgetRepository {
    async fn reserve(
        &self,
        tenant_key: &str,
        request_id: &str,
        estimated_nano_usd: u64,
    ) -> Result<BudgetReservation, BudgetError> {
        let period = current_period(self.period_seconds);
        if self.limit_nano_usd == 0 {
            return Ok(BudgetReservation::Granted(BudgetPermit {
                tenant_key: tenant_key.to_owned(),
                request_id: request_id.to_owned(),
                period,
            }));
        }
        let keys = self.keys(tenant_key, period);
        let mut connection = self.connection.clone();
        let (status, _): (i64, String) = Script::new(RESERVE)
            .key(&keys[0])
            .key(&keys[1])
            .arg(request_id)
            .arg(i64::try_from(estimated_nano_usd).unwrap_or(i64::MAX))
            .arg(i64::try_from(self.limit_nano_usd).unwrap_or(i64::MAX))
            .arg(self.period_seconds.saturating_mul(2))
            .invoke_async(&mut connection)
            .await?;
        if status == 0 {
            return Ok(BudgetReservation::Rejected);
        }
        Ok(BudgetReservation::Granted(BudgetPermit {
            tenant_key: tenant_key.to_owned(),
            request_id: request_id.to_owned(),
            period,
        }))
    }

    async fn settle(&self, permit: &BudgetPermit, actual_nano_usd: u64) -> Result<(), BudgetError> {
        if self.limit_nano_usd == 0 {
            return Ok(());
        }
        let keys = self.keys(&permit.tenant_key, permit.period);
        let mut connection = self.connection.clone();
        Script::new(SETTLE)
            .key(&keys[0])
            .key(&keys[1])
            .arg(&permit.request_id)
            .arg(i64::try_from(actual_nano_usd).unwrap_or(i64::MAX))
            .invoke_async::<i64>(&mut connection)
            .await?;
        Ok(())
    }

    async fn release(&self, permit: BudgetPermit) -> Result<(), BudgetError> {
        if self.limit_nano_usd == 0 {
            return Ok(());
        }
        let keys = self.keys(&permit.tenant_key, permit.period);
        let mut connection = self.connection.clone();
        Script::new(RELEASE)
            .key(&keys[0])
            .key(&keys[1])
            .arg(permit.request_id)
            .invoke_async::<i64>(&mut connection)
            .await?;
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }
    fn limit_nano_usd(&self) -> u64 {
        self.limit_nano_usd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reserve_settle_release_and_deduplicate() {
        let repository: Arc<dyn BudgetRepository> = MemoryBudgetRepository::new(100, 3600);
        let first = match repository.reserve("tenant", "request", 80).await.unwrap() {
            BudgetReservation::Granted(permit) => permit,
            BudgetReservation::Rejected => panic!("first reservation rejected"),
        };
        assert!(matches!(
            repository.reserve("tenant", "other", 21).await.unwrap(),
            BudgetReservation::Rejected
        ));
        let duplicate = match repository.reserve("tenant", "request", 80).await.unwrap() {
            BudgetReservation::Granted(permit) => permit,
            BudgetReservation::Rejected => panic!("duplicate reservation rejected"),
        };
        repository.settle(&first, 40).await.unwrap();
        assert!(matches!(
            repository.reserve("tenant", "other", 60).await.unwrap(),
            BudgetReservation::Granted(_)
        ));
        repository.release(duplicate).await.unwrap();
    }

    #[tokio::test]
    async fn unlimited_budget_does_not_retain_memory_state() {
        let repository = MemoryBudgetRepository::new(0, 3600);
        let permit = match repository.reserve("tenant", "request", 80).await.unwrap() {
            BudgetReservation::Granted(permit) => permit,
            BudgetReservation::Rejected => panic!("unlimited budget rejected"),
        };
        repository.settle(&permit, 90).await.unwrap();
        repository.release(permit).await.unwrap();
        let state = repository.state.lock().unwrap();
        assert!(state.totals.is_empty());
        assert!(state.reservations.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
    async fn redis_budget_is_shared_atomic_and_idempotent() {
        let url = std::env::var("UROUTER_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16380/".to_owned());
        let prefix = format!("urouter-budget-contract-{}", std::process::id());
        let first: Arc<dyn BudgetRepository> =
            RedisBudgetRepository::connect(&url, prefix.clone(), 100, 3600)
                .await
                .unwrap();
        let second: Arc<dyn BudgetRepository> =
            RedisBudgetRepository::connect(&url, prefix, 100, 3600)
                .await
                .unwrap();
        let permit = match first.reserve("tenant", "request", 80).await.unwrap() {
            BudgetReservation::Granted(permit) => permit,
            BudgetReservation::Rejected => panic!("first reservation rejected"),
        };
        assert!(matches!(
            second.reserve("tenant", "other", 21).await.unwrap(),
            BudgetReservation::Rejected
        ));
        assert!(matches!(
            second.reserve("tenant", "request", 80).await.unwrap(),
            BudgetReservation::Granted(_)
        ));
        first.settle(&permit, 40).await.unwrap();
        assert!(matches!(
            second.reserve("tenant", "other", 60).await.unwrap(),
            BudgetReservation::Granted(_)
        ));
    }
}
