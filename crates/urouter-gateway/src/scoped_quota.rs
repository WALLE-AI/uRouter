//! The multi-scope quota ledger.
//!
//! ## Relationship to [`crate::quota`]
//!
//! Two layers, deliberately independent, both of which a request must pass:
//!
//! * [`crate::quota`] is the process-global tenant guard driven by the
//!   `--tenant-*` flags. Its semantics and its three error codes are pinned by
//!   existing tests and by clients in the wild; it is left exactly as it was.
//! * This module is the declarative ledger driven by `route.json`'s
//!   `quota_limits`. It is what adds day windows and the provider / credential /
//!   deployment scopes that free-tier and shared-key accounting need.
//!
//! ## What is pure and what is not
//!
//! The decision — which counters this request draws down, in which order, and
//! whether it fits — lives entirely in `urouter_contracts::quota`. Both backends
//! here execute the same [`QuotaChargePlan`]; the memory one calls
//! [`evaluate_quota_plan`] directly and the Redis one transliterates it into
//! Lua. `plan_and_evaluate_agree_across_backends` is what makes writing the
//! predicate twice safe.
//!
//! ## Window shapes
//!
//! * **Minute** windows are sliding (a ZSET keyed by timestamp). A fixed minute
//!   bucket would let a caller land 2x the limit across a boundary, which is
//!   precisely the burst that trips the upstream's own limiter — the thing this
//!   ledger exists to avoid.
//! * **Day** windows are a bucketed counter with a TTL, not a trailing 24
//!   hours. A daily cap resets at 00:00 UTC, and `day_bucket` from the pure core
//!   *is* UTC midnight. It is also O(1) and self-cleaning, where a day-long ZSET
//!   at provider scope would hold one member per request.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use redis::{Script, aio::ConnectionManager};
use sha2::{Digest, Sha256};
use urouter_contracts::{
    QuotaBreach, QuotaChargePlan, QuotaCounter, QuotaDimension, QuotaObservation, QuotaScopeKind,
    QuotaWindow, evaluate_quota_plan,
};

use crate::quota::QuotaError;

/// What one reservation attempt produced.
///
/// The observation is returned on BOTH paths. That is what makes the
/// `quota_usage_millis` feedback free: no second round trip, and no separate
/// observation path that could drift from the enforcement path.
pub(crate) struct ScopedQuotaOutcome {
    pub(crate) result: Result<ScopedQuotaPermit, QuotaBreach>,
    pub(crate) observation: QuotaObservation,
}

/// Handle to an accepted reservation, used to settle or release it.
#[derive(Debug, Clone)]
pub(crate) struct ScopedQuotaPermit {
    token: String,
    plan: Arc<QuotaChargePlan>,
}

impl ScopedQuotaPermit {
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.plan.is_empty()
    }

    /// A permit that charges nothing, for the "no limits configured" and
    /// "backend unavailable, fail open" paths.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self {
            token: String::new(),
            plan: Arc::new(QuotaChargePlan {
                schema_version: urouter_contracts::QUOTA_SCHEMA_VERSION,
                charges: Vec::new(),
            }),
        }
    }
}

#[async_trait]
pub(crate) trait ScopedQuotaRepository: Send + Sync {
    /// Evaluate every charge, then apply all of them or none.
    async fn reserve(&self, plan: Arc<QuotaChargePlan>) -> Result<ScopedQuotaOutcome, QuotaError>;
    /// Replace the reserved token amounts with what was actually consumed.
    async fn settle(&self, permit: &ScopedQuotaPermit, actual_tokens: u64)
    -> Result<(), QuotaError>;
    /// Give back the concurrency slot and the token reservation.
    async fn release(&self, permit: ScopedQuotaPermit) -> Result<(), QuotaError>;
    fn backend_name(&self) -> &'static str;
}

/// RAII wrapper mirroring [`crate::quota::QuotaLease`].
///
/// The `Drop` impl matters more here than for the tenant lease: this
/// reservation is taken INSIDE the tier loop while a capacity lease and a
/// circuit permit are already held, and every `continue` in that loop must give
/// all three back. A leaked concurrency slot is only cleared by a restart.
pub(crate) struct ScopedQuotaLease {
    repository: Arc<dyn ScopedQuotaRepository>,
    permit: Option<ScopedQuotaPermit>,
}

impl ScopedQuotaLease {
    pub(crate) const fn new(
        repository: Arc<dyn ScopedQuotaRepository>,
        permit: ScopedQuotaPermit,
    ) -> Self {
        Self {
            repository,
            permit: Some(permit),
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

impl Drop for ScopedQuotaLease {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        if permit.is_empty() {
            return;
        }
        let repository = Arc::clone(&self.repository);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = repository.release(permit).await;
            });
        }
    }
}

