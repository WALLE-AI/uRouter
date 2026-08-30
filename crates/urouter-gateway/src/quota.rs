use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use redis::{Script, aio::ConnectionManager};
use sha2::{Digest, Sha256};
use thiserror::Error;

const REQUEST_WINDOW_MS: u64 = 60_000;
const RESERVE_PERMIT: &str = r"
local time = redis.call('TIME')
local now = tonumber(time[1]) * 1000 + math.floor(tonumber(time[2]) / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now)
redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now - tonumber(ARGV[4]))
local expired = redis.call('ZRANGEBYSCORE', KEYS[4], '-inf', now - tonumber(ARGV[4]))
for _, member in ipairs(expired) do
  local amount = tonumber(redis.call('HGET', KEYS[5], member) or '0')
  if amount > 0 then redis.call('DECRBY', KEYS[6], amount) end
  redis.call('HDEL', KEYS[5], member)
end
redis.call('ZREMRANGEBYSCORE', KEYS[4], '-inf', now - tonumber(ARGV[4]))
local current_tokens = tonumber(redis.call('GET', KEYS[6]) or '0')
if tonumber(ARGV[1]) > 0 and redis.call('ZCARD', KEYS[1]) >= tonumber(ARGV[1]) then
  return {0, 'in_flight'}
end
if tonumber(ARGV[3]) > 0 and redis.call('ZCARD', KEYS[2]) >= tonumber(ARGV[3]) then
  return {0, 'requests_per_minute'}
end
if tonumber(ARGV[5]) > 0 and current_tokens + tonumber(ARGV[6]) > tonumber(ARGV[5]) then
  return {0, 'tokens_per_minute'}
end
local token = tostring(redis.call('INCR', KEYS[3]))
if tonumber(ARGV[1]) > 0 then
  redis.call('ZADD', KEYS[1], now + tonumber(ARGV[2]), token)
  redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[2]) * 2)
end
if tonumber(ARGV[3]) > 0 then
  redis.call('ZADD', KEYS[2], now, token)
  redis.call('PEXPIRE', KEYS[2], tonumber(ARGV[4]) * 2)
end
if tonumber(ARGV[5]) > 0 then
  redis.call('ZADD', KEYS[4], now, token)
  redis.call('HSET', KEYS[5], token, ARGV[6])
  redis.call('INCRBY', KEYS[6], ARGV[6])
  redis.call('PEXPIRE', KEYS[4], tonumber(ARGV[4]) * 2)
  redis.call('PEXPIRE', KEYS[5], tonumber(ARGV[4]) * 2)
  redis.call('PEXPIRE', KEYS[6], tonumber(ARGV[4]) * 2)
end
redis.call('PEXPIRE', KEYS[3], math.max(tonumber(ARGV[2]), tonumber(ARGV[4])) * 2)
return {1, token}
";

const SETTLE_TOKENS: &str = r"
local time = redis.call('TIME')
local now = tonumber(time[1]) * 1000 + math.floor(tonumber(time[2]) / 1000)
local previous = redis.call('HGET', KEYS[2], ARGV[1])
local score = redis.call('ZSCORE', KEYS[1], ARGV[1])
if previous and score and tonumber(score) > now - tonumber(ARGV[3]) then
  redis.call('INCRBY', KEYS[3], tonumber(ARGV[2]) - tonumber(previous))
else
  if previous then redis.call('DECRBY', KEYS[3], tonumber(previous)) end
  redis.call('ZADD', KEYS[1], now, ARGV[1])
  redis.call('INCRBY', KEYS[3], ARGV[2])
end
redis.call('HSET', KEYS[2], ARGV[1], ARGV[2])
redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[3]) * 2)
redis.call('PEXPIRE', KEYS[2], tonumber(ARGV[3]) * 2)
redis.call('PEXPIRE', KEYS[3], tonumber(ARGV[3]) * 2)
return 1
";

