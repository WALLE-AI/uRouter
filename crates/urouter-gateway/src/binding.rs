use std::{collections::BTreeMap, sync::Mutex, time::Duration};

use async_trait::async_trait;
use redis::{
    AsyncCommands, Script,
    aio::{ConnectionManager, ConnectionManagerConfig},
};
use serde::Serialize;
use thiserror::Error;
use urouter_types::ModelId;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskBinding {
    #[serde(skip_serializing)]
    pub(crate) tenant_key: String,
    #[serde(skip_serializing)]
    pub(crate) binding_key: String,
    pub(crate) task_key: String,
    pub(crate) conversation_key: Option<String>,
    pub(crate) branch_key: Option<String>,
    pub(crate) tier: String,
    pub(crate) model: ModelId,
    pub(crate) provider: String,
    pub(crate) api: String,
    pub(crate) agent_harness: Option<String>,
    pub(crate) prompt_profile_hash: Option<String>,
    pub(crate) toolset_hash: Option<String>,
    pub(crate) bound_at_turn: Option<String>,
    pub(crate) last_seen_turn: Option<String>,
    pub(crate) generation: u64,
    #[serde(skip_serializing)]
    pub(crate) tenant_generation: u64,
    #[serde(skip_serializing)]
    pub(crate) task_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingWrite {
    Created,
    Unchanged,
    Migrated,
    Conflict,
}

#[derive(Debug, Error)]
pub(crate) enum BindingStoreError {
    #[error("binding state backend failed: {0}")]
    Backend(#[from] redis::RedisError),
    #[error("binding state lock is poisoned")]
    LockPoisoned,
    #[error("binding state backend returned an invalid outcome: {0}")]
    InvalidOutcome(i64),
    #[error("binding state backend returned invalid data: {0}")]
    InvalidData(String),
    #[error("binding write crossed a deletion boundary")]
    StaleScope,
}

#[async_trait]
pub(crate) trait TaskBindingRepository: Send + Sync {
    async fn get(&self, task_key: &str) -> Result<Option<TaskBinding>, BindingStoreError>;
    async fn put(
        &self,
        binding: TaskBinding,
        allow_migration: bool,
    ) -> Result<BindingWrite, BindingStoreError>;
    async fn scope_generation(
        &self,
        tenant_key: &str,
        task_key: Option<&str>,
    ) -> Result<(u64, u64), BindingStoreError>;
    async fn remove(&self, task_key: &str) -> Result<bool, BindingStoreError>;
    async fn remove_task(
        &self,
        tenant_key: &str,
        task_key: &str,
    ) -> Result<usize, BindingStoreError>;
    async fn remove_tenant(&self, tenant_key: &str) -> Result<usize, BindingStoreError>;
    fn backend_name(&self) -> &'static str;
}

#[derive(Debug, Default)]
struct MemoryState {
    values: BTreeMap<String, TaskBinding>,
    order: Vec<String>,
    tenant_generations: BTreeMap<String, u64>,
    task_generations: BTreeMap<String, u64>,
}

pub(crate) struct MemoryTaskBindingRepository {
    state: Mutex<MemoryState>,
    capacity: usize,
}

impl MemoryTaskBindingRepository {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(MemoryState::default()),
            capacity,
        }
    }
}

