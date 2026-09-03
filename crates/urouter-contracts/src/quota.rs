//! Multi-scope rate and token accounting.
//!
//! Adapted from `FreeLLMAPI` `server/src/services/ratelimit.ts`, MIT License,
//! Copyright (c) 2026 Tashfeen Ahmed.
//!
//! ## The split this module exists to enforce
//!
//! The pure core decides **what to charge**; the runtime only **executes the
//! charge list atomically**. That is what lets multi-scope accounting be
//! race-free without any decision logic learning about Redis.
//!
//! [`plan_quota_charges`] turns a limit set plus the identities a request
//! occupies into an ordered [`QuotaChargePlan`]. A memory backend evaluates that
//! plan with [`evaluate_quota_plan`]; a Redis backend transliterates the same
//! predicate into Lua. A differential test proves the two agree, which is the
//! only reason it is safe to have the predicate written twice.
//!
//! ## Why the day window is a bucket, not a sliding window
//!
//! A free-tier daily cap resets at 00:00 UTC; it is not a trailing 24 hours.
//! `day_bucket = now_millis / 86_400_000` **is** UTC midnight by the definition
//! of the unix epoch — no calendar arithmetic, no timezone database, and it is
//! computed here in the pure core so the storage layer never reads a clock to
//! decide which day it is.

use serde::{Deserialize, Serialize};

pub const QUOTA_SCHEMA_VERSION: u16 = 1;
pub const MINUTE_WINDOW_MILLIS: u64 = 60_000;
pub const DAY_WINDOW_MILLIS: u64 = 86_400_000;

/// Whose budget a limit draws down.
///
/// These deliberately mirror the fields a `RouteDeployment` already carries
/// (`provider_scope`, `credential_scope`, `id`) and that the circuit breaker
/// already keys on. Reusing them means a deployment blocked by quota and one
/// blocked by a circuit are excluded through the same pipeline with comparable
/// reasons, instead of two parallel notions of "this deployment is unavailable"
/// that can disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaScopeKind {
    Tenant,
    Provider,
    Credential,
    Deployment,
}

impl QuotaScopeKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Provider => "provider",
            Self::Credential => "credential",
            Self::Deployment => "deployment",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindow {
    /// Concurrency: bounded by the lease TTL rather than a clock window.
    InFlight,
    Minute,
    Day,
}

impl QuotaWindow {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight",
            Self::Minute => "minute",
            Self::Day => "day",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaDimension {
    Concurrency,
    Requests,
    Tokens,
}

impl QuotaDimension {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Concurrency => "concurrency",
            Self::Requests => "requests",
            Self::Tokens => "tokens",
        }
    }
}

/// One operator-configured cap.
///
/// `limit == 0` means unlimited, matching the existing `--tenant-*` flags where
/// zero is already the "off" value. Treating 0 as "reject everything" would turn
/// a partially-filled config into a total outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaLimit {
    pub scope: QuotaScopeKind,
    pub window: QuotaWindow,
    pub dimension: QuotaDimension,
    pub limit: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaLimitSet {
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,
    #[serde(default)]
    pub limits: Vec<QuotaLimit>,
}

const fn default_schema_version() -> u16 {
    QUOTA_SCHEMA_VERSION
}

impl QuotaLimitSet {
    #[must_use]
    pub fn new(limits: Vec<QuotaLimit>) -> Self {
        Self {
            schema_version: QUOTA_SCHEMA_VERSION,
            limits,
        }
    }

    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.schema_version == QUOTA_SCHEMA_VERSION
    }

    /// Duplicate `(scope, window, dimension)` triples, which are almost always
    /// a config edit that silently loses one of the two values.
    #[must_use]
    pub fn duplicates(&self) -> Vec<QuotaLimit> {
        let mut seen = Vec::new();
        let mut duplicates = Vec::new();
        for limit in &self.limits {
            let key = (limit.scope, limit.window, limit.dimension);
            if seen.contains(&key) {
                duplicates.push(*limit);
            } else {
                seen.push(key);
            }
        }
        duplicates
    }

    /// A per-day cap below its own per-minute cap can never bind: the minute
    /// window rejects first, always. It is a config error worth surfacing.
    #[must_use]
    pub fn inconsistent_windows(&self) -> Vec<(QuotaScopeKind, QuotaDimension)> {
        let mut found = Vec::new();
        for day in self
            .limits
            .iter()
            .filter(|limit| limit.window == QuotaWindow::Day && limit.limit > 0)
        {
            if self.limits.iter().any(|minute| {
                minute.window == QuotaWindow::Minute
                    && minute.scope == day.scope
                    && minute.dimension == day.dimension
                    && minute.limit > 0
                    && minute.limit > day.limit
            }) {
                found.push((day.scope, day.dimension));
            }
        }
        found
    }
}