#[derive(Debug, Clone)]
pub(crate) struct QuotaPermit {
    tenant_key: String,
    token: String,
    counted_in_flight: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuotaRejection {
    InFlight,
    RequestsPerMinute,
    TokensPerMinute,
}

pub(crate) enum QuotaReservation {
    Granted(QuotaPermit),
    Rejected(QuotaRejection),
}

#[derive(Debug, Error)]
pub(crate) enum QuotaError {
    #[error("quota state lock is poisoned")]
    LockPoisoned,
    #[error("quota state backend returned an invalid rejection reason: {0}")]
    InvalidRejection(String),
    #[error("quota state backend failed: {0}")]
    Backend(#[from] redis::RedisError),
}

#[async_trait]
pub(crate) trait QuotaRepository: Send + Sync {
    async fn reserve(
        &self,
        tenant_key: &str,
        estimated_tokens: u64,
    ) -> Result<QuotaReservation, QuotaError>;
    async fn settle(&self, permit: &QuotaPermit, actual_tokens: u64) -> Result<(), QuotaError>;
    async fn release(&self, permit: QuotaPermit) -> Result<(), QuotaError>;
    fn backend_name(&self) -> &'static str;
    fn max_in_flight(&self) -> usize;
    fn requests_per_minute(&self) -> usize;
    fn tokens_per_minute(&self) -> u64;
}

pub(crate) enum QuotaAdmission {
    Granted(QuotaLease),
    Rejected(QuotaRejection),
}

pub(crate) struct QuotaLease {
    repository: Arc<dyn QuotaRepository>,
    permit: Option<QuotaPermit>,
}

impl QuotaLease {
    pub(crate) async fn acquire(
        repository: Arc<dyn QuotaRepository>,
        tenant_key: &str,
        estimated_tokens: u64,
    ) -> Result<QuotaAdmission, QuotaError> {
        match repository.reserve(tenant_key, estimated_tokens).await? {
            QuotaReservation::Granted(permit) => Ok(QuotaAdmission::Granted(Self {
                repository,
                permit: Some(permit),
            })),
            QuotaReservation::Rejected(reason) => Ok(QuotaAdmission::Rejected(reason)),
        }
    }

    pub(crate) async fn settle(&self, actual_tokens: u64) -> Result<(), QuotaError> {
        if let Some(permit) = &self.permit {
            self.repository.settle(permit, actual_tokens).await?;
        }
        Ok(())
    }

    pub(crate) async fn release(mut self) -> Result<(), QuotaError> {
        if let Some(permit) = self.permit.take() {
            self.repository.release(permit).await?;
        }
        Ok(())
    }
}

impl Drop for QuotaLease {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        let repository = Arc::clone(&self.repository);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = repository.release(permit).await;
            });
        }
    }
}

trait QuotaClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

struct SystemQuotaClock;

impl QuotaClock for SystemQuotaClock {
    fn now_ms(&self) -> u64 {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        u64::try_from(millis).unwrap_or(u64::MAX)
    }
}

#[derive(Default)]
struct MemoryQuotaState {
    permits: BTreeMap<String, BTreeSet<String>>,
    request_counts: BTreeMap<String, usize>,
    request_expirations: VecDeque<(u64, String)>,
    token_amounts: BTreeMap<(String, String), u64>,
    token_totals: BTreeMap<String, u64>,
    token_expirations: VecDeque<(u64, String, String)>,
}

impl MemoryQuotaState {
    fn prune_requests(&mut self, now_ms: u64) {
        while self
            .request_expirations
            .front()
            .is_some_and(|(expires_at, _)| *expires_at <= now_ms)
        {
            let (_, tenant) = self.request_expirations.pop_front().expect("front exists");
            if let Some(count) = self.request_counts.get_mut(&tenant) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.request_counts.remove(&tenant);
                }
            }
        }
    }

    fn prune_tokens(&mut self, now_ms: u64) {
        while self
            .token_expirations
            .front()
            .is_some_and(|(expires_at, _, _)| *expires_at <= now_ms)
        {
            let (_, tenant, token) = self.token_expirations.pop_front().expect("front exists");
            let amount = self
                .token_amounts
                .remove(&(tenant.clone(), token))
                .unwrap_or(0);
            if let Some(total) = self.token_totals.get_mut(&tenant) {
                *total = total.saturating_sub(amount);
                if *total == 0 {
                    self.token_totals.remove(&tenant);
                }
            }
        }
    }
}