#[async_trait]
impl TaskBindingRepository for MemoryTaskBindingRepository {
    async fn get(&self, task_key: &str) -> Result<Option<TaskBinding>, BindingStoreError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| BindingStoreError::LockPoisoned)?
            .values
            .get(task_key)
            .cloned())
    }

    async fn put(
        &self,
        mut binding: TaskBinding,
        allow_migration: bool,
    ) -> Result<BindingWrite, BindingStoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BindingStoreError::LockPoisoned)?;
        if state
            .tenant_generations
            .get(&binding.tenant_key)
            .copied()
            .unwrap_or_default()
            != binding.tenant_generation
            || state
                .task_generations
                .get(&binding.task_key)
                .copied()
                .unwrap_or_default()
                != binding.task_generation
        {
            return Err(BindingStoreError::StaleScope);
        }
        let outcome = match state.values.get(&binding.binding_key) {
            None => BindingWrite::Created,
            Some(current)
                if current.model == binding.model
                    && current.provider == binding.provider
                    && current.api == binding.api
                    && current.prompt_profile_hash == binding.prompt_profile_hash
                    && current.toolset_hash == binding.toolset_hash =>
            {
                binding.generation = current.generation;
                binding.bound_at_turn.clone_from(&current.bound_at_turn);
                BindingWrite::Unchanged
            }
            Some(_) if !allow_migration => return Ok(BindingWrite::Conflict),
            Some(current) => {
                binding.generation = current.generation.saturating_add(1);
                BindingWrite::Migrated
            }
        };
        if !state.values.contains_key(&binding.binding_key) {
            while state.values.len() >= self.capacity {
                if state.order.is_empty() {
                    break;
                }
                let oldest = state.order.remove(0);
                state.values.remove(&oldest);
            }
            state.order.push(binding.binding_key.clone());
        }
        state.values.insert(binding.binding_key.clone(), binding);
        Ok(outcome)
    }

    async fn scope_generation(
        &self,
        tenant_key: &str,
        task_key: Option<&str>,
    ) -> Result<(u64, u64), BindingStoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| BindingStoreError::LockPoisoned)?;
        Ok((
            state
                .tenant_generations
                .get(tenant_key)
                .copied()
                .unwrap_or_default(),
            task_key
                .and_then(|key| state.task_generations.get(key).copied())
                .unwrap_or_default(),
        ))
    }

    async fn remove(&self, task_key: &str) -> Result<bool, BindingStoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BindingStoreError::LockPoisoned)?;
        let generation = state
            .task_generations
            .entry(task_key.to_owned())
            .or_default();
        *generation = generation.saturating_add(1);
        let removed = state.values.remove(task_key).is_some();
        if removed {
            state.order.retain(|key| key != task_key);
        }
        Ok(removed)
    }

    async fn remove_task(
        &self,
        _tenant_key: &str,
        task_key: &str,
    ) -> Result<usize, BindingStoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BindingStoreError::LockPoisoned)?;
        let generation = state
            .task_generations
            .entry(task_key.to_owned())
            .or_default();
        *generation = generation.saturating_add(1);
        let keys = state
            .values
            .iter()
            .filter(|(_, binding)| binding.task_key == task_key)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in &keys {
            state.values.remove(key);
        }
        state.order.retain(|key| !keys.contains(key));
        Ok(keys.len())
    }

    async fn remove_tenant(&self, tenant_key: &str) -> Result<usize, BindingStoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BindingStoreError::LockPoisoned)?;
        let generation = state
            .tenant_generations
            .entry(tenant_key.to_owned())
            .or_default();
        *generation = generation.saturating_add(1);
        let keys = state
            .values
            .iter()
            .filter(|(_, binding)| binding.tenant_key == tenant_key)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in &keys {
            state.values.remove(key);
        }
        state.order.retain(|key| !keys.contains(key));
        Ok(keys.len())
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }
}

#[derive(Clone)]
pub(crate) struct RedisTaskBindingRepository {
    connection: ConnectionManager,
    prefix: String,
    ttl_seconds: u64,
}

impl RedisTaskBindingRepository {
    pub(crate) async fn connect(
        url: &str,
        prefix: String,
        ttl_seconds: u64,
    ) -> Result<Self, BindingStoreError> {
        let client = redis::Client::open(url)?;
        let connection =
            ConnectionManager::new_with_config(client, redis_connection_config()).await?;
        Ok(Self {
            connection,
            prefix,
            ttl_seconds,
        })
    }

    fn binding_key(&self, task_key: &str) -> String {
        format!("{}:binding:{}", self.prefix, task_key)
    }

    fn tenant_index_key(&self, tenant_key: &str) -> String {
        format!("{}:tenant-bindings:{}", self.prefix, tenant_key)
    }

    fn tenant_generation_key(&self, tenant_key: &str) -> String {
        format!("{}:tenant-generation:{tenant_key}", self.prefix)
    }

