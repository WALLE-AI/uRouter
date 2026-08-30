use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use redis::{
    Script,
    aio::{ConnectionManager, ConnectionManagerConfig},
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    RouteDeployment, UpstreamErrorKind,
    capacity::{CircuitState, CooldownPolicy},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureScope {
    Deployment,
    Credential,
    Provider,
}

impl FailureScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deployment => "deployment",
            Self::Credential => "credential",
            Self::Provider => "provider",
        }
    }
}

#[derive(Debug, Clone)]
struct CircuitTarget {
    scope: FailureScope,
    id: String,
}

#[derive(Debug, Clone)]
pub struct CircuitPermit {
    targets: Vec<CircuitTarget>,
    token: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SharedCircuitSnapshot {
    pub scope: FailureScope,
    pub state: CircuitState,
    pub cooldown_remaining_ms: u64,
}

#[derive(Debug, Error)]
pub enum CircuitError {
    #[error("shared circuit backend failed: {0}")]
    Backend(#[from] redis::RedisError),
}

#[async_trait]
pub trait SharedCircuitRepository: Send + Sync {
    async fn acquire(
        &self,
        deployment: &RouteDeployment,
    ) -> Result<Option<CircuitPermit>, CircuitError>;
    async fn finish(
        &self,
        permit: CircuitPermit,
        result: Result<(), UpstreamErrorKind>,
        tier_size: usize,
    ) -> Result<(), CircuitError>;
    async fn abandon(&self, permit: CircuitPermit) -> Result<(), CircuitError>;
    async fn snapshot(
        &self,
        deployment: &RouteDeployment,
    ) -> Result<Vec<SharedCircuitSnapshot>, CircuitError>;
    fn backend_name(&self) -> &'static str;
}

#[derive(Default)]
pub struct LocalCircuitRepository {
    ticket: AtomicU64,
}

#[async_trait]
impl SharedCircuitRepository for LocalCircuitRepository {
    async fn acquire(
        &self,
        deployment: &RouteDeployment,
    ) -> Result<Option<CircuitPermit>, CircuitError> {
        Ok(Some(CircuitPermit {
            targets: circuit_targets(deployment),
            token: self.ticket.fetch_add(1, Ordering::Relaxed).to_string(),
        }))
    }

    async fn finish(
        &self,
        _permit: CircuitPermit,
        _result: Result<(), UpstreamErrorKind>,
        _tier_size: usize,
    ) -> Result<(), CircuitError> {
        Ok(())
    }

    async fn abandon(&self, _permit: CircuitPermit) -> Result<(), CircuitError> {
        Ok(())
    }

    async fn snapshot(
        &self,
        _deployment: &RouteDeployment,
    ) -> Result<Vec<SharedCircuitSnapshot>, CircuitError> {
        Ok(Vec::new())
    }

    fn backend_name(&self) -> &'static str {
        "local"
    }
}

#[derive(Clone)]
pub struct RedisCircuitRepository {
    connection: ConnectionManager,
    prefix: String,
    policy: CooldownPolicy,
    ticket: std::sync::Arc<AtomicU64>,
}

impl RedisCircuitRepository {
    pub async fn connect(
        url: &str,
        prefix: String,
        policy: CooldownPolicy,
    ) -> Result<Self, CircuitError> {
        let client = redis::Client::open(url)?;
        Ok(Self {
            connection: ConnectionManager::new_with_config(client, redis_connection_config())
                .await?,
            prefix,
            policy,
            ticket: std::sync::Arc::new(AtomicU64::new(0)),
        })
    }

    fn key(&self, target: &CircuitTarget) -> String {
        let digest = Sha256::digest(target.id.as_bytes());
        format!(
            "{}:circuit:{}:{digest:x}",
            self.prefix,
            target.scope.as_str()
        )
    }

