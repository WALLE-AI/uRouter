use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

use redis::{
    AsyncCommands, Script,
    aio::{ConnectionManager, ConnectionManagerConfig},
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{DecisionRecord, FeedbackSignal};

#[derive(Debug, Error)]
pub(crate) enum SharedStateError {
    #[error("shared state backend failed: {0}")]
    Backend(#[from] redis::RedisError),
    #[error("shared state serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("shared state write crossed a deletion boundary")]
    StaleScope,
}

#[derive(Clone)]
pub(crate) struct RedisSharedState {
    connection: ConnectionManager,
    prefix: String,
}

impl RedisSharedState {
    pub(crate) async fn connect(url: &str, prefix: String) -> Result<Self, SharedStateError> {
        let client = redis::Client::open(url)?;
        Ok(Self {
            connection: ConnectionManager::new_with_config(client, redis_connection_config())
                .await?,
            prefix,
        })
    }

    pub(crate) async fn put_record(
        &self,
        record: &DecisionRecord,
        capacity: usize,
    ) -> Result<(), SharedStateError> {
        const PUT: &str = r"
local tenant_generation = tonumber(redis.call('GET', KEYS[3]) or '0')
local task_generation = tonumber(redis.call('GET', KEYS[4]) or '0')
if tenant_generation ~= tonumber(ARGV[6]) or task_generation ~= tonumber(ARGV[7]) then return 0 end
redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
redis.call('ZADD', KEYS[2], ARGV[3], KEYS[1])
redis.call('PEXPIRE', KEYS[2], ARGV[4])
local overflow = redis.call('ZCARD', KEYS[2]) - tonumber(ARGV[5])
if overflow > 0 then
  local old = redis.call('ZRANGE', KEYS[2], 0, overflow - 1)
  for _, key in ipairs(old) do redis.call('DEL', key) end
  redis.call('ZREMRANGEBYRANK', KEYS[2], 0, overflow - 1)
end
return 1
";
        let now_s = unix_seconds();
        let ttl_ms = record
            .expires_at_unix_s
            .saturating_sub(now_s)
            .max(1)
            .saturating_mul(1_000);
        let mut connection = self.connection.clone();
        let written = Script::new(PUT)
            .key(self.record_key(&record.tenant_key, &record.decision_id))
            .key(self.record_index_key(&record.tenant_key))
            .key(self.tenant_generation_key(&record.tenant_key))
            .key(self.task_generation_key(record.task_key.as_deref().unwrap_or_default()))
            .arg(serde_json::to_string(record)?)
            .arg(ttl_ms)
            .arg(decision_score(&record.decision_id))
            .arg(366_u64 * 24 * 60 * 60 * 1_000)
            .arg(capacity)
            .arg(record.tenant_generation)
            .arg(record.task_generation)
            .invoke_async::<i64>(&mut connection)
            .await?;
        if written == 0 {
            return Err(SharedStateError::StaleScope);
        }
        Ok(())
    }

    pub(crate) async fn records(
        &self,
        tenant_key: &str,
    ) -> Result<Vec<DecisionRecord>, SharedStateError> {
        let index = self.record_index_key(tenant_key);
        let mut connection = self.connection.clone();
        let keys: Vec<String> = connection.zrange(&index, 0, -1).await?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let values: Vec<Option<String>> = redis::cmd("MGET")
            .arg(&keys)
            .query_async(&mut connection)
            .await?;
        let mut stale = Vec::new();
        let mut records = Vec::new();
        for (key, value) in keys.into_iter().zip(values) {
            match value {
                Some(value) => records.push(serde_json::from_str(&value)?),
                None => stale.push(key),
            }
        }
        if !stale.is_empty() {
            redis::cmd("ZREM")
                .arg(index)
                .arg(stale)
                .query_async::<i64>(&mut connection)
                .await?;
        }
        Ok(records)
    }

    pub(crate) async fn record(
        &self,
        tenant_key: &str,
        decision_id: &str,
    ) -> Result<Option<DecisionRecord>, SharedStateError> {
        let mut connection = self.connection.clone();
        let value: Option<String> = connection
            .get(self.record_key(tenant_key, decision_id))
            .await?;
        value
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(SharedStateError::from)
    }

    pub(crate) async fn delete_records(
        &self,
        tenant_key: &str,
        decision_ids: &[String],
    ) -> Result<usize, SharedStateError> {
        const DELETE: &str = r"
local deleted = 0
for index = 2, #KEYS do
  deleted = deleted + redis.call('DEL', KEYS[index])
  redis.call('ZREM', KEYS[1], KEYS[index])
end
return deleted
";
        if decision_ids.is_empty() {
            return Ok(0);
        }
        let script = Script::new(DELETE);
        let mut invocation = script.prepare_invoke();
        invocation.key(self.record_index_key(tenant_key));
        for decision_id in decision_ids {
            invocation.key(self.record_key(tenant_key, decision_id));
        }
        let mut connection = self.connection.clone();
        invocation
            .invoke_async(&mut connection)
            .await
            .map_err(SharedStateError::from)
    }

    pub(crate) async fn upsert_feedback(
        &self,
        tenant_key: &str,
        turn: &str,
        signals: &[FeedbackSignal],
        ttl_seconds: u64,
    ) -> Result<Vec<FeedbackSignal>, SharedStateError> {
        const UPSERT: &str = r"
for index = 2, #ARGV, 2 do redis.call('HSET', KEYS[1], ARGV[index], ARGV[index + 1]) end
redis.call('EXPIRE', KEYS[1], ARGV[1])
redis.call('SADD', KEYS[2], KEYS[1])
redis.call('EXPIRE', KEYS[2], 31622400)
return 1
";
        let feedback_key = self.feedback_key(tenant_key, turn);
        let script = Script::new(UPSERT);
        let mut invocation = script.prepare_invoke();
        invocation
            .key(&feedback_key)
            .key(self.feedback_index_key(tenant_key))
            .arg(ttl_seconds.max(1));
        for signal in signals {
            invocation
                .arg(&signal.kind)
                .arg(serde_json::to_string(signal)?);
        }
        let mut connection = self.connection.clone();
        invocation.invoke_async::<i64>(&mut connection).await?;
        self.feedback(tenant_key, turn)
            .await
            .map(Option::unwrap_or_default)
    }

    pub(crate) async fn feedback(
        &self,
        tenant_key: &str,
        turn: &str,
    ) -> Result<Option<Vec<FeedbackSignal>>, SharedStateError> {
        let mut connection = self.connection.clone();
        let values: BTreeMap<String, String> = connection
            .hgetall(self.feedback_key(tenant_key, turn))
            .await?;
        if values.is_empty() {
            return Ok(None);
        }
        values
            .into_values()
            .map(|value| serde_json::from_str(&value).map_err(SharedStateError::from))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    pub(crate) async fn feedback_many(
        &self,
        tenant_key: &str,
        turns: &[String],
    ) -> Result<BTreeMap<String, Vec<FeedbackSignal>>, SharedStateError> {
        if turns.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut pipeline = redis::pipe();
        for turn in turns {
            pipeline.hgetall(self.feedback_key(tenant_key, turn));
        }
        let mut connection = self.connection.clone();
        let values: Vec<BTreeMap<String, String>> = pipeline.query_async(&mut connection).await?;
        turns
            .iter()
            .cloned()
            .zip(values)
            .filter(|(_, values)| !values.is_empty())
            .map(|(turn, values)| {
                values
                    .into_values()
                    .map(|value| serde_json::from_str(&value).map_err(SharedStateError::from))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|signals| (turn, signals))
            })
            .collect()
    }

    pub(crate) async fn delete_feedback(
        &self,
        tenant_key: &str,
        turns: &[String],
    ) -> Result<usize, SharedStateError> {
        const DELETE: &str = r"
local deleted = 0
for index = 2, #KEYS do
  deleted = deleted + redis.call('DEL', KEYS[index])
  redis.call('SREM', KEYS[1], KEYS[index])
end
return deleted
";
        if turns.is_empty() {
            return Ok(0);
        }
        let script = Script::new(DELETE);
        let mut invocation = script.prepare_invoke();
        invocation.key(self.feedback_index_key(tenant_key));
        for turn in turns {
            invocation.key(self.feedback_key(tenant_key, turn));
        }
        let mut connection = self.connection.clone();
        invocation
            .invoke_async(&mut connection)
            .await
            .map_err(SharedStateError::from)
    }

    fn record_index_key(&self, tenant_key: &str) -> String {
        format!("{}:records:{tenant_key}", self.prefix)
    }

    fn record_key(&self, tenant_key: &str, decision_id: &str) -> String {
        format!(
            "{}:record:{:x}",
            self.prefix,
            Sha256::digest(format!("{tenant_key}\0{decision_id}").as_bytes())
        )
    }

    fn feedback_index_key(&self, tenant_key: &str) -> String {
        format!("{}:feedback-index:{tenant_key}", self.prefix)
    }

    fn feedback_key(&self, tenant_key: &str, turn: &str) -> String {
        format!(
            "{}:feedback:{:x}",
            self.prefix,
            Sha256::digest(format!("{tenant_key}\0{turn}").as_bytes())
        )
    }

    fn tenant_generation_key(&self, tenant_key: &str) -> String {
        format!("{}:tenant-generation:{tenant_key}", self.prefix)
    }

    fn task_generation_key(&self, task_key: &str) -> String {
        format!("{}:task-generation:{task_key}", self.prefix)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn redis_connection_config() -> ConnectionManagerConfig {
    ConnectionManagerConfig::new()
        .set_response_timeout(std::time::Duration::from_secs(2))
        .set_connection_timeout(std::time::Duration::from_secs(1))
        .set_max_delay(500)
        .set_number_of_retries(3)
}

fn decision_score(decision_id: &str) -> u64 {
    decision_id
        .strip_prefix("dec_")
        .and_then(|value| u128::from_str_radix(value, 16).ok())
        .and_then(|value| u64::try_from(value / 1_000).ok())
        .unwrap_or_else(|| unix_seconds().saturating_mul(1_000_000))
}