    fn task_generation_key(&self, task_key: &str) -> String {
        format!("{}:task-generation:{task_key}", self.prefix)
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
impl TaskBindingRepository for RedisTaskBindingRepository {
    async fn get(&self, task_key: &str) -> Result<Option<TaskBinding>, BindingStoreError> {
        let mut connection = self.connection.clone();
        let fields: BTreeMap<String, String> =
            connection.hgetall(self.binding_key(task_key)).await?;
        if fields.is_empty() {
            return Ok(None);
        }
        let required = |name: &str| {
            fields
                .get(name)
                .cloned()
                .ok_or_else(|| BindingStoreError::InvalidData(format!("missing {name}")))
        };
        let optional = |name: &str| fields.get(name).filter(|value| !value.is_empty()).cloned();
        Ok(Some(TaskBinding {
            tenant_key: required("tenant_key")?,
            binding_key: required("binding_key").unwrap_or_else(|_| task_key.to_owned()),
            task_key: required("task_key")?,
            conversation_key: optional("conversation_key"),
            branch_key: optional("branch_key"),
            tier: required("tier")?,
            model: ModelId::new(required("model")?)
                .map_err(|error| BindingStoreError::InvalidData(error.to_string()))?,
            provider: optional("provider").unwrap_or_default(),
            api: optional("api").unwrap_or_default(),
            agent_harness: optional("agent_harness"),
            prompt_profile_hash: optional("prompt_profile_hash"),
            toolset_hash: optional("toolset_hash"),
            bound_at_turn: optional("bound_at_turn"),
            last_seen_turn: optional("last_seen_turn"),
            generation: required("generation")?.parse().map_err(
                |error: std::num::ParseIntError| BindingStoreError::InvalidData(error.to_string()),
            )?,
            tenant_generation: 0,
            task_generation: 0,
        }))
    }

    async fn put(
        &self,
        binding: TaskBinding,
        allow_migration: bool,
    ) -> Result<BindingWrite, BindingStoreError> {
        const CAS: &str = r"
local tenant_generation = tonumber(redis.call('GET', KEYS[3]) or '0')
local task_generation = tonumber(redis.call('GET', KEYS[4]) or '0')
if tenant_generation ~= tonumber(ARGV[17]) or task_generation ~= tonumber(ARGV[18]) then return 5 end
local current_model = redis.call('HGET', KEYS[1], 'model')
local outcome = 1
local generation = 1
local bound_at_turn = ARGV[12]
if current_model then
  local stored_provider = redis.call('HGET', KEYS[1], 'provider')
  local stored_api = redis.call('HGET', KEYS[1], 'api')
  local stored_prompt = redis.call('HGET', KEYS[1], 'prompt_profile_hash')
  local stored_tools = redis.call('HGET', KEYS[1], 'toolset_hash')
  local same_identity = current_model == ARGV[7]
    and (not stored_provider or stored_provider == ARGV[8])
    and (not stored_api or stored_api == ARGV[9])
    and (not stored_prompt or stored_prompt == ARGV[10])
    and (not stored_tools or stored_tools == ARGV[11])
  if same_identity then
    generation = tonumber(redis.call('HGET', KEYS[1], 'generation'))
    bound_at_turn = redis.call('HGET', KEYS[1], 'bound_at_turn')
    outcome = 2
  elseif ARGV[14] ~= '1' then
    return 4
  else
    generation = tonumber(redis.call('HGET', KEYS[1], 'generation')) + 1
    outcome = 3
  end
end
redis.call('HSET', KEYS[1],
  'tenant_key', ARGV[1], 'binding_key', ARGV[2], 'task_key', ARGV[3],
  'conversation_key', ARGV[4], 'branch_key', ARGV[5], 'tier', ARGV[6],
  'model', ARGV[7], 'provider', ARGV[8], 'api', ARGV[9],
  'prompt_profile_hash', ARGV[10], 'toolset_hash', ARGV[11],
  'bound_at_turn', bound_at_turn, 'last_seen_turn', ARGV[13],
  'agent_harness', ARGV[15],
  'generation', generation)
redis.call('EXPIRE', KEYS[1], ARGV[16])
redis.call('SADD', KEYS[2], KEYS[1])
redis.call('EXPIRE', KEYS[2], ARGV[16])
return outcome
";
        let binding_key = self.binding_key(&binding.binding_key);
        let tenant_index = self.tenant_index_key(&binding.tenant_key);
        let mut connection = self.connection.clone();
        let outcome: i64 = Script::new(CAS)
            .key(binding_key)
            .key(tenant_index)
            .key(self.tenant_generation_key(&binding.tenant_key))
            .key(self.task_generation_key(&binding.task_key))
            .arg(&binding.tenant_key)
            .arg(&binding.binding_key)
            .arg(&binding.task_key)
            .arg(binding.conversation_key.as_deref().unwrap_or_default())
            .arg(binding.branch_key.as_deref().unwrap_or_default())
            .arg(&binding.tier)
            .arg(binding.model.as_str())
            .arg(&binding.provider)
            .arg(&binding.api)
            .arg(binding.prompt_profile_hash.as_deref().unwrap_or_default())
            .arg(binding.toolset_hash.as_deref().unwrap_or_default())
            .arg(binding.bound_at_turn.as_deref().unwrap_or_default())
            .arg(binding.last_seen_turn.as_deref().unwrap_or_default())
            .arg(i64::from(allow_migration))
            .arg(binding.agent_harness.as_deref().unwrap_or_default())
            .arg(self.ttl_seconds)
            .arg(binding.tenant_generation)
            .arg(binding.task_generation)
            .invoke_async(&mut connection)
            .await?;
        match outcome {
            1 => Ok(BindingWrite::Created),
            2 => Ok(BindingWrite::Unchanged),
            3 => Ok(BindingWrite::Migrated),
            4 => Ok(BindingWrite::Conflict),
            5 => Err(BindingStoreError::StaleScope),
            value => Err(BindingStoreError::InvalidOutcome(value)),
        }
    }

    async fn scope_generation(
        &self,
        tenant_key: &str,
        task_key: Option<&str>,
    ) -> Result<(u64, u64), BindingStoreError> {
        let mut connection = self.connection.clone();
        let mut keys = vec![self.tenant_generation_key(tenant_key)];
        if let Some(task_key) = task_key {
            keys.push(self.task_generation_key(task_key));
        }
        let generations: Vec<Option<u64>> = redis::cmd("MGET")
            .arg(keys)
            .query_async(&mut connection)
            .await?;
        Ok((
            generations.first().copied().flatten().unwrap_or_default(),
            generations.get(1).copied().flatten().unwrap_or_default(),
        ))
    }

    async fn remove(&self, task_key: &str) -> Result<bool, BindingStoreError> {
        const DELETE: &str = r"
local tenant = redis.call('HGET', KEYS[1], 'tenant_key')
if not tenant then return 0 end
redis.call('DEL', KEYS[1])
redis.call('SREM', ARGV[1] .. ':tenant-bindings:' .. tenant, KEYS[1])
return 1
";
        let mut connection = self.connection.clone();
        let removed: usize = Script::new(DELETE)
            .key(self.binding_key(task_key))
            .arg(&self.prefix)
            .invoke_async(&mut connection)
            .await?;
        Ok(removed > 0)
    }

    async fn remove_task(
        &self,
        tenant_key: &str,
        task_key: &str,
    ) -> Result<usize, BindingStoreError> {
        const DELETE_TASK: &str = r"
local keys = redis.call('SMEMBERS', KEYS[1])
redis.call('INCR', KEYS[2])
local deleted = 0
for _, key in ipairs(keys) do
  if redis.call('HGET', key, 'task_key') == ARGV[1] then
    deleted = deleted + redis.call('DEL', key)
    redis.call('SREM', KEYS[1], key)
  end
end
return deleted
";
        let mut connection = self.connection.clone();
        let deleted = Script::new(DELETE_TASK)
            .key(self.tenant_index_key(tenant_key))
            .key(self.task_generation_key(task_key))
            .arg(task_key)
            .invoke_async(&mut connection)
            .await?;
        Ok(deleted)
    }

    async fn remove_tenant(&self, tenant_key: &str) -> Result<usize, BindingStoreError> {
        const DELETE_TENANT: &str = r"
local keys = redis.call('SMEMBERS', KEYS[1])
redis.call('INCR', KEYS[2])
local deleted = 0
for _, key in ipairs(keys) do
  deleted = deleted + redis.call('DEL', key)
end
redis.call('DEL', KEYS[1])
return deleted
";
        let mut connection = self.connection.clone();
        let deleted = Script::new(DELETE_TENANT)
            .key(self.tenant_index_key(tenant_key))
            .key(self.tenant_generation_key(tenant_key))
            .invoke_async(&mut connection)
            .await?;
        Ok(deleted)
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn binding(task: &str, tenant: &str, model: &str, turn: &str) -> TaskBinding {
        TaskBinding {
            tenant_key: tenant.to_owned(),
            binding_key: task.to_owned(),
            task_key: task.to_owned(),
            conversation_key: None,
            branch_key: None,
            tier: if model.contains("27b") {
                "capable".to_owned()
            } else {
                "efficient".to_owned()
            },
            model: ModelId::new(model).unwrap(),
            provider: model.split('/').next().unwrap_or_default().to_owned(),
            api: "open_ai_chat".to_owned(),
            agent_harness: Some("aionui".to_owned()),
            prompt_profile_hash: None,
            toolset_hash: None,
            bound_at_turn: Some(turn.to_owned()),
            last_seen_turn: Some(turn.to_owned()),
            generation: 1,
            tenant_generation: 0,
            task_generation: 0,
        }
    }

    #[tokio::test]
    async fn memory_generation_rejects_write_crossing_task_deletion() {
        let store = MemoryTaskBindingRepository::new(10);
        let mut stale = binding("task-generation", "tenant-a", "provider/model", "turn-a");
        let generation = store
            .scope_generation("tenant-a", Some("task-generation"))
            .await
            .unwrap();
        stale.tenant_generation = generation.0;
        stale.task_generation = generation.1;

        assert_eq!(
            store
                .remove_task("tenant-a", "task-generation")
                .await
                .unwrap(),
            0
        );
        assert!(matches!(
            store.put(stale.clone(), false).await,
            Err(BindingStoreError::StaleScope)
        ));

        let generation = store
            .scope_generation("tenant-a", Some("task-generation"))
            .await
            .unwrap();
        stale.tenant_generation = generation.0;
        stale.task_generation = generation.1;
        assert_eq!(
            store.put(stale, false).await.unwrap(),
            BindingWrite::Created
        );
    }

    #[tokio::test]
    #[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
    async fn redis_cas_is_shared_atomic_and_tenant_scoped() {
        let url = std::env::var("UROUTER_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379/".to_owned());
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!("urouter-test-{unique}");
        let first = RedisTaskBindingRepository::connect(&url, prefix.clone(), 30)
            .await
            .unwrap();
        let second = RedisTaskBindingRepository::connect(&url, prefix.clone(), 30)
            .await
            .unwrap();
        let task = "task-shared";
        let tenant = "tenant-a";
        let efficient = binding(task, tenant, "local-vllm/qwen3.5-4b", "turn-a");
        let capable = binding(task, tenant, "local-vllm-qwen38/qwen3.8-27b", "turn-b");
        let (left, right) = tokio::join!(
            first.put(efficient.clone(), false),
            second.put(capable.clone(), false)
        );
        let outcomes = [left.unwrap(), right.unwrap()];
        assert!(outcomes.contains(&BindingWrite::Created));
        assert!(outcomes.contains(&BindingWrite::Conflict));

        let winner = second.get(task).await.unwrap().unwrap();
        assert!(winner.model == efficient.model || winner.model == capable.model);
        assert_eq!(winner.generation, 1);
        let loser = if winner.model == efficient.model {
            capable
        } else {
            efficient
        };
        assert_eq!(
            first.put(loser.clone(), true).await.unwrap(),
            BindingWrite::Migrated
        );
        let migrated = second.get(task).await.unwrap().unwrap();
        assert_eq!(migrated.model, loser.model);
        assert_eq!(migrated.generation, 2);
        let restarted = RedisTaskBindingRepository::connect(&url, prefix, 30)
            .await
            .unwrap();
        let recovered = restarted.get(task).await.unwrap().unwrap();
        assert_eq!(recovered.model, loser.model);
        assert_eq!(recovered.generation, 2);

        let mut session = binding(
            "session-shared",
            tenant,
            "local-vllm/qwen3.5-4b",
            "turn-session",
        );
        session.binding_key = "session-shared".to_owned();
        session.task_key = "task-owner".to_owned();
        session.conversation_key = Some("conversation-hash".to_owned());
        session.branch_key = Some("branch-hash".to_owned());
        session.prompt_profile_hash = Some("prompt-hash".to_owned());
        session.toolset_hash = Some("toolset-hash".to_owned());
        assert_eq!(
            first.put(session.clone(), false).await.unwrap(),
            BindingWrite::Created
        );
        assert!(second.get("session-shared").await.unwrap().is_some());
        assert_eq!(first.remove_task(tenant, "task-owner").await.unwrap(), 1);
        assert!(second.get("session-shared").await.unwrap().is_none());
        assert!(matches!(
            second.put(session.clone(), false).await,
            Err(BindingStoreError::StaleScope)
        ));
        let generation = second
            .scope_generation(tenant, Some("task-owner"))
            .await
            .unwrap();
        session.tenant_generation = generation.0;
        session.task_generation = generation.1;
        assert_eq!(
            second.put(session, false).await.unwrap(),
            BindingWrite::Created
        );

        first
            .put(
                binding("task-other", "tenant-b", "local-vllm/qwen3.5-4b", "turn"),
                false,
            )
            .await
            .unwrap();
        assert_eq!(restarted.remove_tenant(tenant).await.unwrap(), 2);
        assert!(first.get(task).await.unwrap().is_none());
        assert!(second.get("task-other").await.unwrap().is_some());
        second.remove_tenant("tenant-b").await.unwrap();
    }
}