    fn token(&self) -> String {
        let ticket = self.ticket.fetch_add(1, Ordering::Relaxed);
        format!("{}-{ticket}", std::process::id())
    }
}

fn redis_connection_config() -> ConnectionManagerConfig {
    ConnectionManagerConfig::new()
        .set_response_timeout(Duration::from_secs(2))
        .set_connection_timeout(Duration::from_secs(1))
        .set_max_delay(500)
        .set_number_of_retries(3)
}

#[async_trait]
impl SharedCircuitRepository for RedisCircuitRepository {
    async fn acquire(
        &self,
        deployment: &RouteDeployment,
    ) -> Result<Option<CircuitPermit>, CircuitError> {
        const ACQUIRE: &str = r"
local time = redis.call('TIME')
local now = tonumber(time[1]) * 1000 + math.floor(tonumber(time[2]) / 1000)
for _, key in ipairs(KEYS) do
  local open_until = tonumber(redis.call('HGET', key, 'open_until') or '0')
  if open_until > now then return 0 end
  if open_until > 0 then
    local probe_until = tonumber(redis.call('HGET', key, 'probe_until') or '0')
    if probe_until > now then return 0 end
  end
end
for _, key in ipairs(KEYS) do
  local open_until = tonumber(redis.call('HGET', key, 'open_until') or '0')
  if open_until > 0 then
    redis.call('HSET', key, 'probe_token', ARGV[1], 'probe_until', now + tonumber(ARGV[2]))
    redis.call('PEXPIRE', key, ARGV[3])
  end
end
return 1
";
        let targets = circuit_targets(deployment);
        let token = self.token();
        let script = Script::new(ACQUIRE);
        let mut invocation = script.prepare_invoke();
        for target in &targets {
            invocation.key(self.key(target));
        }
        invocation
            .arg(&token)
            .arg(duration_millis(self.policy.cooldown))
            .arg(state_ttl_millis(self.policy));
        let mut connection = self.connection.clone();
        let acquired: i64 = invocation.invoke_async(&mut connection).await?;
        Ok((acquired == 1).then_some(CircuitPermit { targets, token }))
    }

    async fn finish(
        &self,
        permit: CircuitPermit,
        result: Result<(), UpstreamErrorKind>,
        tier_size: usize,
    ) -> Result<(), CircuitError> {
        const FINISH: &str = r"
local time = redis.call('TIME')
local now = tonumber(time[1]) * 1000 + math.floor(tonumber(time[2]) / 1000)
local failure_index = tonumber(ARGV[3])
for index, key in ipairs(KEYS) do
  if redis.call('HGET', key, 'probe_token') == ARGV[1] then
    redis.call('HDEL', key, 'probe_token', 'probe_until')
  end
  local started = tonumber(redis.call('HGET', key, 'window_started') or '0')
  if started == 0 or now - started > tonumber(ARGV[7]) then
    redis.call('HSET', key, 'window_started', now, 'successes', 0, 'failures', 0)
  end
  if failure_index == 0 then
    redis.call('HINCRBY', key, 'successes', 1)
    redis.call('HSET', key, 'open_until', 0)
  elseif index == failure_index then
    local failures = redis.call('HINCRBY', key, 'failures', 1)
    local successes = tonumber(redis.call('HGET', key, 'successes') or '0')
    local total = successes + failures
    local ratio = math.floor(failures * 1000 / total)
    if tonumber(ARGV[4]) > 1 and (ARGV[5] == '1' or ratio >= tonumber(ARGV[6])) then
      redis.call('HSET', key, 'open_until', now + tonumber(ARGV[2]))
    end
  end
  redis.call('PEXPIRE', key, ARGV[8])
end
return 1
";
        let (failure_index, force_open) = match result {
            Ok(()) => (0, false),
            Err(kind) => (
                permit
                    .targets
                    .iter()
                    .position(|target| target.scope == failure_scope(kind))
                    .map_or(1, |index| index + 1),
                matches!(
                    kind,
                    UpstreamErrorKind::RateLimited | UpstreamErrorKind::Unauthorized
                ),
            ),
        };
        let script = Script::new(FINISH);
        let mut invocation = script.prepare_invoke();
        for target in &permit.targets {
            invocation.key(self.key(target));
        }
        invocation
            .arg(permit.token)
            .arg(duration_millis(self.policy.cooldown))
            .arg(failure_index)
            .arg(tier_size)
            .arg(i64::from(force_open))
            .arg(self.policy.failure_threshold_millis)
            .arg(duration_millis(self.policy.window))
            .arg(state_ttl_millis(self.policy));
        let mut connection = self.connection.clone();
        invocation.invoke_async::<i64>(&mut connection).await?;
        Ok(())
    }