/// Stable identity of one counter, independent of backend.
fn counter_key(
    scope: QuotaScopeKind,
    scope_id: &str,
    window: QuotaWindow,
    dimension: QuotaDimension,
    day_bucket: Option<u64>,
) -> String {
    let day = day_bucket.map_or_else(String::new, |bucket| format!(":{bucket}"));
    format!(
        "{}:{scope_id}:{}:{}{day}",
        scope.as_str(),
        window.as_str(),
        dimension.as_str()
    )
}

// ── memory backend ──────────────────────────────────────────────────────────

pub(crate) trait ScopedQuotaClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub(crate) struct SystemScopedQuotaClock;

impl ScopedQuotaClock for SystemScopedQuotaClock {
    fn now_ms(&self) -> u64 {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        u64::try_from(millis).unwrap_or(u64::MAX)
    }
}

#[derive(Default)]
struct MemoryScopedState {
    /// Sliding minute windows and in-flight sets: token -> expiry.
    events: BTreeMap<String, VecDeque<(u64, String)>>,
    /// Per-token amounts, so a settle can replace a reservation.
    amounts: BTreeMap<(String, String), u64>,
    /// Running total per counter.
    totals: BTreeMap<String, u64>,
    /// In-flight tokens per counter, each with the deadline at which it ages
    /// out.
    ///
    /// The expiry is not decoration. A permit whose holder died — a panicked
    /// task, a killed process mid-request — would otherwise occupy a
    /// concurrency slot until restart. The Redis backend gets this from the
    /// ZSET score it already writes; the memory backend has to carry it too, or
    /// the two disagree about a failure mode that only shows up in production.
    in_flight: BTreeMap<String, BTreeMap<String, u64>>,
}

pub(crate) struct MemoryScopedQuotaRepository {
    ticket: AtomicU64,
    clock: Arc<dyn ScopedQuotaClock>,
    lease_ttl_ms: u64,
    state: Mutex<MemoryScopedState>,
}

impl MemoryScopedQuotaRepository {
    #[must_use]
    pub(crate) fn new(lease_ttl: Duration) -> Arc<Self> {
        Self::with_clock(lease_ttl, Arc::new(SystemScopedQuotaClock))
    }

    #[must_use]
    pub(crate) fn with_clock(
        lease_ttl: Duration,
        clock: Arc<dyn ScopedQuotaClock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            ticket: AtomicU64::new(0),
            clock,
            lease_ttl_ms: u64::try_from(lease_ttl.as_millis()).unwrap_or(u64::MAX),
            state: Mutex::new(MemoryScopedState::default()),
        })
    }

    /// Drop expired members of every sliding window and subtract their amounts,
    /// and age out in-flight permits past their lease deadline.
    fn prune(state: &mut MemoryScopedState, now_ms: u64) {
        state.in_flight.retain(|_, tokens| {
            tokens.retain(|_, expires_at| *expires_at > now_ms);
            !tokens.is_empty()
        });
        let keys: Vec<String> = state.events.keys().cloned().collect();
        for key in keys {
            let mut expired = Vec::new();
            if let Some(queue) = state.events.get_mut(&key) {
                while queue
                    .front()
                    .is_some_and(|(expires_at, _)| *expires_at <= now_ms)
                {
                    let (_, token) = queue.pop_front().expect("front exists");
                    expired.push(token);
                }
            }
            for token in expired {
                let amount = state.amounts.remove(&(key.clone(), token)).unwrap_or(0);
                if let Some(total) = state.totals.get_mut(&key) {
                    *total = total.saturating_sub(amount);
                }
            }
        }
    }

    fn observe(state: &MemoryScopedState, plan: &QuotaChargePlan, now_ms: u64) -> QuotaObservation {
        let counters = plan
            .charges
            .iter()
            .map(|charge| {
                let key = counter_key(
                    charge.scope,
                    &charge.scope_id,
                    charge.window,
                    charge.dimension,
                    charge.day_bucket,
                );
                let used = if charge.window == QuotaWindow::InFlight {
                    u64::try_from(state.in_flight.get(&key).map_or(0, BTreeMap::len))
                        .unwrap_or(u64::MAX)
                } else {
                    state.totals.get(&key).copied().unwrap_or(0)
                };
                QuotaCounter {
                    scope: charge.scope,
                    window: charge.window,
                    dimension: charge.dimension,
                    used,
                    limit: charge.limit,
                    reset_millis: charge.reset_millis(now_ms),
                }
            })
            .collect();
        QuotaObservation {
            schema_version: plan.schema_version,
            counters,
        }
    }
}