/// A concrete identity a request occupies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaScopeRef {
    pub kind: QuotaScopeKind,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRequest {
    pub scopes: Vec<QuotaScopeRef>,
    pub estimated_tokens: u64,
}

/// One counter this request must fit inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaCharge {
    pub scope: QuotaScopeKind,
    pub scope_id: String,
    pub window: QuotaWindow,
    pub dimension: QuotaDimension,
    pub limit: u64,
    pub amount: u64,
    /// Sliding-window span for `Minute`/`InFlight`; the full day for `Day`.
    pub window_millis: u64,
    /// `now_millis / DAY_WINDOW_MILLIS`, i.e. days since the epoch. `None` for
    /// non-daily windows. Computed here so the storage layer never has to
    /// decide what "today" means.
    pub day_bucket: Option<u64>,
}

impl QuotaCharge {
    /// When this counter next has room, in milliseconds from `now`.
    #[must_use]
    pub const fn reset_millis(&self, now_millis: u64) -> u64 {
        match self.day_bucket {
            Some(bucket) => {
                let end = (bucket + 1).saturating_mul(DAY_WINDOW_MILLIS);
                end.saturating_sub(now_millis)
            }
            None => self.window_millis,
        }
    }
}

/// The ordered list of counters a request draws down.
///
/// **The order is the rejection precedence and is part of the contract.** Both
/// the pure evaluator and the Lua script must report the same first breach, or
/// two backends would give an operator different reasons for the same refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaChargePlan {
    pub schema_version: u16,
    pub charges: Vec<QuotaCharge>,
}

impl QuotaChargePlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.charges.is_empty()
    }
}

/// Build the charge list for one request.
///
/// Emits a charge for every configured limit whose scope this request actually
/// occupies. A limit configured for a scope the request has no identity for is
/// skipped, not failed: a deployment with no `provider_scope` simply is not
/// subject to provider-level accounting.
#[must_use]
pub fn plan_quota_charges(
    limits: &QuotaLimitSet,
    request: &QuotaRequest,
    now_millis: u64,
) -> QuotaChargePlan {
    let mut charges: Vec<QuotaCharge> = Vec::new();
    for limit in &limits.limits {
        if limit.limit == 0 {
            continue;
        }
        let Some(scope) = request
            .scopes
            .iter()
            .find(|scope| scope.kind == limit.scope)
        else {
            continue;
        };
        let amount = match limit.dimension {
            QuotaDimension::Concurrency | QuotaDimension::Requests => 1,
            QuotaDimension::Tokens => request.estimated_tokens,
        };
        // A zero-token estimate would occupy a token counter without consuming
        // anything; skip rather than emit a charge that can never breach.
        if amount == 0 {
            continue;
        }
        let (window_millis, day_bucket) = match limit.window {
            QuotaWindow::Minute => (MINUTE_WINDOW_MILLIS, None),
            QuotaWindow::Day => (DAY_WINDOW_MILLIS, Some(now_millis / DAY_WINDOW_MILLIS)),
            QuotaWindow::InFlight => (0, None),
        };
        charges.push(QuotaCharge {
            scope: limit.scope,
            scope_id: scope.id.clone(),
            window: limit.window,
            dimension: limit.dimension,
            limit: limit.limit,
            amount,
            window_millis,
            day_bucket,
        });
    }
    // Deterministic order: scope, then window, then dimension. Both backends
    // walk the plan in this order, so the first breach is the same one.
    charges.sort_by(|a, b| {
        (a.scope, a.window, a.dimension).cmp(&(b.scope, b.window, b.dimension))
    });
    QuotaChargePlan {
        schema_version: QUOTA_SCHEMA_VERSION,
        charges,
    }
}