    async fn abandon(&self, permit: CircuitPermit) -> Result<(), CircuitError> {
        const ABANDON: &str = r"
for _, key in ipairs(KEYS) do
  if redis.call('HGET', key, 'probe_token') == ARGV[1] then
    redis.call('HDEL', key, 'probe_token', 'probe_until')
  end
end
return 1
";
        let script = Script::new(ABANDON);
        let mut invocation = script.prepare_invoke();
        for target in &permit.targets {
            invocation.key(self.key(target));
        }
        invocation.arg(permit.token);
        let mut connection = self.connection.clone();
        invocation.invoke_async::<i64>(&mut connection).await?;
        Ok(())
    }

    async fn snapshot(
        &self,
        deployment: &RouteDeployment,
    ) -> Result<Vec<SharedCircuitSnapshot>, CircuitError> {
        const SNAPSHOT: &str = r"
local time = redis.call('TIME')
local now = tonumber(time[1]) * 1000 + math.floor(tonumber(time[2]) / 1000)
local result = {}
for index, key in ipairs(KEYS) do
  local open_until = tonumber(redis.call('HGET', key, 'open_until') or '0')
  local state = 0
  local remaining = 0
  if open_until > now then
    state = 1
    remaining = open_until - now
  elseif open_until > 0 then
    state = 2
  end
  table.insert(result, index)
  table.insert(result, state)
  table.insert(result, remaining)
end
return result
";
        let targets = circuit_targets(deployment);
        let script = Script::new(SNAPSHOT);
        let mut invocation = script.prepare_invoke();
        for target in &targets {
            invocation.key(self.key(target));
        }
        let mut connection = self.connection.clone();
        let values: Vec<i64> = invocation.invoke_async(&mut connection).await?;
        Ok(values
            .chunks_exact(3)
            .filter_map(|chunk| {
                let target = targets.get(usize::try_from(chunk[0]).ok()?.saturating_sub(1))?;
                Some(SharedCircuitSnapshot {
                    scope: target.scope,
                    state: match chunk[1] {
                        1 => CircuitState::Open,
                        2 => CircuitState::HalfOpen,
                        _ => CircuitState::Closed,
                    },
                    cooldown_remaining_ms: u64::try_from(chunk[2]).unwrap_or(0),
                })
            })
            .collect())
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }
}

fn circuit_targets(deployment: &RouteDeployment) -> Vec<CircuitTarget> {
    let provider = deployment.provider_scope.clone().unwrap_or_else(|| {
        deployment.model.as_str().split_once('/').map_or_else(
            || deployment.model.to_string(),
            |(provider, _)| provider.to_owned(),
        )
    });
    let credential = deployment
        .credential_scope
        .clone()
        .unwrap_or_else(|| provider.clone());
    vec![
        CircuitTarget {
            scope: FailureScope::Deployment,
            id: deployment.id.clone(),
        },
        CircuitTarget {
            scope: FailureScope::Credential,
            id: credential,
        },
        CircuitTarget {
            scope: FailureScope::Provider,
            id: provider,
        },
    ]
}

const fn failure_scope(kind: UpstreamErrorKind) -> FailureScope {
    match kind {
        UpstreamErrorKind::Unauthorized | UpstreamErrorKind::RateLimited => {
            FailureScope::Credential
        }
        UpstreamErrorKind::ProviderUnavailable => FailureScope::Provider,
        _ => FailureScope::Deployment,
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn state_ttl_millis(policy: CooldownPolicy) -> u64 {
    duration_millis(policy.window.max(policy.cooldown.saturating_mul(4)))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::time::sleep;
    use urouter_types::ModelId;

    use super::*;

    fn deployment(id: &str, provider: &str, credential: &str) -> RouteDeployment {
        RouteDeployment {
            id: id.to_owned(),
            model: ModelId::new("local-vllm/qwen3.5-4b").unwrap(),
            base_url: None,
            weight: 1,
            order: 0,
            provider_scope: Some(provider.to_owned()),
            credential_scope: Some(credential.to_owned()),
            enabled: true,
            credential_available: true,
            region: None,
            residency: Vec::new(),
            tenant_allowlist: Vec::new(),
            quota_usage_millis: None,
            accept_new_requests: true,
            binding_grace_until_unix: None,
        }
    }

    fn test_policy() -> CooldownPolicy {
        CooldownPolicy {
            cooldown: Duration::from_millis(50),
            window: Duration::from_secs(1),
            failure_threshold_millis: 500,
        }
    }

    fn prefix(label: &str) -> String {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("urouter-circuit-test-{label}-{unique}")
    }

    #[test]
    fn errors_map_to_the_narrowest_actionable_scope() {
        assert_eq!(
            failure_scope(UpstreamErrorKind::Unauthorized),
            FailureScope::Credential
        );
        assert_eq!(
            failure_scope(UpstreamErrorKind::RateLimited),
            FailureScope::Credential
        );
        assert_eq!(
            failure_scope(UpstreamErrorKind::ProviderUnavailable),
            FailureScope::Provider
        );
        assert_eq!(
            failure_scope(UpstreamErrorKind::Transport),
            FailureScope::Deployment
        );
    }

    #[tokio::test]
    #[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
    async fn redis_circuits_share_scopes_and_single_half_open_probe() {
        let url = std::env::var("UROUTER_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379/".to_owned());
        let shared_prefix = prefix("provider");
        let first = RedisCircuitRepository::connect(&url, shared_prefix.clone(), test_policy())
            .await
            .unwrap();
        let second = RedisCircuitRepository::connect(&url, shared_prefix, test_policy())
            .await
            .unwrap();
        let provider_a = deployment("a-1", "provider-a", "credential-a");
        let provider_a_backup = deployment("a-2", "provider-a", "credential-b");
        let provider_b = deployment("b-1", "provider-b", "credential-c");

        let permit = first.acquire(&provider_a).await.unwrap().unwrap();
        first
            .finish(permit, Err(UpstreamErrorKind::ProviderUnavailable), 2)
            .await
            .unwrap();
        assert!(second.acquire(&provider_a_backup).await.unwrap().is_none());
        assert!(
            second
                .snapshot(&provider_a_backup)
                .await
                .unwrap()
                .iter()
                .any(
                    |item| item.scope == FailureScope::Provider && item.state == CircuitState::Open
                )
        );
        assert!(second.acquire(&provider_b).await.unwrap().is_some());

        sleep(Duration::from_millis(60)).await;
        assert!(
            second
                .snapshot(&provider_a_backup)
                .await
                .unwrap()
                .iter()
                .any(|item| {
                    item.scope == FailureScope::Provider && item.state == CircuitState::HalfOpen
                })
        );
        let probe = first.acquire(&provider_a_backup).await.unwrap().unwrap();
        assert!(second.acquire(&provider_a).await.unwrap().is_none());
        first.abandon(probe).await.unwrap();
        assert!(second.acquire(&provider_a).await.unwrap().is_some());
    }

    #[tokio::test]
    #[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
    async fn redis_credential_and_deployment_failures_do_not_overblock() {
        let url = std::env::var("UROUTER_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379/".to_owned());
        let credential_store =
            RedisCircuitRepository::connect(&url, prefix("credential"), test_policy())
                .await
                .unwrap();
        let first = deployment("a-1", "provider-a", "credential-a");
        let same_credential = deployment("a-2", "provider-a", "credential-a");
        let other_credential = deployment("a-3", "provider-a", "credential-b");
        let permit = credential_store.acquire(&first).await.unwrap().unwrap();
        credential_store
            .finish(permit, Err(UpstreamErrorKind::Unauthorized), 3)
            .await
            .unwrap();
        assert!(
            credential_store
                .acquire(&same_credential)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            credential_store
                .acquire(&other_credential)
                .await
                .unwrap()
                .is_some()
        );

        let deployment_store =
            RedisCircuitRepository::connect(&url, prefix("deployment"), test_policy())
                .await
                .unwrap();
        let permit = deployment_store.acquire(&first).await.unwrap().unwrap();
        deployment_store
            .finish(permit, Err(UpstreamErrorKind::ServerError), 3)
            .await
            .unwrap();
        assert!(deployment_store.acquire(&first).await.unwrap().is_none());
        assert!(
            deployment_store
                .acquire(&same_credential)
                .await
                .unwrap()
                .is_some()
        );
    }
}