#[async_trait]
impl ScopedQuotaRepository for MemoryScopedQuotaRepository {
    async fn reserve(&self, plan: Arc<QuotaChargePlan>) -> Result<ScopedQuotaOutcome, QuotaError> {
        if plan.is_empty() {
            return Ok(ScopedQuotaOutcome {
                result: Ok(ScopedQuotaPermit {
                    token: String::new(),
                    plan,
                }),
                observation: QuotaObservation::default(),
            });
        }
        let now_ms = self.clock.now_ms();
        let mut state = self.state.lock().map_err(|_| QuotaError::LockPoisoned)?;
        Self::prune(&mut state, now_ms);

        // Evaluate the WHOLE plan before writing anything. A breach on the last
        // charge must leave every earlier counter untouched.
        let observation = Self::observe(&state, &plan, now_ms);
        if let Err(breach) = evaluate_quota_plan(&plan, &observation) {
            return Ok(ScopedQuotaOutcome {
                result: Err(with_observed_reset(breach, &observation)),
                observation,
            });
        }

        let token = self.ticket.fetch_add(1, Ordering::Relaxed).to_string();
        for charge in &plan.charges {
            let key = counter_key(
                charge.scope,
                &charge.scope_id,
                charge.window,
                charge.dimension,
                charge.day_bucket,
            );
            match charge.window {
                QuotaWindow::InFlight => {
                    state
                        .in_flight
                        .entry(key)
                        .or_default()
                        .insert(token.clone(), now_ms.saturating_add(self.lease_ttl_ms));
                }
                QuotaWindow::Minute => {
                    state
                        .events
                        .entry(key.clone())
                        .or_default()
                        .push_back((now_ms.saturating_add(charge.window_millis), token.clone()));
                    state
                        .amounts
                        .insert((key.clone(), token.clone()), charge.amount);
                    *state.totals.entry(key).or_default() += charge.amount;
                }
                QuotaWindow::Day => {
                    // A day bucket is not pruned by a sliding window; it simply
                    // stops being addressed once the bucket rolls.
                    state
                        .amounts
                        .insert((key.clone(), token.clone()), charge.amount);
                    *state.totals.entry(key).or_default() += charge.amount;
                }
            }
        }
        let observation = Self::observe(&state, &plan, now_ms);
        Ok(ScopedQuotaOutcome {
            result: Ok(ScopedQuotaPermit { token, plan }),
            observation,
        })
    }

    async fn settle(
        &self,
        permit: &ScopedQuotaPermit,
        actual_tokens: u64,
    ) -> Result<(), QuotaError> {
        if permit.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| QuotaError::LockPoisoned)?;
        for charge in permit
            .plan
            .charges
            .iter()
            .filter(|charge| charge.dimension == QuotaDimension::Tokens)
        {
            let key = counter_key(
                charge.scope,
                &charge.scope_id,
                charge.window,
                charge.dimension,
                charge.day_bucket,
            );
            let entry = (key.clone(), permit.token.clone());
            // Only adjust a reservation that is still live: one whose window has
            // already rolled is no longer part of any total, and re-subtracting
            // it would corrupt the counter.
            if let Some(previous) = state.amounts.get(&entry).copied() {
                state.amounts.insert(entry, actual_tokens);
                if let Some(total) = state.totals.get_mut(&key) {
                    *total = total.saturating_sub(previous).saturating_add(actual_tokens);
                }
            }
        }
        Ok(())
    }

    async fn release(&self, permit: ScopedQuotaPermit) -> Result<(), QuotaError> {
        if permit.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| QuotaError::LockPoisoned)?;
        for charge in &permit.plan.charges {
            let key = counter_key(
                charge.scope,
                &charge.scope_id,
                charge.window,
                charge.dimension,
                charge.day_bucket,
            );
            if charge.window == QuotaWindow::InFlight
                && let Some(tokens) = state.in_flight.get_mut(&key)
            {
                tokens.remove(&permit.token);
                if tokens.is_empty() {
                    state.in_flight.remove(&key);
                }
            }
            // Minute and day counters are NOT given back on release: a request
            // that was made still consumed the provider's request budget even if
            // it failed. Only the token reservation is corrected, by `settle`.
        }
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }
}

// ── Redis backend ───────────────────────────────────────────────────────────