/// Current usage of one counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaCounter {
    pub scope: QuotaScopeKind,
    pub window: QuotaWindow,
    pub dimension: QuotaDimension,
    pub used: u64,
    pub limit: u64,
    pub reset_millis: u64,
}

impl QuotaCounter {
    /// Utilisation in per mille, saturating at 1000.
    #[must_use]
    pub fn used_millis(self) -> Option<u16> {
        if self.limit == 0 {
            return None;
        }
        let ratio = self.used.saturating_mul(1_000) / self.limit;
        Some(u16::try_from(ratio.min(1_000)).unwrap_or(1_000))
    }
}

/// What the storage layer observed, whether or not the request was admitted.
///
/// Returning counters on the reject path too is what makes the
/// [`quota_usage_millis`] feedback free: there is no second round trip and no
/// separate observation path that could drift from the enforcement path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaObservation {
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,
    #[serde(default)]
    pub counters: Vec<QuotaCounter>,
}

/// The counter that refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaBreach {
    pub scope: QuotaScopeKind,
    pub window: QuotaWindow,
    pub dimension: QuotaDimension,
    pub reset_millis: u64,
}

impl QuotaBreach {
    /// The exclusion reason string, e.g. `quota_provider_day_tokens`.
    ///
    /// The `quota_` prefix is what
    /// [`crate::classify_exclusion`] buckets on, so a quota refusal appears in
    /// an exhaustion summary alongside cooldowns without any extra wiring.
    #[must_use]
    pub fn reason(self) -> String {
        format!(
            "quota_{}_{}_{}",
            self.scope.as_str(),
            self.window.as_str(),
            self.dimension.as_str()
        )
    }

    /// The stable client-facing error code.
    ///
    /// The three tenant codes are spelled exactly as they were before
    /// multi-scope accounting existed, so an existing client's error handling
    /// keeps working.
    #[must_use]
    pub const fn error_code(self) -> &'static str {
        match (self.scope, self.dimension) {
            (QuotaScopeKind::Tenant, QuotaDimension::Concurrency) => "tenant_concurrency_exhausted",
            (QuotaScopeKind::Tenant, QuotaDimension::Requests) => "tenant_rate_limit_exhausted",
            (QuotaScopeKind::Tenant, QuotaDimension::Tokens) => "tenant_token_limit_exhausted",
            (QuotaScopeKind::Provider, _) => "provider_quota_exhausted",
            (QuotaScopeKind::Credential, _) => "credential_quota_exhausted",
            (QuotaScopeKind::Deployment, _) => "deployment_quota_exhausted",
        }
    }
}

/// The single admission predicate.
///
/// **All-or-nothing**: every charge is evaluated before any is applied, so a
/// breach on the fourth counter leaves the first three untouched. The Lua
/// transliteration must preserve this, and a contract test checks it.
pub fn evaluate_quota_plan(
    plan: &QuotaChargePlan,
    observation: &QuotaObservation,
) -> Result<(), QuotaBreach> {
    for charge in &plan.charges {
        let used = observation
            .counters
            .iter()
            .find(|counter| {
                counter.scope == charge.scope
                    && counter.window == charge.window
                    && counter.dimension == charge.dimension
            })
            .map_or(0, |counter| counter.used);
        if used.saturating_add(charge.amount) > charge.limit {
            return Err(QuotaBreach {
                scope: charge.scope,
                window: charge.window,
                dimension: charge.dimension,
                reset_millis: charge.window_millis,
            });
        }
    }
    Ok(())
}

/// Observed utilisation for a deployment, in per mille.
///
/// This is the value that replaces the static `quota_usage_millis` an operator
/// hand-wrote on a `RouteDeployment`. It is the MAXIMUM across counters, not an
/// average: a deployment at 5% of its daily tokens but 99% of its per-minute
/// requests is 99% used for the purpose of steering the next request at it.
#[must_use]
pub fn quota_usage_millis(observation: &QuotaObservation) -> Option<u16> {
    observation
        .counters
        .iter()
        .filter_map(|counter| counter.used_millis())
        .max()
}