pub(crate) struct MemoryQuotaRepository {
    max_in_flight: usize,
    requests_per_minute: usize,
    tokens_per_minute: u64,
    ticket: AtomicU64,
    clock: Arc<dyn QuotaClock>,
    state: Mutex<MemoryQuotaState>,
}

impl MemoryQuotaRepository {
    pub(crate) fn new(
        max_in_flight: usize,
        requests_per_minute: usize,
        tokens_per_minute: u64,
    ) -> Arc<Self> {
        Self::with_clock(
            max_in_flight,
            requests_per_minute,
            tokens_per_minute,
            Arc::new(SystemQuotaClock),
        )
    }

    fn with_clock(
        max_in_flight: usize,
        requests_per_minute: usize,
        tokens_per_minute: u64,
        clock: Arc<dyn QuotaClock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            max_in_flight,
            requests_per_minute,
            tokens_per_minute,
            ticket: AtomicU64::new(0),
            clock,
            state: Mutex::new(MemoryQuotaState::default()),
        })
    }
}

#[async_trait]
impl QuotaRepository for MemoryQuotaRepository {
    async fn reserve(
        &self,
        tenant_key: &str,
        estimated_tokens: u64,
    ) -> Result<QuotaReservation, QuotaError> {
        if self.max_in_flight == 0 && self.requests_per_minute == 0 && self.tokens_per_minute == 0 {
            return Ok(QuotaReservation::Granted(QuotaPermit {
                tenant_key: tenant_key.to_owned(),
                token: String::new(),
                counted_in_flight: false,
            }));
        }
        let now_ms = self.clock.now_ms();
        let mut state = self.state.lock().map_err(|_| QuotaError::LockPoisoned)?;
        state.prune_requests(now_ms);
        state.prune_tokens(now_ms);
        if self.max_in_flight > 0
            && state.permits.get(tenant_key).map_or(0, BTreeSet::len) >= self.max_in_flight
        {
            return Ok(QuotaReservation::Rejected(QuotaRejection::InFlight));
        }
        if self.requests_per_minute > 0
            && state.request_counts.get(tenant_key).copied().unwrap_or(0)
                >= self.requests_per_minute
        {
            return Ok(QuotaReservation::Rejected(
                QuotaRejection::RequestsPerMinute,
            ));
        }
        if self.tokens_per_minute > 0
            && state
                .token_totals
                .get(tenant_key)
                .copied()
                .unwrap_or(0)
                .saturating_add(estimated_tokens)
                > self.tokens_per_minute
        {
            return Ok(QuotaReservation::Rejected(QuotaRejection::TokensPerMinute));
        }

        let token = self.ticket.fetch_add(1, Ordering::Relaxed).to_string();
        if self.max_in_flight > 0 {
            state
                .permits
                .entry(tenant_key.to_owned())
                .or_default()
                .insert(token.clone());
        }
        if self.requests_per_minute > 0 {
            *state
                .request_counts
                .entry(tenant_key.to_owned())
                .or_default() += 1;
            state.request_expirations.push_back((
                now_ms.saturating_add(REQUEST_WINDOW_MS),
                tenant_key.to_owned(),
            ));
        }
        if self.tokens_per_minute > 0 {
            state
                .token_amounts
                .insert((tenant_key.to_owned(), token.clone()), estimated_tokens);
            let total = state.token_totals.entry(tenant_key.to_owned()).or_default();
            *total = total.saturating_add(estimated_tokens);
            state.token_expirations.push_back((
                now_ms.saturating_add(REQUEST_WINDOW_MS),
                tenant_key.to_owned(),
                token.clone(),
            ));
        }
        Ok(QuotaReservation::Granted(QuotaPermit {
            tenant_key: tenant_key.to_owned(),
            token,
            counted_in_flight: self.max_in_flight > 0,
        }))
    }