/// Reserve against a variable-length charge list, atomically.
///
/// The two-pass shape — read every counter, decide, only then write — is the
/// all-or-nothing property, and it is why this cannot be a sequence of
/// individual commands from the client.
const RESERVE_PLAN: &str = r"
local now = redis.call('TIME')
local now_ms = tonumber(now[1]) * 1000 + math.floor(tonumber(now[2]) / 1000)
local count = tonumber(ARGV[1])
local token = ARGV[2]
local used = {}

-- pass 1: prune expired members of every sliding window, then read usage
for i = 0, count - 1 do
  local base = 3 + i * 5
  local window = tonumber(ARGV[base])
  local window_ms = tonumber(ARGV[base + 3])
  local events = KEYS[i * 3 + 1]
  local amounts = KEYS[i * 3 + 2]
  local total = KEYS[i * 3 + 3]
  if window == 0 then
    redis.call('ZREMRANGEBYSCORE', events, '-inf', now_ms)
    used[i + 1] = redis.call('ZCARD', events)
  elseif window == 1 then
    local expired = redis.call('ZRANGEBYSCORE', events, '-inf', now_ms - window_ms)
    for _, member in ipairs(expired) do
      local amount = tonumber(redis.call('HGET', amounts, member) or '0')
      if amount > 0 then redis.call('DECRBY', total, amount) end
      redis.call('HDEL', amounts, member)
    end
    redis.call('ZREMRANGEBYSCORE', events, '-inf', now_ms - window_ms)
    used[i + 1] = tonumber(redis.call('GET', total) or '0')
  else
    used[i + 1] = tonumber(redis.call('GET', total) or '0')
  end
end

-- pass 2: decide, before any write
for i = 0, count - 1 do
  local base = 3 + i * 5
  local limit = tonumber(ARGV[base + 1])
  local amount = tonumber(ARGV[base + 2])
  if used[i + 1] + amount > limit then
    return {0, i, cjson.encode(used)}
  end
end

-- pass 3: apply
for i = 0, count - 1 do
  local base = 3 + i * 5
  local window = tonumber(ARGV[base])
  local amount = tonumber(ARGV[base + 2])
  local window_ms = tonumber(ARGV[base + 3])
  local ttl_ms = tonumber(ARGV[base + 4])
  local events = KEYS[i * 3 + 1]
  local amounts = KEYS[i * 3 + 2]
  local total = KEYS[i * 3 + 3]
  if window == 0 then
    redis.call('ZADD', events, now_ms + ttl_ms, token)
    redis.call('PEXPIRE', events, ttl_ms * 2)
  else
    if window == 1 then
      redis.call('ZADD', events, now_ms, token)
      redis.call('PEXPIRE', events, window_ms * 2)
    end
    redis.call('HSET', amounts, token, amount)
    redis.call('INCRBY', total, amount)
    redis.call('PEXPIRE', amounts, ttl_ms)
    redis.call('PEXPIRE', total, ttl_ms)
  end
  used[i + 1] = used[i + 1] + amount
end
return {1, -1, cjson.encode(used)}
";

/// Replace a reserved token amount with the actual one.
const SETTLE_PLAN: &str = r"
local count = tonumber(ARGV[1])
local token = ARGV[2]
local actual = tonumber(ARGV[3])
for i = 0, count - 1 do
  local amounts = KEYS[i * 2 + 1]
  local total = KEYS[i * 2 + 2]
  local previous = redis.call('HGET', amounts, token)
  if previous then
    redis.call('INCRBY', total, actual - tonumber(previous))
    redis.call('HSET', amounts, token, actual)
  end
end
return 1
";

/// Give back concurrency slots. Request and token counters are not refunded:
/// the call was made, and the upstream counted it.
const RELEASE_PLAN: &str = r"
local count = tonumber(ARGV[1])
local token = ARGV[2]
for i = 0, count - 1 do
  redis.call('ZREM', KEYS[i + 1], token)
end
return 1
";

pub(crate) struct RedisScopedQuotaRepository {
    connection: ConnectionManager,
    prefix: String,
    lease_ttl_ms: u64,
    ticket: AtomicU64,
}

impl RedisScopedQuotaRepository {
    pub(crate) async fn connect(
        url: &str,
        prefix: String,
        lease_ttl: Duration,
    ) -> Result<Arc<Self>, QuotaError> {
        let client = redis::Client::open(url)?;
        Ok(Arc::new(Self {
            connection: ConnectionManager::new(client).await?,
            prefix,
            lease_ttl_ms: u64::try_from(lease_ttl.as_millis()).unwrap_or(u64::MAX),
            ticket: AtomicU64::new(0),
        }))
    }

