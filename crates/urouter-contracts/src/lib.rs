//! Protocol-neutral contracts shared by the online and offline routing paths.
//!
//! This crate is pure: it does not read clocks, files, environment variables,
//! networks, or shared state. Runtime observations must be supplied explicitly.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FEATURE_SCHEMA_VERSION: u16 = 1;
pub const TRACE_SCHEMA_VERSION: u16 = 1;
pub const DECISION_RECORD_SCHEMA_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedStateDomain {
    Budget,
    Quota,
    Binding,
    Circuit,
    DecisionRecord,
    Metrics,
    LatencySignal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendFailurePolicy {
    FailClosed,
    FailOpen,
}

#[must_use]
pub const fn shared_state_failure_policy(domain: SharedStateDomain) -> BackendFailurePolicy {
    match domain {
        SharedStateDomain::Budget
        | SharedStateDomain::Quota
        | SharedStateDomain::Binding
        | SharedStateDomain::Circuit
        | SharedStateDomain::DecisionRecord => BackendFailurePolicy::FailClosed,
        SharedStateDomain::Metrics | SharedStateDomain::LatencySignal => {
            BackendFailurePolicy::FailOpen
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamErrorKind {
    Transport,
    Timeout,
    RateLimited,
    ServerError,
    ProviderUnavailable,
    Unauthorized,
    NotFound,
    BadRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackCause {
    ContextWindow,
    ContentPolicy,
    Quota,
    Capacity,
    Transport,
    Timeout,
    RateLimited,
    ServerError,
    ProviderUnavailable,
    Unauthorized,
    NotFound,
    BadRequest,
}

impl From<UpstreamErrorKind> for FallbackCause {
    fn from(value: UpstreamErrorKind) -> Self {
        match value {
            UpstreamErrorKind::Transport => Self::Transport,
            UpstreamErrorKind::Timeout => Self::Timeout,
            UpstreamErrorKind::RateLimited => Self::RateLimited,
            UpstreamErrorKind::ServerError => Self::ServerError,
            UpstreamErrorKind::ProviderUnavailable => Self::ProviderUnavailable,
            UpstreamErrorKind::Unauthorized => Self::Unauthorized,
            UpstreamErrorKind::NotFound => Self::NotFound,
            UpstreamErrorKind::BadRequest => Self::BadRequest,
        }
    }
}

impl UpstreamErrorKind {
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::Transport
                | Self::Timeout
                | Self::RateLimited
                | Self::ServerError
                | Self::ProviderUnavailable
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u8,
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl RetryPolicy {
    #[must_use]
    pub const fn should_retry(self, kind: UpstreamErrorKind, retries_used: u8) -> bool {
        kind.retryable() && retries_used < self.max_retries
    }

    #[must_use]
    pub fn backoff_ms(self, retries_used: u8, retry_after_ms: Option<u64>) -> u64 {
        retry_after_ms.unwrap_or_else(|| {
            self.base_backoff_ms
                .saturating_mul(
                    1_u64
                        .checked_shl(u32::from(retries_used))
                        .unwrap_or(u64::MAX),
                )
                .min(self.max_backoff_ms)
        })
    }

    #[must_use]
    pub fn directive(
        self,
        kind: UpstreamErrorKind,
        retries_used: u8,
        candidate_count: usize,
        retry_after_ms: Option<u64>,
    ) -> RetryDirective {
        if !self.should_retry(kind, retries_used) || candidate_count == 0 {
            RetryDirective::Stop
        } else if candidate_count == 1 {
            RetryDirective::RetrySameDeployment {
                backoff_ms: self.backoff_ms(retries_used, retry_after_ms),
            }
        } else {
            RetryDirective::ReselectDeployment
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RetryDirective {
    Stop,
    RetrySameDeployment { backoff_ms: u64 },
    ReselectDeployment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureWindow {
    pub successes: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CooldownDirective {
    Ignore,
    RecordFailure,
    OpenCircuit,
}

#[must_use]
pub fn cooldown_directive(
    kind: UpstreamErrorKind,
    window: FailureWindow,
    tier_size: usize,
    failure_threshold_millis: u16,
) -> CooldownDirective {
    if !matches!(
        kind,
        UpstreamErrorKind::Transport
            | UpstreamErrorKind::Timeout
            | UpstreamErrorKind::RateLimited
            | UpstreamErrorKind::ServerError
            | UpstreamErrorKind::ProviderUnavailable
            | UpstreamErrorKind::Unauthorized
            | UpstreamErrorKind::NotFound
    ) {
        return CooldownDirective::Ignore;
    }
    let failures = window.failures.saturating_add(1);
    let total = window.successes.saturating_add(failures);
    let failure_millis = failures
        .saturating_mul(1_000)
        .checked_div(total)
        .unwrap_or(0);
    if tier_size > 1
        && (kind == UpstreamErrorKind::RateLimited
            || failure_millis >= u64::from(failure_threshold_millis))
    {
        CooldownDirective::OpenCircuit
    } else {
        CooldownDirective::RecordFailure
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackTierSpec {
    pub tier: String,
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingPlan {
    pub tiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingPlanError {
    EmptyTier,
    DuplicateTier(String),
    UnknownTier(String),
    Cycle(String),
}

pub fn plan_fallback_tiers(
    tiers: &[FallbackTierSpec],
    selected: &str,
    max_depth: u8,
) -> Result<RoutingPlan, RoutingPlanError> {
    let mut indexes = BTreeMap::new();
    for (index, tier) in tiers.iter().enumerate() {
        if tier.tier.is_empty() {
            return Err(RoutingPlanError::EmptyTier);
        }
        if indexes.insert(tier.tier.as_str(), index).is_some() {
            return Err(RoutingPlanError::DuplicateTier(tier.tier.clone()));
        }
    }
    let mut planned = Vec::new();
    let mut emitted = BTreeSet::new();
    let mut active = BTreeSet::new();
    append_planned_tier(
        tiers,
        &indexes,
        selected,
        max_depth,
        &mut emitted,
        &mut active,
        &mut planned,
    )?;
    Ok(RoutingPlan { tiers: planned })
}

fn append_planned_tier(
    tiers: &[FallbackTierSpec],
    indexes: &BTreeMap<&str, usize>,
    tier_name: &str,
    remaining: u8,
    emitted: &mut BTreeSet<String>,
    active: &mut BTreeSet<String>,
    planned: &mut Vec<String>,
) -> Result<(), RoutingPlanError> {
    if active.contains(tier_name) {
        return Err(RoutingPlanError::Cycle(tier_name.to_owned()));
    }
    let index = indexes
        .get(tier_name)
        .copied()
        .ok_or_else(|| RoutingPlanError::UnknownTier(tier_name.to_owned()))?;
    if !emitted.insert(tier_name.to_owned()) {
        return Ok(());
    }
    planned.push(tier_name.to_owned());
    if remaining == 0 {
        return Ok(());
    }
    active.insert(tier_name.to_owned());
    for fallback in &tiers[index].fallbacks {
        append_planned_tier(
            tiers,
            indexes,
            fallback,
            remaining - 1,
            emitted,
            active,
            planned,
        )?;
    }
    active.remove(tier_name);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentCandidate {
    pub id: String,
    pub order: u16,
    pub weight: u32,
    pub available: bool,
    pub unavailable_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentDisposition {
    Selected,
    RunnerUp,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentEvaluation {
    pub deployment: String,
    pub disposition: DeploymentDisposition,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentSelection {
    pub selected: String,
    pub runners_up: Vec<String>,
    pub evaluations: Vec<DeploymentEvaluation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalCircuitAvailability {
    Closed,
    Open,
    HalfOpen,
    HalfOpenProbeInFlight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityCandidateSnapshot {
    pub id: String,
    pub order: u16,
    pub weight: u32,
    pub retry_excluded: bool,
    pub circuit: LocalCircuitAvailability,
    pub in_flight: u64,
    pub latency_ewma_ms: Option<u64>,
    pub quota_usage_millis: Option<u16>,
    #[serde(default)]
    pub unavailable_reasons: Vec<String>,
}

pub const CAPACITY_SNAPSHOT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorRef {
    pub shard: String,
    pub offset_bytes: u64,
    pub dimensions: u32,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacitySnapshot {
    pub schema_version: u16,
    pub candidates: Vec<CapacityCandidateSnapshot>,
}

impl Default for CapacitySnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl CapacitySnapshot {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            schema_version: CAPACITY_SNAPSHOT_SCHEMA_VERSION,
            candidates: Vec::new(),
        }
    }

    #[must_use]
    pub const fn new(candidates: Vec<CapacityCandidateSnapshot>) -> Self {
        Self {
            schema_version: CAPACITY_SNAPSHOT_SCHEMA_VERSION,
            candidates,
        }
    }

    #[must_use]
    pub fn candidate(&self, deployment: &str) -> Option<&CapacityCandidateSnapshot> {
        self.candidates
            .iter()
            .find(|candidate| candidate.id == deployment)
    }

    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.schema_version == CAPACITY_SNAPSHOT_SCHEMA_VERSION
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentPicker {
    #[default]
    Weighted,
    LeastLoaded,
    LowestLatency,
    LowestQuotaUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityLeasePlan {
    pub selection: DeploymentSelection,
    pub reserve_half_open_probe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityLeasePlanError {
    Empty,
    Exhausted(Vec<DeploymentEvaluation>),
    ZeroWeight,
}

pub fn plan_capacity_lease(
    candidates: &[CapacityCandidateSnapshot],
    ticket: u64,
) -> Result<CapacityLeasePlan, CapacityLeasePlanError> {
    plan_capacity_lease_with_picker(candidates, ticket, DeploymentPicker::Weighted)
}

pub fn plan_capacity_lease_with_picker(
    candidates: &[CapacityCandidateSnapshot],
    ticket: u64,
    picker: DeploymentPicker,
) -> Result<CapacityLeasePlan, CapacityLeasePlanError> {
    if candidates.is_empty() {
        return Err(CapacityLeasePlanError::Empty);
    }
    let mut policy_candidates = candidates
        .iter()
        .map(|candidate| {
            let mut unavailable_reasons = candidate.unavailable_reasons.clone();
            if candidate.retry_excluded {
                unavailable_reasons.push("retry_excluded".to_owned());
            }
            if matches!(
                candidate.circuit,
                LocalCircuitAvailability::Open | LocalCircuitAvailability::HalfOpenProbeInFlight
            ) {
                unavailable_reasons.push("local_circuit_unavailable".to_owned());
            }
            DeploymentCandidate {
                id: candidate.id.clone(),
                order: candidate.order,
                weight: candidate.weight,
                available: unavailable_reasons.is_empty(),
                unavailable_reasons,
            }
        })
        .collect::<Vec<_>>();
    if !policy_candidates
        .iter()
        .any(|candidate| candidate.available)
    {
        return Err(CapacityLeasePlanError::Exhausted(
            policy_candidates
                .into_iter()
                .map(|candidate| DeploymentEvaluation {
                    deployment: candidate.id,
                    disposition: DeploymentDisposition::Excluded,
                    reasons: candidate.unavailable_reasons,
                })
                .collect(),
        ));
    }
    apply_picker(&mut policy_candidates, candidates, picker);
    let selection =
        select_weighted_deployment(&policy_candidates, ticket).map_err(|error| match error {
            DeploymentSelectionError::Empty => CapacityLeasePlanError::Empty,
            DeploymentSelectionError::Exhausted => CapacityLeasePlanError::Exhausted(Vec::new()),
            DeploymentSelectionError::ZeroWeight => CapacityLeasePlanError::ZeroWeight,
        })?;
    let reserve_half_open_probe = candidates.iter().any(|candidate| {
        candidate.id == selection.selected
            && candidate.circuit == LocalCircuitAvailability::HalfOpen
    });
    Ok(CapacityLeasePlan {
        selection,
        reserve_half_open_probe,
    })
}

fn apply_picker(
    policy: &mut [DeploymentCandidate],
    snapshots: &[CapacityCandidateSnapshot],
    picker: DeploymentPicker,
) {
    let minimum_order = policy
        .iter()
        .filter(|candidate| candidate.available)
        .map(|candidate| candidate.order)
        .min();
    let Some(minimum_order) = minimum_order else {
        return;
    };
    let metric = |snapshot: &CapacityCandidateSnapshot| match picker {
        DeploymentPicker::Weighted => None,
        DeploymentPicker::LeastLoaded => Some(snapshot.in_flight),
        DeploymentPicker::LowestLatency => snapshot.latency_ewma_ms,
        DeploymentPicker::LowestQuotaUsage => snapshot.quota_usage_millis.map(u64::from),
    };
    let best = snapshots
        .iter()
        .filter(|snapshot| {
            policy.iter().any(|candidate| {
                candidate.id == snapshot.id
                    && candidate.available
                    && candidate.order == minimum_order
            })
        })
        .filter_map(&metric)
        .min();
    let Some(best) = best else {
        return;
    };
    let reason = match picker {
        DeploymentPicker::Weighted => return,
        DeploymentPicker::LeastLoaded => "higher_load",
        DeploymentPicker::LowestLatency => "higher_latency",
        DeploymentPicker::LowestQuotaUsage => "higher_quota_usage",
    };
    for candidate in policy
        .iter_mut()
        .filter(|candidate| candidate.available && candidate.order == minimum_order)
    {
        let candidate_metric = snapshots
            .iter()
            .find(|snapshot| snapshot.id == candidate.id)
            .and_then(metric);
        if candidate_metric.is_some_and(|value| value > best) {
            candidate.available = false;
            candidate.unavailable_reasons.push(reason.to_owned());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentSelectionError {
    Empty,
    Exhausted,
    ZeroWeight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierCandidate {
    pub index: usize,
    pub tier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierDecisionInput {
    pub candidates: Vec<TierCandidate>,
    pub pin_tier: Option<String>,
    pub floor_index: usize,
    pub auxiliary: bool,
    pub high_quality: bool,
    pub low_cost: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOutcome {
    Abstain,
    Selected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEvaluation {
    pub rule: String,
    pub outcome: RuleOutcome,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSelection {
    pub index: usize,
    pub reason: String,
    pub evaluations: Vec<RuleEvaluation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierDecisionError {
    UnknownPinnedTier(String),
    NoEligibleTier,
}

trait TierRule {
    fn id(&self) -> &'static str;

    fn evaluate(
        &self,
        input: &TierDecisionInput,
        candidates: &[&TierCandidate],
    ) -> Result<Option<(usize, String)>, TierDecisionError>;
}

struct PinRule;

impl TierRule for PinRule {
    fn id(&self) -> &'static str {
        "pin"
    }

    fn evaluate(
        &self,
        input: &TierDecisionInput,
        candidates: &[&TierCandidate],
    ) -> Result<Option<(usize, String)>, TierDecisionError> {
        let Some(pin) = &input.pin_tier else {
            return Ok(None);
        };
        candidates
            .iter()
            .find(|candidate| candidate.tier == *pin)
            .map(|candidate| (candidate.index, "preference_pin".to_owned()))
            .map(Some)
            .ok_or_else(|| TierDecisionError::UnknownPinnedTier(pin.clone()))
    }
}

struct QualityRule;

impl TierRule for QualityRule {
    fn id(&self) -> &'static str {
        "quality"
    }

    fn evaluate(
        &self,
        input: &TierDecisionInput,
        candidates: &[&TierCandidate],
    ) -> Result<Option<(usize, String)>, TierDecisionError> {
        Ok((input.high_quality && !input.auxiliary).then(|| {
            (
                candidates
                    .last()
                    .expect("cascade receives eligible candidates")
                    .index,
                "quality_guard".to_owned(),
            )
        }))
    }
}

struct DefaultRule;

impl TierRule for DefaultRule {
    fn id(&self) -> &'static str {
        "default"
    }

    fn evaluate(
        &self,
        input: &TierDecisionInput,
        candidates: &[&TierCandidate],
    ) -> Result<Option<(usize, String)>, TierDecisionError> {
        Ok(Some((
            candidates
                .first()
                .expect("cascade receives eligible candidates")
                .index,
            if input.low_cost {
                "cost_preference".to_owned()
            } else {
                "default_efficient".to_owned()
            },
        )))
    }
}

pub fn select_tier_with_cascade(
    input: &TierDecisionInput,
) -> Result<TierSelection, TierDecisionError> {
    let all = input.candidates.iter().collect::<Vec<_>>();
    if all.is_empty() {
        return Err(TierDecisionError::NoEligibleTier);
    }
    let filtered = input
        .candidates
        .iter()
        .filter(|candidate| candidate.index >= input.floor_index)
        .collect::<Vec<_>>();
    let rules: [&dyn TierRule; 3] = [&PinRule, &QualityRule, &DefaultRule];
    let mut evaluations = Vec::with_capacity(rules.len());
    for (position, rule) in rules.into_iter().enumerate() {
        let candidates = if position == 0 { &all } else { &filtered };
        if candidates.is_empty() {
            return Err(TierDecisionError::NoEligibleTier);
        }
        match rule.evaluate(input, candidates)? {
            Some((index, reason)) => {
                evaluations.push(RuleEvaluation {
                    rule: rule.id().to_owned(),
                    outcome: RuleOutcome::Selected,
                    reason: reason.clone(),
                });
                return Ok(TierSelection {
                    index,
                    reason,
                    evaluations,
                });
            }
            None => evaluations.push(RuleEvaluation {
                rule: rule.id().to_owned(),
                outcome: RuleOutcome::Abstain,
                reason: "not_applicable".to_owned(),
            }),
        }
    }
    Err(TierDecisionError::NoEligibleTier)
}

pub fn select_weighted_deployment(
    candidates: &[DeploymentCandidate],
    ticket: u64,
) -> Result<DeploymentSelection, DeploymentSelectionError> {
    if candidates.is_empty() {
        return Err(DeploymentSelectionError::Empty);
    }
    let minimum_order = candidates
        .iter()
        .filter(|candidate| candidate.available)
        .map(|candidate| candidate.order)
        .min()
        .ok_or(DeploymentSelectionError::Exhausted)?;
    let mut eligible = candidates
        .iter()
        .filter(|candidate| candidate.available && candidate.order == minimum_order)
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| left.id.cmp(&right.id));
    let total_weight = eligible.iter().try_fold(0_u64, |sum, candidate| {
        sum.checked_add(u64::from(candidate.weight))
    });
    let total_weight = total_weight
        .filter(|weight| *weight > 0)
        .ok_or(DeploymentSelectionError::ZeroWeight)?;
    let mut position = ticket % total_weight;
    let selected = eligible
        .iter()
        .find(|candidate| {
            if position < u64::from(candidate.weight) {
                true
            } else {
                position -= u64::from(candidate.weight);
                false
            }
        })
        .ok_or(DeploymentSelectionError::ZeroWeight)?;
    let selected = selected.id.clone();
    let evaluations = candidates
        .iter()
        .map(|candidate| {
            let (disposition, reasons) = if !candidate.available {
                (
                    DeploymentDisposition::Excluded,
                    if candidate.unavailable_reasons.is_empty() {
                        vec!["unavailable".to_owned()]
                    } else {
                        candidate.unavailable_reasons.clone()
                    },
                )
            } else if candidate.order != minimum_order {
                (
                    DeploymentDisposition::Excluded,
                    vec!["lower_priority_order".to_owned()],
                )
            } else if candidate.id == selected {
                (DeploymentDisposition::Selected, Vec::new())
            } else {
                (DeploymentDisposition::RunnerUp, Vec::new())
            };
            DeploymentEvaluation {
                deployment: candidate.id.clone(),
                disposition,
                reasons,
            }
        })
        .collect();
    Ok(DeploymentSelection {
        selected: selected.clone(),
        runners_up: eligible
            .iter()
            .filter(|candidate| candidate.id != selected)
            .map(|candidate| candidate.id.clone())
            .collect(),
        evaluations,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureFrame {
    pub schema_version: u16,
    pub message_count: u32,
    pub input_text_bytes: u64,
    pub available_tool_count: u32,
    pub content: ContentFeatures,
    pub requests: RequestFeatures,
    pub requested_max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentFeatures {
    pub has_tool_result: bool,
    pub contains_image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestFeatures {
    pub structured_output: bool,
    pub reasoning: bool,
}

impl FeatureFrame {
    #[must_use]
    pub fn from_openai_chat(request: &Value) -> Self {
        let messages = request
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut input_text_bytes = 0_u64;
        let mut contains_image = false;
        let mut has_tool_result = false;
        for message in messages {
            has_tool_result |= message.get("role").and_then(Value::as_str) == Some("tool");
            if let Some(content) = message.get("content") {
                collect_content_features(content, &mut input_text_bytes, &mut contains_image);
            }
        }

        let available_tool_count = request
            .get("tools")
            .and_then(Value::as_array)
            .map_or(0, |tools| saturating_u32(tools.len()));
        let requests_structured_output = request
            .pointer("/response_format/type")
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "json_object" | "json_schema"));
        let requests_reasoning = request
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .is_some_and(|effort| !effort.eq_ignore_ascii_case("none"));
        let requested_max_output_tokens = request
            .get("max_completion_tokens")
            .or_else(|| request.get("max_output_tokens"))
            .or_else(|| request.get("max_tokens"))
            .and_then(Value::as_u64);

        Self {
            schema_version: FEATURE_SCHEMA_VERSION,
            message_count: saturating_u32(messages.len()),
            input_text_bytes,
            available_tool_count,
            content: ContentFeatures {
                has_tool_result,
                contains_image,
            },
            requests: RequestFeatures {
                structured_output: requests_structured_output,
                reasoning: requests_reasoning,
            },
            requested_max_output_tokens,
        }
    }
}

fn collect_content_features(content: &Value, text_bytes: &mut u64, contains_image: &mut bool) {
    match content {
        Value::String(text) => {
            *text_bytes = text_bytes.saturating_add(text.len() as u64);
        }
        Value::Array(parts) => {
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str);
                *contains_image |= matches!(kind, Some("image_url" | "input_image" | "image"));
                if matches!(kind, Some("text" | "input_text"))
                    && let Some(text) = part.get("text").and_then(Value::as_str)
                {
                    *text_bytes = text_bytes.saturating_add(text.len() as u64);
                }
            }
        }
        _ => {}
    }
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceCompleteness {
    Summary,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateDisposition {
    Selected,
    Eligible,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateTrace {
    pub candidate: String,
    pub disposition: CandidateDisposition,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingTrace {
    pub schema_version: u16,
    pub completeness: TraceCompleteness,
    pub policy: String,
    pub reason: String,
    #[serde(default)]
    pub decisions: Vec<RuleEvaluation>,
    #[serde(default)]
    pub runtime_filters: Vec<DeploymentEvaluation>,
    #[serde(default)]
    pub policy_filters: Vec<RuleEvaluation>,
    pub candidates: Vec<CandidateTrace>,
}

impl RoutingTrace {
    #[must_use]
    pub fn current_summary(
        selected: impl Into<String>,
        alternatives: impl IntoIterator<Item = String>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        let mut candidates = vec![CandidateTrace {
            candidate: selected.into(),
            disposition: CandidateDisposition::Selected,
            reasons: vec![reason.clone()],
        }];
        candidates.extend(alternatives.into_iter().map(|candidate| CandidateTrace {
            candidate,
            disposition: CandidateDisposition::Eligible,
            reasons: Vec::new(),
        }));
        Self {
            schema_version: TRACE_SCHEMA_VERSION,
            completeness: TraceCompleteness::Summary,
            policy: "current_gateway".to_owned(),
            reason,
            decisions: Vec::new(),
            runtime_filters: Vec::new(),
            policy_filters: Vec::new(),
            candidates,
        }
    }

    #[must_use]
    pub fn with_decisions(mut self, decisions: Vec<RuleEvaluation>) -> Self {
        self.decisions = decisions;
        self
    }

    #[must_use]
    pub fn with_runtime_filters(mut self, runtime_filters: Vec<DeploymentEvaluation>) -> Self {
        self.runtime_filters = runtime_filters;
        self
    }

    #[must_use]
    pub fn with_policy_filters(mut self, policy_filters: Vec<RuleEvaluation>) -> Self {
        self.policy_filters = policy_filters;
        self.completeness = TraceCompleteness::Full;
        self
    }

    #[must_use]
    pub fn current_admission_summary(
        selected: impl Into<String>,
        alternatives: impl IntoIterator<Item = String>,
        excluded: impl IntoIterator<Item = (String, Vec<String>)>,
        reason: impl Into<String>,
    ) -> Self {
        let mut trace = Self::current_summary(selected, alternatives, reason);
        trace.candidates.extend(
            excluded
                .into_iter()
                .map(|(candidate, reasons)| CandidateTrace {
                    candidate,
                    disposition: CandidateDisposition::Excluded,
                    reasons,
                }),
        );
        trace
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionSet {
    pub catalog: String,
    pub route: String,
    pub feature_schema: u16,
    pub policy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecordContext {
    pub schema_version: u16,
    pub request_id: String,
    pub revisions: RevisionSet,
    pub features: FeatureFrame,
    pub trace: RoutingTrace,
    pub eligible_candidates: Vec<String>,
    pub propensity_millionths: Option<u32>,
}

impl DecisionRecordContext {
    #[must_use]
    pub fn deterministic(
        request_id: impl Into<String>,
        revisions: RevisionSet,
        features: FeatureFrame,
        trace: RoutingTrace,
        eligible_candidates: Vec<String>,
    ) -> Self {
        Self {
            schema_version: DECISION_RECORD_SCHEMA_VERSION,
            request_id: request_id.into(),
            revisions,
            features,
            trace,
            eligible_candidates,
            propensity_millionths: None,
        }
    }

    #[must_use]
    pub fn training_complete(&self) -> bool {
        self.schema_version == DECISION_RECORD_SCHEMA_VERSION
            && !self.request_id.is_empty()
            && !self.revisions.catalog.is_empty()
            && !self.revisions.route.is_empty()
            && !self.revisions.policy.is_empty()
            && !self.eligible_candidates.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retry_policy_is_typed_bounded_and_honors_retry_after() {
        let policy = RetryPolicy {
            max_retries: 2,
            base_backoff_ms: 50,
            max_backoff_ms: 1_000,
        };
        assert!(policy.should_retry(UpstreamErrorKind::Timeout, 0));
        assert!(policy.should_retry(UpstreamErrorKind::ServerError, 1));
        assert!(!policy.should_retry(UpstreamErrorKind::ServerError, 2));
        assert!(!policy.should_retry(UpstreamErrorKind::BadRequest, 0));
        assert_eq!(policy.backoff_ms(1, None), 100);
        assert_eq!(policy.backoff_ms(1, Some(750)), 750);
        assert_eq!(
            policy.directive(UpstreamErrorKind::Timeout, 0, 1, Some(750)),
            RetryDirective::RetrySameDeployment { backoff_ms: 750 }
        );
        assert_eq!(
            policy.directive(UpstreamErrorKind::ServerError, 0, 2, None),
            RetryDirective::ReselectDeployment
        );
        assert_eq!(
            policy.directive(UpstreamErrorKind::BadRequest, 0, 2, None),
            RetryDirective::Stop
        );
        assert_eq!(
            policy.directive(UpstreamErrorKind::Timeout, 0, 0, None),
            RetryDirective::Stop
        );
    }

    #[test]
    fn cooldown_policy_is_typed_and_preserves_single_deployment_availability() {
        let empty = FailureWindow {
            successes: 0,
            failures: 0,
        };
        assert_eq!(
            cooldown_directive(UpstreamErrorKind::BadRequest, empty, 2, 500),
            CooldownDirective::Ignore
        );
        assert_eq!(
            cooldown_directive(UpstreamErrorKind::RateLimited, empty, 1, 500),
            CooldownDirective::RecordFailure
        );
        assert_eq!(
            cooldown_directive(UpstreamErrorKind::RateLimited, empty, 2, 500),
            CooldownDirective::OpenCircuit
        );
        assert_eq!(
            cooldown_directive(
                UpstreamErrorKind::ServerError,
                FailureWindow {
                    successes: 1,
                    failures: 0,
                },
                2,
                500,
            ),
            CooldownDirective::OpenCircuit
        );
    }

    #[test]
    fn cooldown_opening_is_monotonic_across_failure_thresholds() {
        for successes in 0..8 {
            for failures in 0..8 {
                let window = FailureWindow {
                    successes,
                    failures,
                };
                let lower = cooldown_directive(UpstreamErrorKind::Timeout, window, 2, 250);
                let higher = cooldown_directive(UpstreamErrorKind::Timeout, window, 2, 750);
                assert!(
                    higher != CooldownDirective::OpenCircuit
                        || lower == CooldownDirective::OpenCircuit
                );
                assert_ne!(
                    cooldown_directive(UpstreamErrorKind::Timeout, window, 1, 0),
                    CooldownDirective::OpenCircuit
                );
            }
        }
    }

    #[test]
    fn fallback_plan_is_depth_bounded_and_deduplicates_branches() {
        let tiers = vec![
            FallbackTierSpec {
                tier: "efficient".to_owned(),
                fallbacks: vec!["balanced".to_owned(), "capable".to_owned()],
            },
            FallbackTierSpec {
                tier: "balanced".to_owned(),
                fallbacks: vec!["capable".to_owned()],
            },
            FallbackTierSpec {
                tier: "capable".to_owned(),
                fallbacks: Vec::new(),
            },
        ];
        assert_eq!(
            plan_fallback_tiers(&tiers, "efficient", 0).unwrap().tiers,
            ["efficient"]
        );
        assert_eq!(
            plan_fallback_tiers(&tiers, "efficient", 2).unwrap().tiers,
            ["efficient", "balanced", "capable"]
        );
    }

    #[test]
    fn fallback_plan_rejects_unknown_references_and_cycles() {
        let unknown = vec![FallbackTierSpec {
            tier: "efficient".to_owned(),
            fallbacks: vec!["missing".to_owned()],
        }];
        assert_eq!(
            plan_fallback_tiers(&unknown, "efficient", 1),
            Err(RoutingPlanError::UnknownTier("missing".to_owned()))
        );

        let cycle = vec![
            FallbackTierSpec {
                tier: "efficient".to_owned(),
                fallbacks: vec!["capable".to_owned()],
            },
            FallbackTierSpec {
                tier: "capable".to_owned(),
                fallbacks: vec!["efficient".to_owned()],
            },
        ];
        assert_eq!(
            plan_fallback_tiers(&cycle, "efficient", 2),
            Err(RoutingPlanError::Cycle("efficient".to_owned()))
        );
    }

    #[test]
    fn deployment_selection_preserves_order_and_weighted_ticket_behavior() {
        let candidates = vec![
            DeploymentCandidate {
                id: "b".to_owned(),
                order: 0,
                weight: 2,
                available: true,
                unavailable_reasons: Vec::new(),
            },
            DeploymentCandidate {
                id: "a".to_owned(),
                order: 0,
                weight: 1,
                available: true,
                unavailable_reasons: Vec::new(),
            },
            DeploymentCandidate {
                id: "backup".to_owned(),
                order: 1,
                weight: 100,
                available: true,
                unavailable_reasons: Vec::new(),
            },
        ];
        assert_eq!(
            select_weighted_deployment(&candidates, 0).unwrap().selected,
            "a"
        );
        assert_eq!(
            select_weighted_deployment(&candidates, 1).unwrap().selected,
            "b"
        );
        assert_eq!(
            select_weighted_deployment(&candidates, 2).unwrap().selected,
            "b"
        );
        assert_eq!(
            select_weighted_deployment(&candidates, 0)
                .unwrap()
                .runners_up,
            ["b"]
        );
    }

    #[test]
    fn deployment_selection_excludes_unavailable_before_ordering() {
        let candidates = vec![
            DeploymentCandidate {
                id: "primary".to_owned(),
                order: 0,
                weight: 1,
                available: false,
                unavailable_reasons: vec!["cooldown".to_owned()],
            },
            DeploymentCandidate {
                id: "backup".to_owned(),
                order: 1,
                weight: 1,
                available: true,
                unavailable_reasons: Vec::new(),
            },
        ];
        assert_eq!(
            select_weighted_deployment(&candidates, 0).unwrap().selected,
            "backup"
        );
        let selection = select_weighted_deployment(&candidates, 0).unwrap();
        assert_eq!(
            selection.evaluations[0].disposition,
            DeploymentDisposition::Excluded
        );
        assert_eq!(selection.evaluations[0].reasons, ["cooldown"]);
    }

    #[test]
    fn capacity_lease_plan_reserves_only_the_selected_half_open_probe() {
        let candidates = vec![
            CapacityCandidateSnapshot {
                id: "open-primary".to_owned(),
                order: 0,
                weight: 1,
                retry_excluded: false,
                circuit: LocalCircuitAvailability::Open,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
            CapacityCandidateSnapshot {
                id: "half-open".to_owned(),
                order: 0,
                weight: 1,
                retry_excluded: false,
                circuit: LocalCircuitAvailability::HalfOpen,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
            CapacityCandidateSnapshot {
                id: "backup".to_owned(),
                order: 1,
                weight: 1,
                retry_excluded: false,
                circuit: LocalCircuitAvailability::Closed,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
        ];
        let plan = plan_capacity_lease(&candidates, 0).unwrap();
        assert_eq!(plan.selection.selected, "half-open");
        assert!(plan.reserve_half_open_probe);
        assert!(plan.selection.evaluations.iter().any(|candidate| {
            candidate.deployment == "open-primary"
                && candidate.reasons == ["local_circuit_unavailable"]
        }));
    }

    #[test]
    fn capacity_lease_plan_exhaustion_retains_all_exclusion_reasons() {
        let candidates = vec![
            CapacityCandidateSnapshot {
                id: "retry".to_owned(),
                order: 0,
                weight: 1,
                retry_excluded: true,
                circuit: LocalCircuitAvailability::Closed,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
            CapacityCandidateSnapshot {
                id: "busy-probe".to_owned(),
                order: 0,
                weight: 1,
                retry_excluded: true,
                circuit: LocalCircuitAvailability::HalfOpenProbeInFlight,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
        ];
        let CapacityLeasePlanError::Exhausted(evaluations) =
            plan_capacity_lease(&candidates, 0).unwrap_err()
        else {
            panic!("expected exhausted capacity plan");
        };
        assert_eq!(evaluations.len(), 2);
        assert_eq!(evaluations[0].reasons, ["retry_excluded"]);
        assert_eq!(
            evaluations[1].reasons,
            ["retry_excluded", "local_circuit_unavailable"]
        );
    }

    #[test]
    fn capacity_pickers_use_signals_and_deterministic_weighted_ties() {
        let candidate = |id: &str, load, latency, quota| CapacityCandidateSnapshot {
            id: id.to_owned(),
            order: 0,
            weight: 1,
            retry_excluded: false,
            circuit: LocalCircuitAvailability::Closed,
            in_flight: load,
            latency_ewma_ms: latency,
            quota_usage_millis: quota,
            unavailable_reasons: Vec::new(),
        };
        let candidates = vec![
            candidate("a", 2, Some(30), Some(500)),
            candidate("b", 0, Some(10), Some(700)),
            candidate("c", 1, Some(20), Some(100)),
        ];
        assert_eq!(
            plan_capacity_lease_with_picker(&candidates, 0, DeploymentPicker::LeastLoaded)
                .unwrap()
                .selection
                .selected,
            "b"
        );
        assert_eq!(
            plan_capacity_lease_with_picker(&candidates, 0, DeploymentPicker::LowestLatency)
                .unwrap()
                .selection
                .selected,
            "b"
        );
        assert_eq!(
            plan_capacity_lease_with_picker(&candidates, 0, DeploymentPicker::LowestQuotaUsage)
                .unwrap()
                .selection
                .selected,
            "c"
        );
    }

    fn tier_input() -> TierDecisionInput {
        TierDecisionInput {
            candidates: vec![
                TierCandidate {
                    index: 0,
                    tier: "efficient".to_owned(),
                },
                TierCandidate {
                    index: 1,
                    tier: "capable".to_owned(),
                },
            ],
            pin_tier: None,
            floor_index: 0,
            auxiliary: false,
            high_quality: false,
            low_cost: false,
        }
    }

    #[test]
    fn tier_cascade_records_abstention_before_default_selection() {
        let selection = select_tier_with_cascade(&tier_input()).unwrap();
        assert_eq!(selection.index, 0);
        assert_eq!(selection.reason, "default_efficient");
        assert_eq!(selection.evaluations.len(), 3);
        assert_eq!(selection.evaluations[0].outcome, RuleOutcome::Abstain);
        assert_eq!(selection.evaluations[2].outcome, RuleOutcome::Selected);
    }

    #[test]
    fn tier_cascade_preserves_pin_quality_and_auxiliary_semantics() {
        let mut input = tier_input();
        input.pin_tier = Some("capable".to_owned());
        assert_eq!(select_tier_with_cascade(&input).unwrap().index, 1);

        input.pin_tier = None;
        input.high_quality = true;
        assert_eq!(select_tier_with_cascade(&input).unwrap().index, 1);

        input.auxiliary = true;
        input.low_cost = true;
        let selection = select_tier_with_cascade(&input).unwrap();
        assert_eq!(selection.index, 0);
        assert_eq!(selection.reason, "cost_preference");
    }

    #[test]
    fn tier_cascade_rejects_unknown_pin_and_empty_floor() {
        let mut input = tier_input();
        input.pin_tier = Some("unknown".to_owned());
        assert_eq!(
            select_tier_with_cascade(&input),
            Err(TierDecisionError::UnknownPinnedTier("unknown".to_owned()))
        );
        input.pin_tier = None;
        input.floor_index = 2;
        assert_eq!(
            select_tier_with_cascade(&input),
            Err(TierDecisionError::NoEligibleTier)
        );
    }

    #[test]
    fn extracts_stable_chat_features_without_semantic_guessing() {
        let frame = FeatureFrame::from_openai_chat(&json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "solve"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,x"}}
                ]},
                {"role": "tool", "content": "result"}
            ],
            "tools": [{"type": "function"}],
            "response_format": {"type": "json_schema"},
            "reasoning_effort": "high",
            "max_completion_tokens": 64
        }));
        assert_eq!(frame.schema_version, FEATURE_SCHEMA_VERSION);
        assert_eq!(frame.message_count, 2);
        assert_eq!(frame.input_text_bytes, 11);
        assert_eq!(frame.available_tool_count, 1);
        assert!(frame.content.has_tool_result);
        assert!(frame.requests.structured_output);
        assert!(frame.requests.reasoning);
        assert!(frame.content.contains_image);
        assert_eq!(frame.requested_max_output_tokens, Some(64));
    }

    #[test]
    fn summary_trace_does_not_claim_full_filter_coverage() {
        let trace = RoutingTrace::current_admission_summary(
            "small",
            ["large".to_owned()],
            [(
                "text-only".to_owned(),
                vec!["missing_modality:image".to_owned()],
            )],
            "default_efficient",
        );
        assert_eq!(trace.completeness, TraceCompleteness::Summary);
        assert_eq!(
            trace.candidates[0].disposition,
            CandidateDisposition::Selected
        );
        assert_eq!(
            trace.candidates[1].disposition,
            CandidateDisposition::Eligible
        );
        assert_eq!(
            trace.candidates[2].disposition,
            CandidateDisposition::Excluded
        );
        assert_eq!(trace.candidates[2].reasons, ["missing_modality:image"]);
        assert_eq!(trace.completeness, TraceCompleteness::Summary);
    }

    #[test]
    fn deterministic_record_context_is_complete_without_fake_propensity() {
        let features = FeatureFrame::from_openai_chat(&json!({
            "messages": [{"role": "user", "content": "hello"}]
        }));
        let context = DecisionRecordContext::deterministic(
            "req_1",
            RevisionSet {
                catalog: "sha256:catalog".to_owned(),
                route: "sha256:route".to_owned(),
                feature_schema: FEATURE_SCHEMA_VERSION,
                policy: "current_gateway:v1".to_owned(),
            },
            features,
            RoutingTrace::current_summary("small", [], "default"),
            vec!["small".to_owned()],
        );
        assert!(context.training_complete());
        assert_eq!(context.propensity_millionths, None);
    }

    #[test]
    fn shared_state_failure_matrix_protects_correctness_domains() {
        for domain in [
            SharedStateDomain::Budget,
            SharedStateDomain::Quota,
            SharedStateDomain::Binding,
            SharedStateDomain::Circuit,
            SharedStateDomain::DecisionRecord,
        ] {
            assert_eq!(
                shared_state_failure_policy(domain),
                BackendFailurePolicy::FailClosed
            );
        }
        for domain in [SharedStateDomain::Metrics, SharedStateDomain::LatencySignal] {
            assert_eq!(
                shared_state_failure_policy(domain),
                BackendFailurePolicy::FailOpen
            );
        }
    }
}