    async fn settle(&self, permit: &QuotaPermit, actual_tokens: u64) -> Result<(), QuotaError> {
        if self.tokens_per_minute == 0 {
            return Ok(());
        }
        let now_ms = self.clock.now_ms();
        let mut state = self.state.lock().map_err(|_| QuotaError::LockPoisoned)?;
        state.prune_tokens(now_ms);
        let key = (permit.tenant_key.clone(), permit.token.clone());
        if let Some(previous) = state.token_amounts.get_mut(&key) {
            let prior = *previous;
            *previous = actual_tokens;
            let total = state
                .token_totals
                .entry(permit.tenant_key.clone())
                .or_default();
            *total = total.saturating_sub(prior).saturating_add(actual_tokens);
        } else {
            state.token_amounts.insert(key, actual_tokens);
            let total = state
                .token_totals
                .entry(permit.tenant_key.clone())
                .or_default();
            *total = total.saturating_add(actual_tokens);
            state.token_expirations.push_back((
                now_ms.saturating_add(REQUEST_WINDOW_MS),
                permit.tenant_key.clone(),
                permit.token.clone(),
            ));
        }
        Ok(())
    }

    async fn release(&self, permit: QuotaPermit) -> Result<(), QuotaError> {
        if !permit.counted_in_flight {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| QuotaError::LockPoisoned)?;
        if let Some(tenant) = state.permits.get_mut(&permit.tenant_key) {
            tenant.remove(&permit.token);
            if tenant.is_empty() {
                state.permits.remove(&permit.tenant_key);
            }
        }
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }

    fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    fn requests_per_minute(&self) -> usize {
        self.requests_per_minute
    }

    fn tokens_per_minute(&self) -> u64 {
        self.tokens_per_minute
    }
}

pub(crate) struct RedisQuotaRepository {
    connection: ConnectionManager,
    prefix: String,
    max_in_flight: usize,
    requests_per_minute: usize,
    tokens_per_minute: u64,
    lease_ttl: Duration,
}

impl RedisQuotaRepository {
    pub(crate) async fn connect(
        url: &str,
        prefix: String,
        max_in_flight: usize,
        requests_per_minute: usize,
        tokens_per_minute: u64,
        lease_ttl: Duration,
    ) -> Result<Arc<Self>, QuotaError> {
        let client = redis::Client::open(url)?;
        Ok(Arc::new(Self {
            connection: ConnectionManager::new(client).await?,
            prefix,
            max_in_flight,
            requests_per_minute,
            tokens_per_minute,
            lease_ttl,
        }))
    }

    fn keys(&self, tenant_key: &str) -> [String; 6] {
        let digest = format!("{:x}", Sha256::digest(tenant_key.as_bytes()));
        [
            format!("{}:quota:in-flight:{digest}", self.prefix),
            format!("{}:quota:rpm:{digest}", self.prefix),
            format!("{}:quota:sequence:{digest}", self.prefix),
            format!("{}:quota:tpm-events:{digest}", self.prefix),
            format!("{}:quota:tpm-amounts:{digest}", self.prefix),
            format!("{}:quota:tpm-total:{digest}", self.prefix),
        ]
    }
}

#[async_trait]
impl QuotaRepository for RedisQuotaRepository {
    async fn reserve(
        &self,
        tenant_key: &str,
        estimated_tokens: u64,
    ) -> Result<QuotaReservation, QuotaError> {
        if self.max_in_flight == 0 && self.requests_per_minute == 0 && self.tokens_per_minute == 0 {
            return Ok(QuotaReservation::Granted(QuotaPermit {
                tenant_key: tenant_key.to_owned(),
                token: String::new(),
                counted_in_flight: false,
            }));
        }
        let keys = self.keys(tenant_key);
        let ttl_ms = u64::try_from(self.lease_ttl.as_millis()).unwrap_or(u64::MAX);
        let token_limit = i64::try_from(self.tokens_per_minute).unwrap_or(i64::MAX);
        let estimated_tokens = i64::try_from(estimated_tokens).unwrap_or(i64::MAX);
        let mut connection = self.connection.clone();
        let (granted, value): (i64, String) = Script::new(RESERVE_PERMIT)
            .key(&keys[0])
            .key(&keys[1])
            .key(&keys[2])
            .key(&keys[3])
            .key(&keys[4])
            .key(&keys[5])
            .arg(self.max_in_flight)
            .arg(ttl_ms.max(1))
            .arg(self.requests_per_minute)
            .arg(REQUEST_WINDOW_MS)
            .arg(token_limit)
            .arg(estimated_tokens)
            .invoke_async(&mut connection)
            .await?;
        if granted == 0 {
            let reason = match value.as_str() {
                "in_flight" => QuotaRejection::InFlight,
                "requests_per_minute" => QuotaRejection::RequestsPerMinute,
                "tokens_per_minute" => QuotaRejection::TokensPerMinute,
                _ => return Err(QuotaError::InvalidRejection(value)),
            };
            return Ok(QuotaReservation::Rejected(reason));
        }
        Ok(QuotaReservation::Granted(QuotaPermit {
            tenant_key: tenant_key.to_owned(),
            token: value,
            counted_in_flight: self.max_in_flight > 0,
        }))
    }