    /// The three Redis keys backing one charge.
    ///
    /// `v2` in the prefix means the previous key space coexists rather than
    /// being migrated: old keys age out on their own TTLs, and a rollback reads
    /// state that is at most one window stale.
    fn charge_keys(&self, charge: &urouter_contracts::QuotaCharge) -> [String; 3] {
        let digest = format!("{:x}", Sha256::digest(charge.scope_id.as_bytes()));
        let day = charge
            .day_bucket
            .map_or_else(String::new, |bucket| format!(":{bucket}"));
        let base = format!(
            "{}:quota:v2:{}:{digest}:{}:{}{day}",
            self.prefix,
            charge.scope.as_str(),
            charge.window.as_str(),
            charge.dimension.as_str()
        );
        [
            format!("{base}:events"),
            format!("{base}:amounts"),
            format!("{base}:total"),
        ]
    }
}

#[async_trait]
impl ScopedQuotaRepository for RedisScopedQuotaRepository {
    async fn reserve(&self, plan: Arc<QuotaChargePlan>) -> Result<ScopedQuotaOutcome, QuotaError> {
        if plan.is_empty() {
            return Ok(ScopedQuotaOutcome {
                result: Ok(ScopedQuotaPermit {
                    token: String::new(),
                    plan,
                }),
                observation: QuotaObservation::default(),
            });
        }
        let token = format!(
            "{}-{}",
            self.ticket.fetch_add(1, Ordering::Relaxed),
            plan.charges.len()
        );
        let script = Script::new(RESERVE_PLAN);
        let mut invocation = script.prepare_invoke();
        for charge in &plan.charges {
            for key in self.charge_keys(charge) {
                invocation.key(key);
            }
        }
        invocation.arg(plan.charges.len()).arg(token.clone());
        for charge in &plan.charges {
            let window_code = match charge.window {
                QuotaWindow::InFlight => 0_u8,
                QuotaWindow::Minute => 1,
                QuotaWindow::Day => 2,
            };
            let ttl = match charge.window {
                QuotaWindow::InFlight => self.lease_ttl_ms,
                // Two windows' worth: long enough that a sliding minute window
                // never loses live members, and long enough that yesterday's
                // day bucket disappears on its own without a sweeper.
                QuotaWindow::Minute | QuotaWindow::Day => {
                    charge.window_millis.saturating_mul(2)
                }
            };
            invocation
                .arg(window_code)
                .arg(charge.limit)
                .arg(charge.amount)
                .arg(charge.window_millis)
                .arg(ttl);
        }
        let mut connection = self.connection.clone();
        let (granted, breach_index, used_json): (i64, i64, String) =
            invocation.invoke_async(&mut connection).await?;
        let used: Vec<u64> = serde_json::from_str(&used_json).unwrap_or_default();
        let observation = observation_from(&plan, &used, SystemScopedQuotaClock.now_ms());
        if granted == 1 {
            return Ok(ScopedQuotaOutcome {
                result: Ok(ScopedQuotaPermit { token, plan }),
                observation,
            });
        }
        let index = usize::try_from(breach_index).unwrap_or(0);
        let charge = plan
            .charges
            .get(index)
            .ok_or_else(|| QuotaError::InvalidRejection(format!("charge index {index}")))?;
        let breach = QuotaBreach {
            scope: charge.scope,
            window: charge.window,
            dimension: charge.dimension,
            reset_millis: charge.window_millis,
        };
        Ok(ScopedQuotaOutcome {
            result: Err(with_observed_reset(breach, &observation)),
            observation,
        })
    }

    async fn settle(
        &self,
        permit: &ScopedQuotaPermit,
        actual_tokens: u64,
    ) -> Result<(), QuotaError> {
        let token_charges: Vec<_> = permit
            .plan
            .charges
            .iter()
            .filter(|charge| charge.dimension == QuotaDimension::Tokens)
            .collect();
        if token_charges.is_empty() {
            return Ok(());
        }
        let script = Script::new(SETTLE_PLAN);
        let mut invocation = script.prepare_invoke();
        for charge in &token_charges {
            let keys = self.charge_keys(charge);
            invocation.key(keys[1].clone()).key(keys[2].clone());
        }
        invocation
            .arg(token_charges.len())
            .arg(permit.token.clone())
            .arg(actual_tokens);
        let mut connection = self.connection.clone();
        let _: i64 = invocation.invoke_async(&mut connection).await?;
        Ok(())
    }