/// Observed utilisation wins; the configured constant is the cold-start value.
///
/// Keeping the configured value as a fallback matters on a fresh process: with
/// no observations yet, `LowestQuotaUsage` would otherwise see `None` for every
/// candidate and rank on nothing at all.
#[must_use]
pub const fn effective_quota_usage_millis(
    configured: Option<u16>,
    observed: Option<u16>,
) -> Option<u16> {
    match observed {
        Some(observed) => Some(observed),
        None => configured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn limit(
        scope: QuotaScopeKind,
        window: QuotaWindow,
        dimension: QuotaDimension,
        limit: u64,
    ) -> QuotaLimit {
        QuotaLimit {
            scope,
            window,
            dimension,
            limit,
        }
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

    fn counter(
        scope: QuotaScopeKind,
        window: QuotaWindow,
        dimension: QuotaDimension,
        used: u64,
        limit: u64,
    ) -> QuotaCounter {
        QuotaCounter {
            scope,
            window,
            dimension,
            used,
            limit,
            reset_millis: 0,
        }
    }

    #[test]
    fn a_zero_limit_is_unlimited_not_a_total_outage() {
        let limits = QuotaLimitSet::new(vec![limit(
            QuotaScopeKind::Tenant,
            QuotaWindow::Minute,
            QuotaDimension::Requests,
            0,
        )]);
        let plan = plan_quota_charges(&limits, &request(&[(QuotaScopeKind::Tenant, "t")], 10), NOW);
        assert!(plan.is_empty());
    }

    #[test]
    fn a_limit_for_a_scope_the_request_lacks_is_skipped_not_failed() {
        let limits = QuotaLimitSet::new(vec![
            limit(
                QuotaScopeKind::Tenant,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                10,
            ),
            limit(
                QuotaScopeKind::Provider,
                QuotaWindow::Day,
                QuotaDimension::Requests,
                100,
            ),
        ]);
        // A deployment with no provider_scope has no provider identity.
        let plan = plan_quota_charges(&limits, &request(&[(QuotaScopeKind::Tenant, "t")], 5), NOW);
        assert_eq!(plan.charges.len(), 1);
        assert_eq!(plan.charges[0].scope, QuotaScopeKind::Tenant);
    }

    #[test]
    fn the_day_bucket_is_utc_midnight_by_construction() {
        let limits = QuotaLimitSet::new(vec![limit(
            QuotaScopeKind::Provider,
            QuotaWindow::Day,
            QuotaDimension::Requests,
            100,
        )]);
        // 1970-01-02T00:00:00Z exactly.
        let plan = plan_quota_charges(
            &limits,
            &request(&[(QuotaScopeKind::Provider, "p")], 1),
            DAY_WINDOW_MILLIS,
        );
        assert_eq!(plan.charges[0].day_bucket, Some(1));
        // One millisecond earlier is still the previous day.
        let plan = plan_quota_charges(
            &limits,
            &request(&[(QuotaScopeKind::Provider, "p")], 1),
            DAY_WINDOW_MILLIS - 1,
        );
        assert_eq!(plan.charges[0].day_bucket, Some(0));
    }

    #[test]
    fn a_daily_counter_resets_at_the_next_midnight_not_in_24_hours() {
        let charge = QuotaCharge {
            scope: QuotaScopeKind::Provider,
            scope_id: "p".to_owned(),
            window: QuotaWindow::Day,
            dimension: QuotaDimension::Requests,
            limit: 10,
            amount: 1,
            window_millis: DAY_WINDOW_MILLIS,
            day_bucket: Some(0),
        };
        // 23 hours into day 0: one hour left, not another 24.
        let now = 23 * 3_600_000;
        assert_eq!(charge.reset_millis(now), 3_600_000);
    }

    #[test]
    fn plan_order_is_deterministic_and_is_the_rejection_precedence() {
        let limits = QuotaLimitSet::new(vec![
            limit(
                QuotaScopeKind::Deployment,
                QuotaWindow::Day,
                QuotaDimension::Tokens,
                1_000,
            ),
            limit(
                QuotaScopeKind::Tenant,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                10,
            ),
            limit(
                QuotaScopeKind::Provider,
                QuotaWindow::Minute,
                QuotaDimension::Tokens,
                500,
            ),
        ]);
        let plan = plan_quota_charges(
            &limits,
            &request(
                &[
                    (QuotaScopeKind::Tenant, "t"),
                    (QuotaScopeKind::Provider, "p"),
                    (QuotaScopeKind::Deployment, "d"),
                ],
                100,
            ),
            NOW,
        );
        let order: Vec<_> = plan.charges.iter().map(|charge| charge.scope).collect();
        assert_eq!(
            order,
            vec![
                QuotaScopeKind::Tenant,
                QuotaScopeKind::Provider,
                QuotaScopeKind::Deployment
            ]
        );
        // Building the same plan twice gives the same order.
        let again = plan_quota_charges(
            &limits,
            &request(
                &[
                    (QuotaScopeKind::Deployment, "d"),
                    (QuotaScopeKind::Provider, "p"),
                    (QuotaScopeKind::Tenant, "t"),
                ],
                100,
            ),
            NOW,
        );
        assert_eq!(plan, again);
    }

    #[test]
    fn evaluation_reports_the_first_breach_in_plan_order() {
        let limits = QuotaLimitSet::new(vec![
            limit(
                QuotaScopeKind::Tenant,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                10,
            ),
            limit(
                QuotaScopeKind::Provider,
                QuotaWindow::Day,
                QuotaDimension::Requests,
                100,
            ),
        ]);
        let plan = plan_quota_charges(
            &limits,
            &request(
                &[
                    (QuotaScopeKind::Tenant, "t"),
                    (QuotaScopeKind::Provider, "p"),
                ],
                0,
            ),
            NOW,
        );
        // Both are exhausted; the tenant one comes first in the plan.
        let observation = QuotaObservation {
            schema_version: QUOTA_SCHEMA_VERSION,
            counters: vec![
                counter(
                    QuotaScopeKind::Tenant,
                    QuotaWindow::Minute,
                    QuotaDimension::Requests,
                    10,
                    10,
                ),
                counter(
                    QuotaScopeKind::Provider,
                    QuotaWindow::Day,
                    QuotaDimension::Requests,
                    100,
                    100,
                ),
            ],
        };
        let breach = evaluate_quota_plan(&plan, &observation).unwrap_err();
        assert_eq!(breach.scope, QuotaScopeKind::Tenant);
        assert_eq!(breach.reason(), "quota_tenant_minute_requests");
        assert_eq!(breach.error_code(), "tenant_rate_limit_exhausted");
    }

    #[test]
    fn a_request_that_exactly_fills_the_limit_is_admitted() {
        let limits = QuotaLimitSet::new(vec![limit(
            QuotaScopeKind::Tenant,
            QuotaWindow::Minute,
            QuotaDimension::Tokens,
            100,
        )]);
        let plan = plan_quota_charges(&limits, &request(&[(QuotaScopeKind::Tenant, "t")], 40), NOW);
        let observation = QuotaObservation {
            schema_version: QUOTA_SCHEMA_VERSION,
            counters: vec![counter(
                QuotaScopeKind::Tenant,
                QuotaWindow::Minute,
                QuotaDimension::Tokens,
                60,
                100,
            )],
        };
        assert!(evaluate_quota_plan(&plan, &observation).is_ok());

        // One more token does not fit.
        let plan = plan_quota_charges(&limits, &request(&[(QuotaScopeKind::Tenant, "t")], 41), NOW);
        assert!(evaluate_quota_plan(&plan, &observation).is_err());
    }

    /// The tenant error codes predate multi-scope accounting and are in client
    /// error handling in the wild; they must not drift.
    #[test]
    fn the_historical_tenant_error_codes_are_preserved() {
        for (dimension, code) in [
            (QuotaDimension::Concurrency, "tenant_concurrency_exhausted"),
            (QuotaDimension::Requests, "tenant_rate_limit_exhausted"),
            (QuotaDimension::Tokens, "tenant_token_limit_exhausted"),
        ] {
            let breach = QuotaBreach {
                scope: QuotaScopeKind::Tenant,
                window: QuotaWindow::Minute,
                dimension,
                reset_millis: 0,
            };
            assert_eq!(breach.error_code(), code);
        }
    }

    /// A quota refusal must bucket as "wait", so it appears in an exhaustion
    /// summary next to cooldowns without any extra wiring.
    #[test]
    fn a_quota_reason_buckets_as_rate_limited() {
        use crate::{ExclusionBucket, classify_exclusion};
        let breach = QuotaBreach {
            scope: QuotaScopeKind::Provider,
            window: QuotaWindow::Day,
            dimension: QuotaDimension::Tokens,
            reset_millis: 0,
        };
        assert_eq!(breach.reason(), "quota_provider_day_tokens");
        assert_eq!(
            classify_exclusion(&breach.reason()),
            ExclusionBucket::RateLimitedOrCooling
        );
    }

    #[test]
    fn utilisation_is_the_maximum_across_counters_not_the_average() {
        let observation = QuotaObservation {
            schema_version: QUOTA_SCHEMA_VERSION,
            counters: vec![
                counter(
                    QuotaScopeKind::Deployment,
                    QuotaWindow::Day,
                    QuotaDimension::Tokens,
                    5,
                    100,
                ),
                counter(
                    QuotaScopeKind::Deployment,
                    QuotaWindow::Minute,
                    QuotaDimension::Requests,
                    99,
                    100,
                ),
            ],
        };
        assert_eq!(quota_usage_millis(&observation), Some(990));
    }

    #[test]
    fn utilisation_saturates_and_ignores_unlimited_counters() {
        let observation = QuotaObservation {
            schema_version: QUOTA_SCHEMA_VERSION,
            counters: vec![
                counter(
                    QuotaScopeKind::Tenant,
                    QuotaWindow::Minute,
                    QuotaDimension::Requests,
                    500,
                    100,
                ),
                counter(
                    QuotaScopeKind::Tenant,
                    QuotaWindow::Day,
                    QuotaDimension::Requests,
                    50,
                    0,
                ),
            ],
        };
        assert_eq!(quota_usage_millis(&observation), Some(1_000));
        assert_eq!(quota_usage_millis(&QuotaObservation::default()), None);
    }

    #[test]
    fn observed_utilisation_wins_but_config_covers_the_cold_start() {
        assert_eq!(effective_quota_usage_millis(Some(800), Some(200)), Some(200));
        assert_eq!(effective_quota_usage_millis(Some(800), None), Some(800));
        assert_eq!(effective_quota_usage_millis(None, Some(200)), Some(200));
        assert_eq!(effective_quota_usage_millis(None, None), None);
    }

    #[test]
    fn config_validation_catches_duplicates_and_unreachable_daily_caps() {
        let limits = QuotaLimitSet::new(vec![
            limit(
                QuotaScopeKind::Tenant,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                10,
            ),
            limit(
                QuotaScopeKind::Tenant,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                20,
            ),
        ]);
        assert_eq!(limits.duplicates().len(), 1);

        // 100 requests/minute but only 50/day: the day cap can never bind.
        let limits = QuotaLimitSet::new(vec![
            limit(
                QuotaScopeKind::Provider,
                QuotaWindow::Minute,
                QuotaDimension::Requests,
                100,
            ),
            limit(
                QuotaScopeKind::Provider,
                QuotaWindow::Day,
                QuotaDimension::Requests,
                50,
            ),
        ]);
        assert_eq!(
            limits.inconsistent_windows(),
            vec![(QuotaScopeKind::Provider, QuotaDimension::Requests)]
        );
    }

    #[test]
    fn a_zero_token_estimate_does_not_occupy_a_token_counter() {
        let limits = QuotaLimitSet::new(vec![limit(
            QuotaScopeKind::Tenant,
            QuotaWindow::Minute,
            QuotaDimension::Tokens,
            100,
        )]);
        let plan = plan_quota_charges(&limits, &request(&[(QuotaScopeKind::Tenant, "t")], 0), NOW);
        assert!(plan.is_empty());
    }
}