    async fn settle(&self, permit: &QuotaPermit, actual_tokens: u64) -> Result<(), QuotaError> {
        if self.tokens_per_minute == 0 {
            return Ok(());
        }
        let keys = self.keys(&permit.tenant_key);
        let actual_tokens = i64::try_from(actual_tokens).unwrap_or(i64::MAX);
        let mut connection = self.connection.clone();
        Script::new(SETTLE_TOKENS)
            .key(&keys[3])
            .key(&keys[4])
            .key(&keys[5])
            .arg(&permit.token)
            .arg(actual_tokens)
            .arg(REQUEST_WINDOW_MS)
            .invoke_async::<i64>(&mut connection)
            .await?;
        Ok(())
    }

    async fn release(&self, permit: QuotaPermit) -> Result<(), QuotaError> {
        if !permit.counted_in_flight {
            return Ok(());
        }
        let keys = self.keys(&permit.tenant_key);
        let mut connection = self.connection.clone();
        redis::cmd("ZREM")
            .arg(&keys[0])
            .arg(permit.token)
            .query_async::<i64>(&mut connection)
            .await?;
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }

    fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    fn requests_per_minute(&self) -> usize {
        self.requests_per_minute
    }

    fn tokens_per_minute(&self) -> u64 {
        self.tokens_per_minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn granted(reservation: QuotaReservation) -> QuotaPermit {
        let QuotaReservation::Granted(permit) = reservation else {
            panic!("quota reservation should be granted");
        };
        permit
    }

    async fn assert_concurrency_contract(repository: Arc<dyn QuotaRepository>) {
        let first = granted(repository.reserve("tenant-a", 0).await.unwrap());
        assert!(matches!(
            repository.reserve("tenant-a", 0).await.unwrap(),
            QuotaReservation::Rejected(QuotaRejection::InFlight)
        ));
        let other = granted(repository.reserve("tenant-b", 0).await.unwrap());
        repository.release(first).await.unwrap();
        let replacement = granted(repository.reserve("tenant-a", 0).await.unwrap());
        repository.release(replacement).await.unwrap();
        repository.release(other).await.unwrap();
    }

    async fn assert_unique_permits(repository: Arc<dyn QuotaRepository>) {
        let first = granted(repository.reserve("tenant", 0).await.unwrap());
        let second = granted(repository.reserve("tenant", 0).await.unwrap());
        assert_ne!(first.token, second.token);
        repository.release(first).await.unwrap();
        repository.release(second).await.unwrap();
    }

    async fn assert_rpm_contract(repository: Arc<dyn QuotaRepository>) {
        let first = granted(repository.reserve("tenant-a", 0).await.unwrap());
        let second = granted(repository.reserve("tenant-a", 0).await.unwrap());
        repository.release(first).await.unwrap();
        repository.release(second).await.unwrap();
        assert!(matches!(
            repository.reserve("tenant-a", 0).await.unwrap(),
            QuotaReservation::Rejected(QuotaRejection::RequestsPerMinute)
        ));
        let other = granted(repository.reserve("tenant-b", 0).await.unwrap());
        repository.release(other).await.unwrap();
    }

    async fn assert_tpm_contract(repository: Arc<dyn QuotaRepository>) {
        let first = granted(repository.reserve("tenant-a", 6).await.unwrap());
        assert!(matches!(
            repository.reserve("tenant-a", 5).await.unwrap(),
            QuotaReservation::Rejected(QuotaRejection::TokensPerMinute)
        ));
        repository.settle(&first, 3).await.unwrap();
        repository.release(first).await.unwrap();
        let second = granted(repository.reserve("tenant-a", 7).await.unwrap());
        repository.settle(&second, 8).await.unwrap();
        repository.release(second).await.unwrap();
        assert!(matches!(
            repository.reserve("tenant-a", 1).await.unwrap(),
            QuotaReservation::Rejected(QuotaRejection::TokensPerMinute)
        ));
        let other = granted(repository.reserve("tenant-b", 10).await.unwrap());
        repository.release(other).await.unwrap();
    }

    #[derive(Default)]
    struct TestClock(AtomicU64);

    impl QuotaClock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[tokio::test]
    async fn memory_quota_is_atomic_scoped_and_releases_capacity() {
        assert_concurrency_contract(MemoryQuotaRepository::new(1, 0, 0)).await;
    }

    #[tokio::test]
    async fn unlimited_memory_quota_does_not_retain_state() {
        let repository = MemoryQuotaRepository::new(0, 0, 0);
        for _ in 0..100 {
            let permit = granted(repository.reserve("tenant", 0).await.unwrap());
            repository.release(permit).await.unwrap();
        }
        let state = repository.state.lock().unwrap();
        assert!(state.permits.is_empty());
        assert!(state.request_counts.is_empty());
    }

    #[tokio::test]
    async fn memory_quota_assigns_unique_permits() {
        assert_unique_permits(MemoryQuotaRepository::new(2, 0, 0)).await;
    }

    #[tokio::test]
    async fn memory_rpm_is_sliding_tenant_scoped_and_not_released() {
        let clock = Arc::new(TestClock::default());
        let repository = MemoryQuotaRepository::with_clock(0, 2, 0, clock.clone());
        assert_rpm_contract(repository.clone()).await;

        clock.0.store(REQUEST_WINDOW_MS, Ordering::Relaxed);
        assert!(matches!(
            repository.reserve("tenant-a", 0).await.unwrap(),
            QuotaReservation::Granted(_)
        ));
        let state = repository.state.lock().unwrap();
        assert!(!state.request_counts.contains_key("tenant-b"));
    }

    #[tokio::test]
    async fn memory_tpm_reserves_settles_and_expires() {
        let clock = Arc::new(TestClock::default());
        let repository = MemoryQuotaRepository::with_clock(0, 0, 10, clock.clone());
        assert_tpm_contract(repository.clone()).await;

        clock.0.store(REQUEST_WINDOW_MS, Ordering::Relaxed);
        assert!(matches!(
            repository.reserve("tenant-a", 10).await.unwrap(),
            QuotaReservation::Granted(_)
        ));
        let state = repository.state.lock().unwrap();
        assert!(!state.token_totals.contains_key("tenant-b"));
    }

    #[tokio::test]
    async fn memory_tpm_release_without_usage_keeps_reservation() {
        let repository = MemoryQuotaRepository::new(0, 0, 10);
        let permit = granted(repository.reserve("tenant", 10).await.unwrap());
        repository.release(permit).await.unwrap();
        assert!(matches!(
            repository.reserve("tenant", 1).await.unwrap(),
            QuotaReservation::Rejected(QuotaRejection::TokensPerMinute)
        ));
    }

    #[tokio::test]
    #[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
    async fn redis_quota_passes_shared_contract() {
        let url = std::env::var("UROUTER_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16380/".to_owned());
        let prefix = format!("urouter-quota-contract-{}", std::process::id());
        let repository = RedisQuotaRepository::connect(
            &url,
            format!("{prefix}:limit"),
            1,
            0,
            0,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_concurrency_contract(repository).await;
        let repository = RedisQuotaRepository::connect(
            &url,
            format!("{prefix}:unique"),
            2,
            0,
            0,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_unique_permits(repository).await;
        let repository = RedisQuotaRepository::connect(
            &url,
            format!("{prefix}:rpm"),
            0,
            2,
            0,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_rpm_contract(repository).await;
        let repository = RedisQuotaRepository::connect(
            &url,
            format!("{prefix}:tpm"),
            0,
            0,
            10,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_tpm_contract(repository).await;
    }
}