    async fn release(&self, permit: ScopedQuotaPermit) -> Result<(), QuotaError> {
        let in_flight: Vec<_> = permit
            .plan
            .charges
            .iter()
            .filter(|charge| charge.window == QuotaWindow::InFlight)
            .collect();
        if in_flight.is_empty() {
            return Ok(());
        }
        let script = Script::new(RELEASE_PLAN);
        let mut invocation = script.prepare_invoke();
        for charge in &in_flight {
            invocation.key(self.charge_keys(charge)[0].clone());
        }
        invocation.arg(in_flight.len()).arg(permit.token.clone());
        let mut connection = self.connection.clone();
        let _: i64 = invocation.invoke_async(&mut connection).await?;
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }
}

/// Replace a breach's window-length placeholder with the counter's real reset.
///
/// `evaluate_quota_plan` is pure and has no clock, so it can only report the
/// window LENGTH. For a daily cap that is wrong in the way that matters: a
/// client told to come back in 24 hours when the bucket rolls in 20 minutes
/// will wait 24 hours. The observation carries the true figure, computed from
/// the day bucket.
fn with_observed_reset(breach: QuotaBreach, observation: &QuotaObservation) -> QuotaBreach {
    observation
        .counters
        .iter()
        .find(|counter| {
            counter.scope == breach.scope
                && counter.window == breach.window
                && counter.dimension == breach.dimension
        })
        .map_or(breach, |counter| QuotaBreach {
            reset_millis: counter.reset_millis,
            ..breach
        })
}

