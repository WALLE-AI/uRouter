use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use urouter_contracts::DecisionRecordContext;

pub const DATASET_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRecord {
    pub schema_version: u16,
    pub tenant_key: String,
    pub task_key: Option<String>,
    pub timestamp_unix: u64,
    pub deletion_generation: u64,
    pub context: DecisionRecordContext,
    pub selected_tier: String,
    pub semantic_task: String,
    pub outcome: Option<EvaluationOutcome>,
    pub governance: ExportGovernance,
    pub exploration: Option<ExplorationEvidence>,
    #[serde(default)]
    pub flags: SampleFlags,
    #[serde(default)]
    pub feedback: Vec<FeedbackEvidence>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SampleFlags {
    pub usage_unavailable: bool,
    pub capacity_constrained: bool,
    pub catalog_drift: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationOutcome {
    pub quality_millionths: i64,
    pub cost_nano_usd: u64,
    pub latency_ms: u64,
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportGovernance {
    pub recording: RecordingClass,
    pub training_consent: bool,
    pub redaction_profile: String,
    pub body_retained: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingClass {
    None,
    MetadataOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationEvidence {
    pub epsilon_millionths: u32,
    pub propensity_millionths: u32,
    pub eligible_set: Vec<String>,
    pub selected_by_exploration: bool,
    pub authorized_budget_nano_usd: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackEvidence {
    pub source: String,
    pub source_event_id: String,
    pub value_millionths: i64,
    pub observed_at_unix: u64,
}

pub fn evaluation_record_from_gateway(value: &Value) -> Result<EvaluationRecord, IngestError> {
    let context: DecisionRecordContext = serde_json::from_value(
        value
            .get("context")
            .cloned()
            .ok_or(IngestError::MissingField("context"))?,
    )?;
    if !context.training_complete() {
        return Err(IngestError::IncompleteContext);
    }
    let tenant_key = required_gateway_string(value, "tenant_key")?;
    let selected_tier = required_gateway_string(value, "tier")?;
    let semantic_task = required_gateway_string(value, "semantic_task")?;
    let ok = value
        .pointer("/execution/ok")
        .and_then(Value::as_bool)
        .ok_or(IngestError::MissingField("execution.ok"))?;
    let quality_millionths = gateway_quality(value, ok);
    let exploration = value
        .get("exploration")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;
    let flags = SampleFlags {
        usage_unavailable: value
            .pointer("/execution/usage_unavailable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        capacity_constrained: gateway_capacity_constrained(value, &context),
        catalog_drift: false,
    };
    Ok(EvaluationRecord {
        schema_version: DATASET_SCHEMA_VERSION,
        tenant_key,
        task_key: value
            .get("task_key")
            .and_then(Value::as_str)
            .map(str::to_owned),
        timestamp_unix: value
            .get("created_at_unix_s")
            .and_then(Value::as_u64)
            .ok_or(IngestError::MissingField("created_at_unix_s"))?,
        deletion_generation: value
            .get("deletion_generation")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        context,
        selected_tier,
        semantic_task,
        outcome: Some(EvaluationOutcome {
            quality_millionths,
            cost_nano_usd: value
                .pointer("/execution/cost/total")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            latency_ms: value
                .pointer("/execution/upstream_latency_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            failed: !ok,
        }),
        governance: ExportGovernance {
            recording: match value.get("recording").and_then(Value::as_str) {
                Some("metadata_only") => RecordingClass::MetadataOnly,
                _ => RecordingClass::None,
            },
            training_consent: value
                .get("training_eligible")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            redaction_profile: value
                .get("redaction_profile")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            body_retained: false,
        },
        exploration,
        flags,
        feedback: Vec::new(),
    })
}

fn gateway_capacity_constrained(value: &Value, context: &DecisionRecordContext) -> bool {
    let removed = value
        .pointer("/execution/runtime_filter_trace")
        .and_then(Value::as_array)
        .map_or(0, |trace| {
            trace
                .iter()
                .filter(|entry| {
                    entry.get("disposition").and_then(Value::as_str) == Some("filtered")
                        && entry
                            .get("reasons")
                            .and_then(Value::as_array)
                            .is_some_and(|reasons| {
                                reasons.iter().any(|reason| {
                                    reason.as_str().is_some_and(|reason| {
                                        reason.contains("quota") || reason.contains("circuit")
                                    })
                                })
                            })
                })
                .count()
        });
    let candidates = context.eligible_candidates.len();
    candidates > 0 && removed.saturating_mul(2) > candidates
}

fn required_gateway_string(value: &Value, field: &'static str) -> Result<String, IngestError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(IngestError::MissingField(field))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn gateway_quality(value: &Value, ok: bool) -> i64 {
    value
        .get("outcome_signals")
        .and_then(Value::as_array)
        .and_then(|signals| {
            let values = signals
                .iter()
                .filter_map(|signal| signal.get("strength").and_then(Value::as_f64))
                .collect::<Vec<_>>();
            (!values.is_empty()).then(|| {
                ((values.iter().sum::<f64>() / values.len() as f64) * 1_000_000.0)
                    .clamp(-1_000_000.0, 1_000_000.0) as i64
            })
        })
        .unwrap_or(if ok { 1_000_000 } else { 0 })
}

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("gateway record is missing required field: {0}")]
    MissingField(&'static str),
    #[error("gateway record has an incomplete v2 context")]
    IncompleteContext,
    #[error("gateway record contains invalid structured data: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportFilter {
    pub tenant_keys: BTreeSet<String>,
    pub task_keys: BTreeSet<String>,
    pub start_unix: Option<u64>,
    pub end_unix: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackPolicy {
    pub allowed_sources: BTreeMap<String, u32>,
    pub max_events_per_source: usize,
    pub max_abs_value_millionths: i64,
    pub max_source_disagreement_millionths: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetManifest {
    pub schema_version: u16,
    pub dataset_hash: String,
    pub catalog_revision: String,
    pub route_revision: String,
    pub policy_revision: String,
    pub feature_schema: u16,
    pub records: usize,
    pub quarantined: usize,
    pub privacy_audit_samples: Vec<String>,
    pub quality: DatasetQuality,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatasetQuality {
    pub matching_records: usize,
    pub usage_unavailable: usize,
    pub usage_unavailable_millionths: u32,
    pub maximum_usage_unavailable_tier_share_millionths: u32,
    pub capacity_constrained: usize,
    pub capacity_constrained_millionths: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetBundle {
    pub manifest: DatasetManifest,
    pub records: Vec<EvaluationRecord>,
    pub quarantine: Vec<QuarantinedRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantinedRecord {
    pub request_id: String,
    pub reasons: Vec<QuarantineReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    InvalidSchema,
    IncompleteContext,
    CrossRevision,
    MissingOutcome,
    RecordingDisabled,
    MissingConsent,
    MissingRedactionProfile,
    BodyRetained,
    DeletedGeneration,
    InvalidExploration,
    SuspiciousFeedback,
    UsageUnavailable,
}

pub fn build_dataset(
    mut records: Vec<EvaluationRecord>,
    filter: &ExportFilter,
    deletion_generations: &BTreeMap<String, u64>,
    feedback_policy: &FeedbackPolicy,
) -> Result<DatasetBundle, DatasetError> {
    records.sort_by(|left, right| {
        left.timestamp_unix
            .cmp(&right.timestamp_unix)
            .then_with(|| left.context.request_id.cmp(&right.context.request_id))
    });
    let matching = records
        .iter()
        .filter(|record| matches_filter(record, filter))
        .collect::<Vec<_>>();
    let baseline = matching
        .first()
        .map(|record| record.context.revisions.clone())
        .ok_or(DatasetError::NoMatchingRecords)?;
    let quality = dataset_quality(&matching);
    let mut accepted = Vec::new();
    let mut quarantine = Vec::new();
    for record in records
        .into_iter()
        .filter(|record| matches_filter(record, filter))
    {
        let reasons = quarantine_reasons(
            &record,
            &baseline.catalog,
            &baseline.route,
            &baseline.policy,
            baseline.feature_schema,
            deletion_generations,
            feedback_policy,
        );
        if reasons.is_empty() {
            accepted.push(record);
        } else {
            quarantine.push(QuarantinedRecord {
                request_id: record.context.request_id,
                reasons,
            });
        }
    }
    if accepted.is_empty() {
        return Err(DatasetError::NoAcceptedRecords);
    }
    let encoded = serde_json::to_vec(&accepted)?;
    let dataset_hash = format!("sha256:{:x}", Sha256::digest(encoded));
    let privacy_audit_samples = deterministic_audit_sample(&accepted, 20);
    Ok(DatasetBundle {
        manifest: DatasetManifest {
            schema_version: DATASET_SCHEMA_VERSION,
            dataset_hash,
            catalog_revision: baseline.catalog,
            route_revision: baseline.route,
            policy_revision: baseline.policy,
            feature_schema: baseline.feature_schema,
            records: accepted.len(),
            quarantined: quarantine.len(),
            privacy_audit_samples,
            quality,
        },
        records: accepted,
        quarantine,
    })
}

fn dataset_quality(records: &[&EvaluationRecord]) -> DatasetQuality {
    let usage_unavailable = records
        .iter()
        .filter(|record| record.flags.usage_unavailable)
        .count();
    let capacity_constrained = records
        .iter()
        .filter(|record| record.flags.capacity_constrained)
        .count();
    let mut unavailable_by_tier = BTreeMap::<&str, usize>::new();
    for record in records
        .iter()
        .filter(|record| record.flags.usage_unavailable)
    {
        *unavailable_by_tier
            .entry(record.selected_tier.as_str())
            .or_default() += 1;
    }
    DatasetQuality {
        matching_records: records.len(),
        usage_unavailable,
        usage_unavailable_millionths: ratio_millionths(usage_unavailable, records.len()),
        maximum_usage_unavailable_tier_share_millionths: unavailable_by_tier
            .values()
            .copied()
            .max()
            .map_or(0, |maximum| ratio_millionths(maximum, usage_unavailable)),
        capacity_constrained,
        capacity_constrained_millionths: ratio_millionths(capacity_constrained, records.len()),
    }
}

fn ratio_millionths(numerator: usize, denominator: usize) -> u32 {
    if denominator == 0 {
        return 0;
    }
    u32::try_from(numerator.saturating_mul(1_000_000) / denominator).unwrap_or(1_000_000)
}

fn matches_filter(record: &EvaluationRecord, filter: &ExportFilter) -> bool {
    (filter.tenant_keys.is_empty() || filter.tenant_keys.contains(&record.tenant_key))
        && (filter.task_keys.is_empty()
            || record
                .task_key
                .as_ref()
                .is_some_and(|task| filter.task_keys.contains(task)))
        && filter
            .start_unix
            .is_none_or(|start| record.timestamp_unix >= start)
        && filter
            .end_unix
            .is_none_or(|end| record.timestamp_unix <= end)
}

#[allow(clippy::too_many_arguments)]
fn quarantine_reasons(
    record: &EvaluationRecord,
    catalog_revision: &str,
    route_revision: &str,
    policy_revision: &str,
    feature_schema: u16,
    deletion_generations: &BTreeMap<String, u64>,
    feedback_policy: &FeedbackPolicy,
) -> Vec<QuarantineReason> {
    let mut reasons = BTreeSet::new();
    if record.schema_version != DATASET_SCHEMA_VERSION {
        reasons.insert(QuarantineReason::InvalidSchema);
    }
    if !record.context.training_complete() {
        reasons.insert(QuarantineReason::IncompleteContext);
    }
    let revisions = &record.context.revisions;
    if revisions.catalog != catalog_revision
        || revisions.route != route_revision
        || revisions.policy != policy_revision
        || revisions.feature_schema != feature_schema
    {
        reasons.insert(QuarantineReason::CrossRevision);
    }
    if record.outcome.is_none() {
        reasons.insert(QuarantineReason::MissingOutcome);
    }
    if record.flags.usage_unavailable {
        reasons.insert(QuarantineReason::UsageUnavailable);
    }
    if record.governance.recording == RecordingClass::None {
        reasons.insert(QuarantineReason::RecordingDisabled);
    }
    if !record.governance.training_consent {
        reasons.insert(QuarantineReason::MissingConsent);
    }
    if record.governance.redaction_profile.trim().is_empty() {
        reasons.insert(QuarantineReason::MissingRedactionProfile);
    }
    if record.governance.body_retained {
        reasons.insert(QuarantineReason::BodyRetained);
    }
    let generation = deletion_generations
        .get(&record.tenant_key)
        .copied()
        .unwrap_or_default();
    if record.deletion_generation < generation {
        reasons.insert(QuarantineReason::DeletedGeneration);
    }
    if record
        .exploration
        .as_ref()
        .is_some_and(|evidence| !valid_exploration(evidence, &record.selected_tier))
    {
        reasons.insert(QuarantineReason::InvalidExploration);
    }
    if !feedback_is_trusted(&record.feedback, feedback_policy) {
        reasons.insert(QuarantineReason::SuspiciousFeedback);
    }
    reasons.into_iter().collect()
}

fn valid_exploration(evidence: &ExplorationEvidence, selected_tier: &str) -> bool {
    evidence.epsilon_millionths <= 1_000_000
        && (1..=1_000_000).contains(&evidence.propensity_millionths)
        && !evidence.eligible_set.is_empty()
        && evidence
            .eligible_set
            .iter()
            .any(|tier| tier == selected_tier)
        && evidence.authorized_budget_nano_usd > 0
}

fn feedback_is_trusted(feedback: &[FeedbackEvidence], policy: &FeedbackPolicy) -> bool {
    let mut counts = BTreeMap::<&str, usize>::new();
    let mut weighted = Vec::new();
    for event in feedback {
        let Some(weight) = policy.allowed_sources.get(&event.source) else {
            return false;
        };
        if event.source_event_id.is_empty()
            || event.value_millionths.unsigned_abs()
                > policy.max_abs_value_millionths.unsigned_abs()
        {
            return false;
        }
        let count = counts.entry(&event.source).or_default();
        *count += 1;
        if *count > policy.max_events_per_source {
            return false;
        }
        weighted.push(event.value_millionths.saturating_mul(i64::from(*weight)));
    }
    let Some((minimum, maximum)) = weighted.iter().min().zip(weighted.iter().max()) else {
        return true;
    };
    maximum.saturating_sub(*minimum) <= policy.max_source_disagreement_millionths
}

fn deterministic_audit_sample(records: &[EvaluationRecord], per_mille: u16) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| {
            let digest = Sha256::digest(record.context.request_id.as_bytes());
            let bucket = u16::from_be_bytes([digest[0], digest[1]]) % 1_000;
            (bucket < per_mille).then(|| record.context.request_id.clone())
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterfactualSample {
    pub reward: f64,
    pub behavior_probability: f64,
    pub target_probability: f64,
    pub direct_estimate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterfactualReport {
    pub samples: usize,
    pub ips: Estimate,
    pub snips: Estimate,
    pub doubly_robust: Estimate,
    pub effective_sample_size: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Estimate {
    pub value: f64,
    pub variance: f64,
    pub confidence_95_low: f64,
    pub confidence_95_high: f64,
}

#[allow(clippy::cast_precision_loss)]
pub fn counterfactual_report(
    samples: &[CounterfactualSample],
) -> Result<CounterfactualReport, EvaluationError> {
    if samples.is_empty() {
        return Err(EvaluationError::NoSamples);
    }
    if samples.iter().any(|sample| {
        !sample.reward.is_finite()
            || !sample.direct_estimate.is_finite()
            || !(0.0..=1.0).contains(&sample.target_probability)
            || !(0.0..=1.0).contains(&sample.behavior_probability)
            || sample.behavior_probability == 0.0
    }) {
        return Err(EvaluationError::InvalidProbability);
    }
    let weights = samples
        .iter()
        .map(|sample| sample.target_probability / sample.behavior_probability)
        .collect::<Vec<_>>();
    let ips_values = samples
        .iter()
        .zip(&weights)
        .map(|(sample, weight)| weight * sample.reward)
        .collect::<Vec<_>>();
    let sum_weights = weights.iter().sum::<f64>();
    if sum_weights == 0.0 {
        return Err(EvaluationError::ZeroTargetSupport);
    }
    let snips_values = samples
        .iter()
        .zip(&weights)
        .map(|(sample, weight)| weight * sample.reward * samples.len() as f64 / sum_weights)
        .collect::<Vec<_>>();
    let dr_values = samples
        .iter()
        .zip(&weights)
        .map(|(sample, weight)| {
            sample.direct_estimate + weight * (sample.reward - sample.direct_estimate)
        })
        .collect::<Vec<_>>();
    let squared_weight_sum = weights.iter().map(|weight| weight * weight).sum::<f64>();
    Ok(CounterfactualReport {
        samples: samples.len(),
        ips: estimate(&ips_values),
        snips: estimate(&snips_values),
        doubly_robust: estimate(&dr_values),
        effective_sample_size: sum_weights * sum_weights / squared_weight_sum,
    })
}

#[allow(clippy::cast_precision_loss)]
fn estimate(values: &[f64]) -> Estimate {
    let count = values.len() as f64;
    let value = values.iter().sum::<f64>() / count;
    let variance = values
        .iter()
        .map(|sample| (sample - value).powi(2))
        .sum::<f64>()
        / count;
    let margin = 1.96 * (variance / count).sqrt();
    Estimate {
        value,
        variance,
        confidence_95_low: value - margin,
        confidence_95_high: value + margin,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportDomain {
    pub allowed_semantic_tasks: BTreeSet<String>,
    pub minimum_samples_per_task: usize,
    pub maximum_input_text_bytes: u64,
    pub tools_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportDecision {
    pub supported: bool,
    pub reasons: Vec<String>,
}

impl SupportDomain {
    #[must_use]
    pub fn evaluate(
        &self,
        semantic_task: &str,
        input_text_bytes: u64,
        requires_tools: bool,
        observed_samples: usize,
    ) -> SupportDecision {
        let mut reasons = Vec::new();
        if !self.allowed_semantic_tasks.contains(semantic_task) {
            reasons.push("semantic_task_out_of_domain".to_owned());
        }
        if input_text_bytes > self.maximum_input_text_bytes {
            reasons.push("input_length_out_of_domain".to_owned());
        }
        if requires_tools && !self.tools_supported {
            reasons.push("tools_out_of_domain".to_owned());
        }
        if observed_samples < self.minimum_samples_per_task {
            reasons.push("insufficient_support".to_owned());
        }
        SupportDecision {
            supported: reasons.is_empty(),
            reasons,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkCase {
    pub id: String,
    pub category: BenchmarkCategory,
    pub semantic_task: String,
    pub quality_millionths: i64,
    pub cost_nano_usd: u64,
    pub latency_ms: u64,
    pub supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkCategory {
    Daily,
    Tool,
    Math,
    Code,
    LongContext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSummary {
    pub cases: usize,
    pub supported_cases: usize,
    pub mean_quality_millionths: f64,
    pub total_cost_nano_usd: u64,
    pub mean_latency_ms: f64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub by_category: BTreeMap<BenchmarkCategory, usize>,
    pub category_summaries: BTreeMap<BenchmarkCategory, BenchmarkCategorySummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkCategorySummary {
    pub cases: usize,
    pub supported_cases: usize,
    pub mean_quality_millionths: f64,
    pub total_cost_nano_usd: u64,
    pub mean_latency_ms: f64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
}

#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn benchmark_summary(cases: &[BenchmarkCase]) -> BenchmarkSummary {
    let mut by_category = BTreeMap::new();
    for case in cases {
        *by_category.entry(case.category).or_default() += 1;
    }
    let category_summaries = by_category
        .keys()
        .copied()
        .map(|category| {
            let category_cases = cases
                .iter()
                .filter(|case| case.category == category)
                .collect::<Vec<_>>();
            (category, summarize_category(&category_cases))
        })
        .collect();
    let mut latencies = cases.iter().map(|case| case.latency_ms).collect::<Vec<_>>();
    latencies.sort_unstable();
    let count = cases.len().max(1) as f64;
    BenchmarkSummary {
        cases: cases.len(),
        supported_cases: cases.iter().filter(|case| case.supported).count(),
        mean_quality_millionths: cases
            .iter()
            .map(|case| case.quality_millionths as f64)
            .sum::<f64>()
            / count,
        total_cost_nano_usd: cases.iter().fold(0_u64, |total, case| {
            total.saturating_add(case.cost_nano_usd)
        }),
        mean_latency_ms: cases.iter().map(|case| case.latency_ms as f64).sum::<f64>() / count,
        p50_latency_ms: percentile(&latencies, 50),
        p95_latency_ms: percentile(&latencies, 95),
        by_category,
        category_summaries,
    }
}

#[allow(clippy::cast_precision_loss)]
fn summarize_category(cases: &[&BenchmarkCase]) -> BenchmarkCategorySummary {
    let count = cases.len().max(1) as f64;
    let mut latencies = cases.iter().map(|case| case.latency_ms).collect::<Vec<_>>();
    latencies.sort_unstable();
    BenchmarkCategorySummary {
        cases: cases.len(),
        supported_cases: cases.iter().filter(|case| case.supported).count(),
        mean_quality_millionths: cases
            .iter()
            .map(|case| case.quality_millionths as f64)
            .sum::<f64>()
            / count,
        total_cost_nano_usd: cases.iter().fold(0_u64, |total, case| {
            total.saturating_add(case.cost_nano_usd)
        }),
        mean_latency_ms: cases.iter().map(|case| case.latency_ms as f64).sum::<f64>() / count,
        p50_latency_ms: percentile(&latencies, 50),
        p95_latency_ms: percentile(&latencies, 95),
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = percentile
        .saturating_mul(sorted.len())
        .div_ceil(100)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[rank]
}

#[derive(Debug, Error)]
pub enum DatasetError {
    #[error("no records match the export filter")]
    NoMatchingRecords,
    #[error("all matching records were quarantined")]
    NoAcceptedRecords,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error, PartialEq)]
pub enum EvaluationError {
    #[error("counterfactual evaluation requires at least one sample")]
    NoSamples,
    #[error("probabilities and rewards must be finite and behavior probability non-zero")]
    InvalidProbability,
    #[error("target policy has zero support")]
    ZeroTargetSupport,
}

#[cfg(test)]
mod tests {
    use super::*;
    use urouter_contracts::{FeatureFrame, RevisionSet, RoutingTrace};

    fn record(id: &str) -> EvaluationRecord {
        EvaluationRecord {
            schema_version: 1,
            tenant_key: "tenant-a".to_owned(),
            task_key: Some("task-a".to_owned()),
            timestamp_unix: 100,
            deletion_generation: 1,
            context: DecisionRecordContext::deterministic(
                id,
                RevisionSet {
                    catalog: "catalog-1".to_owned(),
                    route: "route-1".to_owned(),
                    feature_schema: 1,
                    policy: "policy-1".to_owned(),
                },
                FeatureFrame::from_openai_chat(&serde_json::json!({
                    "messages": [{"role": "user", "content": "hello"}]
                })),
                RoutingTrace::current_summary("efficient", Vec::new(), "default"),
                vec!["efficient".to_owned(), "capable".to_owned()],
            ),
            selected_tier: "efficient".to_owned(),
            semantic_task: "greeting".to_owned(),
            outcome: Some(EvaluationOutcome {
                quality_millionths: 800_000,
                cost_nano_usd: 10,
                latency_ms: 20,
                failed: false,
            }),
            governance: ExportGovernance {
                recording: RecordingClass::MetadataOnly,
                training_consent: true,
                redaction_profile: "metadata-v1".to_owned(),
                body_retained: false,
            },
            exploration: Some(ExplorationEvidence {
                epsilon_millionths: 10_000,
                propensity_millionths: 500_000,
                eligible_set: vec!["efficient".to_owned(), "capable".to_owned()],
                selected_by_exploration: true,
                authorized_budget_nano_usd: 1_000,
            }),
            flags: SampleFlags::default(),
            feedback: vec![FeedbackEvidence {
                source: "user".to_owned(),
                source_event_id: format!("feedback-{id}"),
                value_millionths: 900_000,
                observed_at_unix: 101,
            }],
        }
    }

    fn feedback_policy() -> FeedbackPolicy {
        FeedbackPolicy {
            allowed_sources: BTreeMap::from([("user".to_owned(), 1)]),
            max_events_per_source: 2,
            max_abs_value_millionths: 1_000_000,
            max_source_disagreement_millionths: 1_000_000,
        }
    }

    #[test]
    fn export_is_reproducible_and_quarantines_privacy_revision_and_deletion_failures() {
        let valid = record("valid");
        let mut wrong_revision = record("wrong-revision");
        wrong_revision.context.revisions.route = "route-2".to_owned();
        let mut body = record("body");
        body.governance.body_retained = true;
        let mut deleted = record("deleted");
        deleted.deletion_generation = 0;
        let generations = BTreeMap::from([("tenant-a".to_owned(), 1)]);
        let first = build_dataset(
            vec![body, wrong_revision, deleted, valid.clone()],
            &ExportFilter::default(),
            &generations,
            &feedback_policy(),
        )
        .unwrap();
        let second = build_dataset(
            vec![valid],
            &ExportFilter {
                end_unix: Some(100),
                ..ExportFilter::default()
            },
            &generations,
            &feedback_policy(),
        )
        .unwrap();
        assert_eq!(first.manifest.records, 1);
        assert_eq!(first.manifest.quarantined, 3);
        assert_eq!(first.manifest.dataset_hash, second.manifest.dataset_hash);
    }

    #[test]
    fn suspicious_feedback_and_invalid_exploration_are_quarantined() {
        let mut poisoned = record("poisoned");
        poisoned.feedback[0].source = "anonymous-bot".to_owned();
        let mut invalid_exploration = record("bad-exploration");
        invalid_exploration
            .exploration
            .as_mut()
            .unwrap()
            .propensity_millionths = 0;
        let result = build_dataset(
            vec![record("valid"), poisoned, invalid_exploration],
            &ExportFilter::default(),
            &BTreeMap::new(),
            &feedback_policy(),
        )
        .unwrap();
        assert!(result.quarantine.iter().any(|record| {
            record
                .reasons
                .contains(&QuarantineReason::SuspiciousFeedback)
        }));
        assert!(result.quarantine.iter().any(|record| {
            record
                .reasons
                .contains(&QuarantineReason::InvalidExploration)
        }));
    }

    #[test]
    fn counterfactual_report_exposes_all_estimators_and_effective_sample_size() {
        let report = counterfactual_report(&[
            CounterfactualSample {
                reward: 1.0,
                behavior_probability: 0.5,
                target_probability: 0.75,
                direct_estimate: 0.8,
            },
            CounterfactualSample {
                reward: 0.0,
                behavior_probability: 0.5,
                target_probability: 0.25,
                direct_estimate: 0.2,
            },
        ])
        .unwrap();
        assert_eq!(report.samples, 2);
        assert!((report.ips.value - 0.75).abs() < f64::EPSILON);
        assert!((report.snips.value - 0.75).abs() < f64::EPSILON);
        assert!((report.doubly_robust.value - 0.6).abs() < f64::EPSILON);
        assert!((1.0..=2.0).contains(&report.effective_sample_size));
        assert!(report.doubly_robust.confidence_95_high.is_finite());
    }

    #[test]
    fn support_domain_blocks_ood_and_low_support_canary() {
        let domain = SupportDomain {
            allowed_semantic_tasks: BTreeSet::from(["greeting".to_owned()]),
            minimum_samples_per_task: 100,
            maximum_input_text_bytes: 1_024,
            tools_supported: false,
        };
        assert!(domain.evaluate("greeting", 5, false, 100).supported);
        let rejected = domain.evaluate("weather", 2_000, true, 2);
        assert!(!rejected.supported);
        assert_eq!(rejected.reasons.len(), 4);
    }

    #[test]
    fn benchmark_covers_all_required_categories() {
        let categories = [
            BenchmarkCategory::Daily,
            BenchmarkCategory::Tool,
            BenchmarkCategory::Math,
            BenchmarkCategory::Code,
            BenchmarkCategory::LongContext,
        ];
        let cases = categories
            .into_iter()
            .enumerate()
            .map(|(index, category)| BenchmarkCase {
                id: format!("case-{index}"),
                category,
                semantic_task: "general".to_owned(),
                quality_millionths: 900_000,
                cost_nano_usd: 10,
                latency_ms: 20,
                supported: true,
            })
            .collect::<Vec<_>>();
        let summary = benchmark_summary(&cases);
        assert_eq!(summary.by_category.len(), 5);
        assert_eq!(summary.category_summaries.len(), 5);
        assert_eq!(summary.supported_cases, 5);
        assert_eq!(summary.p50_latency_ms, 20);
        assert_eq!(summary.p95_latency_ms, 20);
    }

    #[test]
    fn benchmark_reports_long_tail_and_category_summaries() {
        let cases = [
            BenchmarkCase {
                id: "daily-fast".to_owned(),
                category: BenchmarkCategory::Daily,
                semantic_task: "greeting".to_owned(),
                quality_millionths: 1_000_000,
                cost_nano_usd: 0,
                latency_ms: 100,
                supported: true,
            },
            BenchmarkCase {
                id: "daily-slow".to_owned(),
                category: BenchmarkCategory::Daily,
                semantic_task: "question".to_owned(),
                quality_millionths: 500_000,
                cost_nano_usd: 20,
                latency_ms: 1_000,
                supported: false,
            },
            BenchmarkCase {
                id: "math-tail".to_owned(),
                category: BenchmarkCategory::Math,
                semantic_task: "equation".to_owned(),
                quality_millionths: 1_000_000,
                cost_nano_usd: 80,
                latency_ms: 20_000,
                supported: true,
            },
        ];
        let summary = benchmark_summary(&cases);
        assert_eq!(summary.p50_latency_ms, 1_000);
        assert_eq!(summary.p95_latency_ms, 20_000);
        let daily = &summary.category_summaries[&BenchmarkCategory::Daily];
        assert_eq!(daily.cases, 2);
        assert_eq!(daily.supported_cases, 1);
        assert_eq!(daily.total_cost_nano_usd, 20);
        assert!((daily.mean_quality_millionths - 750_000.0).abs() < f64::EPSILON);
        assert_eq!(daily.p95_latency_ms, 1_000);
    }

    #[test]
    fn gateway_v2_record_normalizes_without_inventing_governance_or_revisions() {
        let source = record("gateway");
        let gateway = serde_json::json!({
            "context": source.context,
            "tenant_key": "tenant-a",
            "task_key": "task-a",
            "tier": "efficient",
            "semantic_task": "greeting",
            "created_at_unix_s": 10,
            "recording": "metadata_only",
            "training_eligible": true,
            "redaction_profile": "metadata-v1",
            "execution": {
                "ok": true,
                "upstream_latency_ms": 25,
                "cost": {"total": 123}
            },
            "exploration": {
                "epsilon_millionths": 10000,
                "propensity_millionths": 995_000,
                "eligible_set": ["efficient", "capable"],
                "selected_by_exploration": false,
                "authorized_budget_nano_usd": 1000
            }
        });
        let normalized = evaluation_record_from_gateway(&gateway).unwrap();
        assert_eq!(normalized.context.revisions.route, "route-1");
        assert_eq!(normalized.outcome.unwrap().cost_nano_usd, 123);
        assert!(normalized.governance.training_consent);
        assert_eq!(
            normalized.exploration.unwrap().propensity_millionths,
            995_000
        );
    }
}