fn observation_from(plan: &QuotaChargePlan, used: &[u64], now_millis: u64) -> QuotaObservation {
    QuotaObservation {
        schema_version: plan.schema_version,
        counters: plan
            .charges
            .iter()
            .enumerate()
            .map(|(index, charge)| QuotaCounter {
                scope: charge.scope,
                window: charge.window,
                dimension: charge.dimension,
                used: used.get(index).copied().unwrap_or(0),
                limit: charge.limit,
                reset_millis: charge.reset_millis(now_millis),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use urouter_contracts::{
        DAY_WINDOW_MILLIS, QuotaLimit, QuotaLimitSet, QuotaRequest, QuotaScopeRef,
        plan_quota_charges, quota_usage_millis,
    };

    struct FixedClock(Mutex<u64>);

    impl FixedClock {
        fn new(now: u64) -> Arc<Self> {
            Arc::new(Self(Mutex::new(now)))
        }

        fn advance(&self, millis: u64) {
            *self.0.lock().unwrap() += millis;
        }
    }

    impl ScopedQuotaClock for FixedClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }

    fn limits(entries: &[(QuotaScopeKind, QuotaWindow, QuotaDimension, u64)]) -> QuotaLimitSet {
        QuotaLimitSet::new(
            entries
                .iter()
                .map(|(scope, window, dimension, limit)| QuotaLimit {
                    scope: *scope,
                    window: *window,
                    dimension: *dimension,
                    limit: *limit,
                })
                .collect(),
        )
    }

    fn request(scopes: &[(QuotaScopeKind, &str)], tokens: u64) -> QuotaRequest {
        QuotaRequest {
            scopes: scopes
                .iter()
                .map(|(kind, id)| QuotaScopeRef {
                    kind: *kind,
                    id: (*id).to_owned(),
                })
                .collect(),
            estimated_tokens: tokens,
        }
    }

    #[tokio::test]
    async fn a_provider_request_cap_binds_across_deployments_sharing_it() {
        let clock = FixedClock::new(0);
        let repository =
            MemoryScopedQuotaRepository::with_clock(Duration::from_secs(60), clock.clone());
        let limits = limits(&[(
            QuotaScopeKind::Provider,
            QuotaWindow::Minute,
            QuotaDimension::Requests,
            2,
        )]);

        // Two different deployments, one shared provider budget.
        let mut leases = Vec::new();
        for deployment in ["d1", "d2"] {
            let plan = Arc::new(plan_quota_charges(
                &limits,
                &request(
                    &[
                        (QuotaScopeKind::Provider, "siliconflow"),
                        (QuotaScopeKind::Deployment, deployment),
                    ],
                    0,
                ),
                clock.now_ms(),
            ));
            let outcome = repository.reserve(plan).await.unwrap();
            leases.push(outcome.result.expect("within the provider cap"));
        }

        let plan = Arc::new(plan_quota_charges(
            &limits,
            &request(&[(QuotaScopeKind::Provider, "siliconflow")], 0),
            clock.now_ms(),
        ));
        let breach = repository
            .reserve(plan)
            .await
            .unwrap()
            .result
            .expect_err("the third call exceeds the shared provider cap");
        assert_eq!(breach.scope, QuotaScopeKind::Provider);
        assert_eq!(breach.reason(), "quota_provider_minute_requests");
    }

    #[tokio::test]
    async fn a_daily_cap_resets_at_the_utc_midnight_boundary() {
        let clock = FixedClock::new(0);
        let repository =
            MemoryScopedQuotaRepository::with_clock(Duration::from_secs(60), clock.clone());
        let limits = limits(&[(
            QuotaScopeKind::Credential,
            QuotaWindow::Day,
            QuotaDimension::Requests,
            1,
        )]);
        let make_plan = |now: u64| {
            Arc::new(plan_quota_charges(
                &limits,
                &request(&[(QuotaScopeKind::Credential, "key-1")], 0),
                now,
            ))
        };

        assert!(
            repository
                .reserve(make_plan(clock.now_ms()))
                .await
                .unwrap()
                .result
                .is_ok()
        );
        assert!(
            repository
                .reserve(make_plan(clock.now_ms()))
                .await
                .unwrap()
                .result
                .is_err(),
            "the daily cap is one request"
        );

        // One millisecond before midnight: still refused.
        clock.advance(DAY_WINDOW_MILLIS - 1);
        assert!(
            repository
                .reserve(make_plan(clock.now_ms()))
                .await
                .unwrap()
                .result
                .is_err()
        );
        // Crossing midnight addresses a new bucket, so the cap is fresh —
        // it does not wait 24 hours from the first request.
        clock.advance(1);
        assert!(
            repository
                .reserve(make_plan(clock.now_ms()))
                .await
                .unwrap()
                .result
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_breach_on_a_later_charge_leaves_earlier_counters_untouched() {
        let clock = FixedClock::new(0);
        let repository =
            MemoryScopedQuotaRepository::with_clock(Duration::from_secs(60), clock.clone());
        // Tenant is roomy; the deployment cap is what fails. Tenant sorts first
        // in plan order, so it is the "earlier charge".
        let limits = limits(&[
            (
                QuotaScopeKind::Tenant,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                100,
            ),
            (
                QuotaScopeKind::Deployment,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                1,
            ),
        ]);
        let plan = || {
            Arc::new(plan_quota_charges(
                &limits,
                &request(
                    &[
                        (QuotaScopeKind::Tenant, "t"),
                        (QuotaScopeKind::Deployment, "d"),
                    ],
                    0,
                ),
                clock.now_ms(),
            ))
        };
        repository.reserve(plan()).await.unwrap().result.unwrap();
        let outcome = repository.reserve(plan()).await.unwrap();
        assert!(outcome.result.is_err());

        // The tenant counter must still read 1, not 2: the refused request must
        // not have consumed the tenant's budget on its way to being refused.
        let tenant = outcome
            .observation
            .counters
            .iter()
            .find(|counter| counter.scope == QuotaScopeKind::Tenant)
            .expect("tenant counter is observed");
        assert_eq!(tenant.used, 1);
    }

    #[tokio::test]
    async fn releasing_returns_concurrency_but_not_the_request_count() {
        let clock = FixedClock::new(0);
        let repository =
            MemoryScopedQuotaRepository::with_clock(Duration::from_secs(60), clock.clone());
        let limits = limits(&[
            (
                QuotaScopeKind::Deployment,
                QuotaWindow::InFlight,
                QuotaDimension::Concurrency,
                1,
            ),
            (
                QuotaScopeKind::Deployment,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                10,
            ),
        ]);
        let plan = || {
            Arc::new(plan_quota_charges(
                &limits,
                &request(&[(QuotaScopeKind::Deployment, "d")], 0),
                clock.now_ms(),
            ))
        };
        let permit = repository.reserve(plan()).await.unwrap().result.unwrap();
        assert!(repository.reserve(plan()).await.unwrap().result.is_err());

        repository.release(permit).await.unwrap();
        let outcome = repository.reserve(plan()).await.unwrap();
        assert!(
            outcome.result.is_ok(),
            "the concurrency slot must come back"
        );
        // The request counter did not: the call was still made.
        let requests = outcome
            .observation
            .counters
            .iter()
            .find(|counter| counter.dimension == QuotaDimension::Requests)
            .expect("request counter is observed");
        assert_eq!(requests.used, 2);
    }

    #[tokio::test]
    async fn settling_replaces_the_estimate_with_the_real_token_count() {
        let clock = FixedClock::new(0);
        let repository =
            MemoryScopedQuotaRepository::with_clock(Duration::from_secs(60), clock.clone());
        let limits = limits(&[(
            QuotaScopeKind::Provider,
            QuotaWindow::Minute,
            QuotaDimension::Tokens,
            1_000,
        )]);
        let plan = |tokens: u64| {
            Arc::new(plan_quota_charges(
                &limits,
                &request(&[(QuotaScopeKind::Provider, "p")], tokens),
                clock.now_ms(),
            ))
        };
        let permit = repository.reserve(plan(900)).await.unwrap().result.unwrap();
        // 900 reserved: a second 900-token call cannot fit.
        assert!(repository.reserve(plan(900)).await.unwrap().result.is_err());

        // The call actually used 100.
        repository.settle(&permit, 100).await.unwrap();
        let outcome = repository.reserve(plan(900)).await.unwrap();
        assert!(
            outcome.result.is_ok(),
            "settling must free the over-reservation"
        );
    }

    /// The value that replaces the hand-written `quota_usage_millis` constant.
    #[tokio::test]
    async fn the_observation_feeds_the_deployment_usage_signal() {
        let clock = FixedClock::new(0);
        let repository =
            MemoryScopedQuotaRepository::with_clock(Duration::from_secs(60), clock.clone());
        let limits = limits(&[(
            QuotaScopeKind::Deployment,
            QuotaWindow::Minute,
            QuotaDimension::Requests,
            10,
        )]);
        let plan = || {
            Arc::new(plan_quota_charges(
                &limits,
                &request(&[(QuotaScopeKind::Deployment, "d")], 0),
                clock.now_ms(),
            ))
        };
        let mut last = None;
        for _ in 0..4 {
            let outcome = repository.reserve(plan()).await.unwrap();
            last = quota_usage_millis(&outcome.observation);
        }
        // 4 of 10 used.
        assert_eq!(last, Some(400));
    }

    #[tokio::test]
    async fn an_empty_plan_is_a_free_pass_and_allocates_nothing() {
        let repository = MemoryScopedQuotaRepository::new(Duration::from_secs(60));
        let plan = Arc::new(plan_quota_charges(
            &QuotaLimitSet::default(),
            &request(&[(QuotaScopeKind::Tenant, "t")], 10),
            0,
        ));
        let outcome = repository.reserve(plan).await.unwrap();
        let permit = outcome.result.expect("no limits means no refusal");
        assert!(permit.is_empty());
        assert!(outcome.observation.counters.is_empty());
    }

    #[tokio::test]
    async fn a_sliding_minute_window_frees_as_it_rolls() {
        let clock = FixedClock::new(0);
        let repository =
            MemoryScopedQuotaRepository::with_clock(Duration::from_secs(60), clock.clone());
        let limits = limits(&[(
            QuotaScopeKind::Provider,
            QuotaWindow::Minute,
            QuotaDimension::Requests,
            1,
        )]);
        let plan = || {
            Arc::new(plan_quota_charges(
                &limits,
                &request(&[(QuotaScopeKind::Provider, "p")], 0),
                clock.now_ms(),
            ))
        };
        repository.reserve(plan()).await.unwrap().result.unwrap();
        assert!(repository.reserve(plan()).await.unwrap().result.is_err());
        clock.advance(60_001);
        assert!(repository.reserve(plan()).await.unwrap().result.is_ok());
    }

    #[tokio::test]
    #[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
    async fn redis_and_memory_backends_agree_on_the_same_plan() {
        let url = std::env::var("UROUTER_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_owned());
        let prefix = format!("urouter-test-{}", std::process::id());
        let redis = RedisScopedQuotaRepository::connect(&url, prefix, Duration::from_secs(60))
            .await
            .expect("redis is reachable");
        let memory = MemoryScopedQuotaRepository::new(Duration::from_secs(60));
        let limits = limits(&[
            (
                QuotaScopeKind::Provider,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                3,
            ),
            (
                QuotaScopeKind::Deployment,
                QuotaWindow::Day,
                QuotaDimension::Tokens,
                500,
            ),
        ]);
        for _ in 0..6 {
            let plan = Arc::new(plan_quota_charges(
                &limits,
                &request(
                    &[
                        (QuotaScopeKind::Provider, "p"),
                        (QuotaScopeKind::Deployment, "d"),
                    ],
                    100,
                ),
                0,
            ));
            let from_redis = redis.reserve(Arc::clone(&plan)).await.unwrap();
            let from_memory = memory.reserve(plan).await.unwrap();
            // Same admission decision AND the same first breaching counter.
            assert_eq!(
                from_redis.result.as_ref().err().map(|breach| breach.reason()),
                from_memory
                    .result
                    .as_ref()
                    .err()
                    .map(|breach| breach.reason())
            );
        }
    }
}
