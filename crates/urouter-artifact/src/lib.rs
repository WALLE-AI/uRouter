use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use urouter_contracts::FeatureFrame;

pub const ARTIFACT_SCHEMA_VERSION: u16 = 1;
const HMAC_BLOCK_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterArtifact {
    pub schema_version: u16,
    pub artifact_revision: String,
    pub feature_schema: u16,
    pub catalog_revision: String,
    pub route_revision: String,
    pub dataset_hash: String,
    pub seed: u64,
    pub training_rows: usize,
    pub model: LinearPolicy,
    pub support: ArtifactSupportDomain,
    pub export_gates: ExportGates,
    pub content_hash: String,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinearPolicy {
    pub baseline_tier: String,
    pub promoted_tier: String,
    pub threshold_millis: i64,
    pub bias_millis: i64,
    pub weights: FeatureWeights,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureWeights {
    pub input_kib_millis: i64,
    pub message_millis: i64,
    pub tool_millis: i64,
    pub image_millis: i64,
    pub structured_millis: i64,
    pub reasoning_millis: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSupportDomain {
    pub semantic_tasks: BTreeSet<String>,
    pub maximum_input_text_bytes: u64,
    pub tools_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct ExportGates {
    pub reproducible: bool,
    pub privacy_passed: bool,
    pub support_domain_defined: bool,
    pub counterfactual_passed: bool,
    pub quality_lower_bound_millionths: i64,
    pub maximum_error_rate_millionths: u32,
    pub maximum_cost_regression_millionths: i64,
}

#[derive(Serialize)]
struct ArtifactPayload<'a> {
    schema_version: u16,
    artifact_revision: &'a str,
    feature_schema: u16,
    catalog_revision: &'a str,
    route_revision: &'a str,
    dataset_hash: &'a str,
    seed: u64,
    training_rows: usize,
    model: &'a LinearPolicy,
    support: &'a ArtifactSupportDomain,
    export_gates: &'a ExportGates,
}

impl RouterArtifact {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        feature_schema: u16,
        catalog_revision: impl Into<String>,
        route_revision: impl Into<String>,
        dataset_hash: impl Into<String>,
        seed: u64,
        training_rows: usize,
        model: LinearPolicy,
        support: ArtifactSupportDomain,
        export_gates: ExportGates,
    ) -> Result<Self, ArtifactError> {
        if !all_export_gates_pass(&export_gates) {
            return Err(ArtifactError::ExportGateFailed);
        }
        let catalog_revision = catalog_revision.into();
        let route_revision = route_revision.into();
        let dataset_hash = dataset_hash.into();
        let artifact_revision = format!(
            "sha256:{:x}",
            Sha256::digest(
                format!(
                    "{feature_schema}\n{catalog_revision}\n{route_revision}\n{dataset_hash}\n{seed}"
                )
                .as_bytes()
            )
        );
        let mut artifact = Self {
            schema_version: ARTIFACT_SCHEMA_VERSION,
            artifact_revision,
            feature_schema,
            catalog_revision,
            route_revision,
            dataset_hash,
            seed,
            training_rows,
            model,
            support,
            export_gates,
            content_hash: String::new(),
            signature: None,
        };
        artifact.content_hash = artifact.compute_hash()?;
        Ok(artifact)
    }

    pub fn sign(&mut self, key: &[u8]) -> Result<(), ArtifactError> {
        if key.is_empty() {
            return Err(ArtifactError::EmptySigningKey);
        }
        self.content_hash = self.compute_hash()?;
        self.signature = Some(hmac_sha256(key, self.signing_payload().as_bytes()));
        Ok(())
    }

    pub fn verify(
        &self,
        expected_feature_schema: u16,
        expected_catalog_revision: &str,
        expected_route_revision: &str,
        signing_key: Option<&[u8]>,
    ) -> Result<(), ArtifactError> {
        if self.schema_version != ARTIFACT_SCHEMA_VERSION {
            return Err(ArtifactError::UnsupportedSchema);
        }
        if self.feature_schema != expected_feature_schema {
            return Err(ArtifactError::FeatureSchemaMismatch);
        }
        if self.catalog_revision != expected_catalog_revision {
            return Err(ArtifactError::CatalogRevisionMismatch);
        }
        if self.route_revision != expected_route_revision {
            return Err(ArtifactError::RouteRevisionMismatch);
        }
        if !all_export_gates_pass(&self.export_gates) {
            return Err(ArtifactError::ExportGateFailed);
        }
        if self.content_hash != self.compute_hash()? {
            return Err(ArtifactError::ContentHashMismatch);
        }
        if let Some(key) = signing_key {
            let signature = self
                .signature
                .as_deref()
                .ok_or(ArtifactError::MissingSignature)?;
            let expected = hmac_sha256(key, self.signing_payload().as_bytes());
            if signature.len() != expected.len()
                || !bool::from(signature.as_bytes().ct_eq(expected.as_bytes()))
            {
                return Err(ArtifactError::InvalidSignature);
            }
        }
        Ok(())
    }

    pub fn infer(
        &self,
        features: &FeatureFrame,
        semantic_task: &str,
        operation_limit: u32,
    ) -> Result<Inference, InferenceError> {
        const REQUIRED_OPERATIONS: u32 = 6;
        if operation_limit < REQUIRED_OPERATIONS {
            return Err(InferenceError::OperationBudgetExceeded);
        }
        if !self.support.semantic_tasks.contains(semantic_task)
            || features.input_text_bytes > self.support.maximum_input_text_bytes
            || (features.available_tool_count > 0 && !self.support.tools_supported)
        {
            return Err(InferenceError::OutOfSupportDomain);
        }
        let input_kib = i64::try_from(features.input_text_bytes / 1_024).unwrap_or(i64::MAX);
        let messages = i64::from(features.message_count);
        let tools = i64::from(features.available_tool_count);
        let weights = &self.model.weights;
        let mut score = self.model.bias_millis;
        score = score.saturating_add(input_kib.saturating_mul(weights.input_kib_millis));
        score = score.saturating_add(messages.saturating_mul(weights.message_millis));
        score = score.saturating_add(tools.saturating_mul(weights.tool_millis));
        if features.content.contains_image {
            score = score.saturating_add(weights.image_millis);
        }
        if features.requests.structured_output {
            score = score.saturating_add(weights.structured_millis);
        }
        if features.requests.reasoning {
            score = score.saturating_add(weights.reasoning_millis);
        }
        let tier = if score >= self.model.threshold_millis {
            self.model.promoted_tier.clone()
        } else {
            self.model.baseline_tier.clone()
        };
        Ok(Inference {
            artifact_revision: self.artifact_revision.clone(),
            tier,
            score_millis: score,
            operations: REQUIRED_OPERATIONS,
        })
    }

    fn compute_hash(&self) -> Result<String, ArtifactError> {
        let payload = ArtifactPayload {
            schema_version: self.schema_version,
            artifact_revision: &self.artifact_revision,
            feature_schema: self.feature_schema,
            catalog_revision: &self.catalog_revision,
            route_revision: &self.route_revision,
            dataset_hash: &self.dataset_hash,
            seed: self.seed,
            training_rows: self.training_rows,
            model: &self.model,
            support: &self.support,
            export_gates: &self.export_gates,
        };
        Ok(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&payload)?)
        ))
    }

    fn signing_payload(&self) -> String {
        format!(
            "{}\n{}\n{}",
            self.schema_version, self.artifact_revision, self.content_hash
        )
    }
}

fn all_export_gates_pass(gates: &ExportGates) -> bool {
    gates.reproducible
        && gates.privacy_passed
        && gates.support_domain_defined
        && gates.counterfactual_passed
        && gates.quality_lower_bound_millionths >= 0
        && gates.maximum_error_rate_millionths <= 1_000_000
        && gates.maximum_cost_regression_millionths <= 1_000_000
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inference {
    pub artifact_revision: String,
    pub tier: String,
    pub score_millis: i64,
    pub operations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolloutPolicy {
    pub shadow: bool,
    pub canary_basis_points: u16,
    pub minimum_samples: u64,
    pub operation_limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDecision {
    pub applied_tier: String,
    pub source: DecisionSource,
    pub active_revision: Option<String>,
    pub candidate_revision: Option<String>,
    pub shadow_tier: Option<String>,
    pub fallback_reason: Option<String>,
    pub canary_bucket: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    Rule,
    ActiveArtifact,
    CandidateCanary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactStatus {
    pub active_revision: Option<String>,
    pub candidate_revision: Option<String>,
    pub killed: bool,
    pub rollout: RolloutPolicy,
    pub observations: u64,
    pub audit_events: usize,
}

#[derive(Clone)]
pub struct ArtifactController {
    inner: Arc<RwLock<ControllerState>>,
}

struct ControllerState {
    active: Option<RouterArtifact>,
    candidate: Option<RouterArtifact>,
    history: VecDeque<RouterArtifact>,
    killed: bool,
    rollout: RolloutPolicy,
    observations: u64,
    audit: Vec<ArtifactAuditEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactAuditEvent {
    pub sequence: u64,
    pub action: String,
    pub revision: Option<String>,
    pub reason: String,
}

impl ArtifactController {
    #[must_use]
    pub fn new(active: Option<RouterArtifact>, rollout: RolloutPolicy) -> Self {
        Self {
            inner: Arc::new(RwLock::new(ControllerState {
                active,
                candidate: None,
                history: VecDeque::new(),
                killed: false,
                rollout,
                observations: 0,
                audit: Vec::new(),
            })),
        }
    }

    pub fn load_candidate(&self, candidate: RouterArtifact) {
        let mut state = self.inner.write().expect("artifact lock poisoned");
        audit(
            &mut state,
            "candidate_loaded",
            Some(&candidate),
            "validated",
        );
        state.candidate = Some(candidate);
    }

    #[must_use]
    pub fn promote(&self, reason: &str) -> bool {
        let mut state = self.inner.write().expect("artifact lock poisoned");
        let Some(candidate) = state.candidate.take() else {
            return false;
        };
        if let Some(active) = state.active.replace(candidate) {
            state.history.push_front(active);
            state.history.truncate(2);
        }
        let active = state.active.clone();
        audit(&mut state, "promoted", active.as_ref(), reason);
        true
    }

    #[must_use]
    pub fn rollback(&self, reason: &str) -> bool {
        let mut state = self.inner.write().expect("artifact lock poisoned");
        let Some(previous) = state.history.pop_front() else {
            return false;
        };
        if let Some(active) = state.active.replace(previous) {
            state.history.push_front(active);
        }
        let active = state.active.clone();
        audit(&mut state, "rolled_back", active.as_ref(), reason);
        true
    }

    pub fn set_kill_switch(&self, killed: bool, reason: &str) {
        let mut state = self.inner.write().expect("artifact lock poisoned");
        state.killed = killed;
        audit(
            &mut state,
            if killed { "killed" } else { "enabled" },
            None,
            reason,
        );
    }

    pub fn update_rollout(
        &self,
        rollout: RolloutPolicy,
        reason: &str,
    ) -> Result<(), ArtifactError> {
        if rollout.canary_basis_points > 10_000 || rollout.operation_limit < 6 {
            return Err(ArtifactError::InvalidRollout);
        }
        let mut state = self.inner.write().expect("artifact lock poisoned");
        state.rollout = rollout;
        audit(&mut state, "rollout_updated", None, reason);
        Ok(())
    }

    #[must_use]
    pub fn decide(
        &self,
        features: &FeatureFrame,
        semantic_task: &str,
        tenant_key: &str,
        task_key: &str,
        rule_tier: &str,
    ) -> ArtifactDecision {
        let state = self.inner.read().expect("artifact lock poisoned");
        if state.killed {
            return rule_decision(rule_tier, "kill_switch");
        }
        let active_inference = state
            .active
            .as_ref()
            .map(|artifact| artifact.infer(features, semantic_task, state.rollout.operation_limit));
        let candidate_inference = state
            .candidate
            .as_ref()
            .map(|artifact| artifact.infer(features, semantic_task, state.rollout.operation_limit));
        let bucket = stable_bucket(tenant_key, task_key);
        let active_revision = state
            .active
            .as_ref()
            .map(|artifact| artifact.artifact_revision.clone());
        let candidate_revision = state
            .candidate
            .as_ref()
            .map(|artifact| artifact.artifact_revision.clone());
        let shadow_tier = if state.rollout.shadow {
            candidate_inference
                .as_ref()
                .and_then(|inference| inference.as_ref().ok())
                .map(|inference| inference.tier.clone())
        } else {
            None
        };
        if state.observations >= state.rollout.minimum_samples
            && bucket < state.rollout.canary_basis_points
            && let Some(Ok(candidate)) = candidate_inference
        {
            return ArtifactDecision {
                applied_tier: candidate.tier,
                source: DecisionSource::CandidateCanary,
                active_revision,
                candidate_revision,
                shadow_tier,
                fallback_reason: None,
                canary_bucket: Some(bucket),
            };
        }
        match active_inference {
            Some(Ok(active)) => ArtifactDecision {
                applied_tier: active.tier,
                source: DecisionSource::ActiveArtifact,
                active_revision,
                candidate_revision,
                shadow_tier,
                fallback_reason: None,
                canary_bucket: Some(bucket),
            },
            Some(Err(error)) => ArtifactDecision {
                fallback_reason: Some(error.to_string()),
                ..rule_decision(rule_tier, "artifact_inference_failed")
            },
            None => rule_decision(rule_tier, "no_active_artifact"),
        }
    }

    #[must_use]
    pub fn observe_and_maybe_rollback(
        &self,
        observation: CanaryObservation,
        thresholds: RollbackThresholds,
    ) -> bool {
        let mut state = self.inner.write().expect("artifact lock poisoned");
        state.observations = state.observations.saturating_add(observation.samples);
        let reason = if observation.quality_delta_lower_millionths
            < thresholds.minimum_quality_delta_lower_millionths
        {
            Some("quality_threshold")
        } else if observation.error_rate_millionths > thresholds.maximum_error_rate_millionths {
            Some("error_threshold")
        } else if observation.cost_regression_millionths
            > thresholds.maximum_cost_regression_millionths
        {
            Some("cost_threshold")
        } else if observation.p95_latency_regression_millionths
            > thresholds.maximum_p95_latency_regression_millionths
        {
            Some("latency_threshold")
        } else {
            None
        };
        let Some(reason) = reason else {
            return false;
        };
        let Some(previous) = state.history.pop_front() else {
            audit(&mut state, "rollback_unavailable", None, reason);
            return false;
        };
        if let Some(active) = state.active.replace(previous) {
            state.history.push_front(active);
        }
        let active = state.active.clone();
        audit(&mut state, "auto_rollback", active.as_ref(), reason);
        true
    }

    #[must_use]
    pub fn status(&self) -> ArtifactStatus {
        let state = self.inner.read().expect("artifact lock poisoned");
        ArtifactStatus {
            active_revision: state
                .active
                .as_ref()
                .map(|artifact| artifact.artifact_revision.clone()),
            candidate_revision: state
                .candidate
                .as_ref()
                .map(|artifact| artifact.artifact_revision.clone()),
            killed: state.killed,
            rollout: state.rollout.clone(),
            observations: state.observations,
            audit_events: state.audit.len(),
        }
    }

    #[must_use]
    pub fn audit_events(&self) -> Vec<ArtifactAuditEvent> {
        self.inner
            .read()
            .expect("artifact lock poisoned")
            .audit
            .clone()
    }
}

fn rule_decision(tier: &str, reason: &str) -> ArtifactDecision {
    ArtifactDecision {
        applied_tier: tier.to_owned(),
        source: DecisionSource::Rule,
        active_revision: None,
        candidate_revision: None,
        shadow_tier: None,
        fallback_reason: Some(reason.to_owned()),
        canary_bucket: None,
    }
}

fn stable_bucket(tenant_key: &str, task_key: &str) -> u16 {
    let digest = Sha256::digest(format!("{tenant_key}\n{task_key}").as_bytes());
    u16::from_be_bytes([digest[0], digest[1]]) % 10_000
}

fn audit(
    state: &mut ControllerState,
    action: &str,
    artifact: Option<&RouterArtifact>,
    reason: &str,
) {
    state.audit.push(ArtifactAuditEvent {
        sequence: state.audit.len() as u64 + 1,
        action: action.to_owned(),
        revision: artifact.map(|artifact| artifact.artifact_revision.clone()),
        reason: reason.to_owned(),
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryObservation {
    pub samples: u64,
    pub quality_delta_lower_millionths: i64,
    pub error_rate_millionths: u32,
    pub cost_regression_millionths: i64,
    pub p95_latency_regression_millionths: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackThresholds {
    pub minimum_quality_delta_lower_millionths: i64,
    pub maximum_error_rate_millionths: u32,
    pub maximum_cost_regression_millionths: i64,
    pub maximum_p95_latency_regression_millionths: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Primary,
    Judge,
    Escalation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepBudget {
    pub maximum_depth: u8,
    pub maximum_calls: u8,
    pub maximum_cost_nano_usd: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepState {
    pub depth: u8,
    pub calls: u8,
    pub spent_nano_usd: u64,
    pub judge_used: bool,
    pub cancelled: bool,
}

impl StepBudget {
    pub fn authorize(
        self,
        state: StepState,
        kind: StepKind,
        estimated_cost_nano_usd: u64,
    ) -> Result<StepState, StepError> {
        if state.cancelled {
            return Err(StepError::Cancelled);
        }
        if state.depth >= self.maximum_depth || state.calls >= self.maximum_calls {
            return Err(StepError::DepthOrCallLimit);
        }
        if kind == StepKind::Judge && state.judge_used {
            return Err(StepError::RecursiveJudge);
        }
        let spent = state
            .spent_nano_usd
            .checked_add(estimated_cost_nano_usd)
            .ok_or(StepError::CostLimit)?;
        if spent > self.maximum_cost_nano_usd {
            return Err(StepError::CostLimit);
        }
        Ok(StepState {
            depth: state.depth.saturating_add(1),
            calls: state.calls.saturating_add(1),
            spent_nano_usd: spent,
            judge_used: state.judge_used || kind == StepKind::Judge,
            cancelled: false,
        })
    }
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact schema version is unsupported")]
    UnsupportedSchema,
    #[error("artifact feature schema is incompatible")]
    FeatureSchemaMismatch,
    #[error("artifact Catalog revision is incompatible")]
    CatalogRevisionMismatch,
    #[error("artifact Route revision is incompatible")]
    RouteRevisionMismatch,
    #[error("artifact export gates did not pass")]
    ExportGateFailed,
    #[error("artifact content hash mismatch")]
    ContentHashMismatch,
    #[error("artifact signature is required")]
    MissingSignature,
    #[error("artifact signature is invalid")]
    InvalidSignature,
    #[error("artifact signing key is empty")]
    EmptySigningKey,
    #[error("artifact rollout requires canary basis points <= 10000 and operation limit >= 6")]
    InvalidRollout,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("artifact inference operation budget was exceeded")]
    OperationBudgetExceeded,
    #[error("request is outside the artifact support domain")]
    OutOfSupportDomain,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StepError {
    #[error("step execution was cancelled")]
    Cancelled,
    #[error("step depth or call limit was reached")]
    DepthOrCallLimit,
    #[error("Judge cannot recursively invoke Judge")]
    RecursiveJudge,
    #[error("step cost budget was exceeded")]
    CostLimit,
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> String {
    let mut normalized = [0_u8; HMAC_BLOCK_BYTES];
    if key.len() > HMAC_BLOCK_BYTES {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; HMAC_BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; HMAC_BLOCK_BYTES];
    for index in 0..HMAC_BLOCK_BYTES {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let inner = Sha256::new()
        .chain_update(inner_pad)
        .chain_update(message)
        .finalize();
    let output = Sha256::new()
        .chain_update(outer_pad)
        .chain_update(inner)
        .finalize();
    format!("hmac-sha256:{output:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(seed: u64) -> RouterArtifact {
        RouterArtifact::build(
            1,
            "catalog-1",
            "route-1",
            "dataset-1",
            seed,
            1_000,
            LinearPolicy {
                baseline_tier: "efficient".to_owned(),
                promoted_tier: "capable".to_owned(),
                threshold_millis: 500,
                bias_millis: 0,
                weights: FeatureWeights {
                    input_kib_millis: 10,
                    message_millis: 5,
                    tool_millis: 600,
                    image_millis: 600,
                    structured_millis: 400,
                    reasoning_millis: 700,
                },
            },
            ArtifactSupportDomain {
                semantic_tasks: BTreeSet::from(["greeting".to_owned(), "general".to_owned()]),
                maximum_input_text_bytes: 100_000,
                tools_supported: true,
            },
            ExportGates {
                reproducible: true,
                privacy_passed: true,
                support_domain_defined: true,
                counterfactual_passed: true,
                quality_lower_bound_millionths: 0,
                maximum_error_rate_millionths: 10_000,
                maximum_cost_regression_millionths: 0,
            },
        )
        .unwrap()
    }

    fn features(tools: u32) -> FeatureFrame {
        let mut frame = FeatureFrame::from_openai_chat(&serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}]
        }));
        frame.available_tool_count = tools;
        frame
    }

    #[test]
    fn artifact_hash_signature_revision_and_gates_are_verified_before_load() {
        let mut artifact = artifact(7);
        artifact.sign(b"test-key").unwrap();
        assert!(
            artifact
                .verify(1, "catalog-1", "route-1", Some(b"test-key"))
                .is_ok()
        );
        assert!(matches!(
            artifact.verify(1, "catalog-2", "route-1", Some(b"test-key")),
            Err(ArtifactError::CatalogRevisionMismatch)
        ));
        artifact.model.bias_millis = 1;
        assert!(matches!(
            artifact.verify(1, "catalog-1", "route-1", Some(b"test-key")),
            Err(ArtifactError::ContentHashMismatch)
        ));
    }

    #[test]
    fn inference_is_bounded_and_falls_outside_support() {
        let artifact = artifact(7);
        assert_eq!(
            artifact.infer(&features(0), "greeting", 6).unwrap().tier,
            "efficient"
        );
        assert_eq!(
            artifact.infer(&features(1), "greeting", 6).unwrap().tier,
            "capable"
        );
        assert!(matches!(
            artifact.infer(&features(0), "weather", 6),
            Err(InferenceError::OutOfSupportDomain)
        ));
        assert!(matches!(
            artifact.infer(&features(0), "greeting", 5),
            Err(InferenceError::OperationBudgetExceeded)
        ));
    }

    #[test]
    fn shadow_canary_kill_switch_and_manual_rollback_preserve_rule_fallback() {
        let active = artifact(1);
        let candidate = artifact(2);
        let controller = ArtifactController::new(
            Some(active.clone()),
            RolloutPolicy {
                shadow: true,
                canary_basis_points: 10_000,
                minimum_samples: 0,
                operation_limit: 6,
            },
        );
        controller.load_candidate(candidate.clone());
        let decision =
            controller.decide(&features(1), "greeting", "tenant-a", "task-a", "efficient");
        assert_eq!(decision.source, DecisionSource::CandidateCanary);
        assert_eq!(decision.shadow_tier.as_deref(), Some("capable"));
        assert!(controller.promote("approved"));
        assert!(controller.rollback("manual drill"));
        controller.set_kill_switch(true, "operator drill");
        assert_eq!(
            controller
                .decide(&features(1), "greeting", "tenant-a", "task-a", "efficient")
                .source,
            DecisionSource::Rule
        );
        assert!(controller.status().killed);
    }

    #[test]
    fn hard_canary_threshold_triggers_audited_auto_rollback() {
        let controller = ArtifactController::new(
            Some(artifact(1)),
            RolloutPolicy {
                shadow: false,
                canary_basis_points: 100,
                minimum_samples: 10,
                operation_limit: 6,
            },
        );
        controller.load_candidate(artifact(2));
        assert!(controller.promote("stage 1"));
        assert!(controller.observe_and_maybe_rollback(
            CanaryObservation {
                samples: 20,
                quality_delta_lower_millionths: -20_000,
                error_rate_millionths: 0,
                cost_regression_millionths: 0,
                p95_latency_regression_millionths: 0,
            },
            RollbackThresholds {
                minimum_quality_delta_lower_millionths: -10_000,
                maximum_error_rate_millionths: 10_000,
                maximum_cost_regression_millionths: 10_000,
                maximum_p95_latency_regression_millionths: 10_000,
            }
        ));
        assert!(
            controller
                .audit_events()
                .iter()
                .any(|event| event.action == "auto_rollback")
        );
    }

    #[test]
    fn step_budget_prevents_recursive_judge_depth_cost_and_cancellation() {
        let budget = StepBudget {
            maximum_depth: 1,
            maximum_calls: 1,
            maximum_cost_nano_usd: 100,
        };
        let initial = StepState {
            depth: 0,
            calls: 0,
            spent_nano_usd: 0,
            judge_used: false,
            cancelled: false,
        };
        let judged = budget.authorize(initial, StepKind::Judge, 50).unwrap();
        assert_eq!(
            budget.authorize(judged, StepKind::Judge, 1).unwrap_err(),
            StepError::DepthOrCallLimit
        );
        assert_eq!(
            StepBudget {
                maximum_depth: 2,
                maximum_calls: 2,
                maximum_cost_nano_usd: 100,
            }
            .authorize(judged, StepKind::Judge, 1)
            .unwrap_err(),
            StepError::RecursiveJudge
        );
        assert_eq!(
            budget
                .authorize(
                    StepState {
                        cancelled: true,
                        ..initial
                    },
                    StepKind::Escalation,
                    1
                )
                .unwrap_err(),
            StepError::Cancelled
        );
    }

    #[test]
    fn stable_canary_bucket_keeps_same_task_in_one_group() {
        assert_eq!(
            stable_bucket("tenant-a", "task-a"),
            stable_bucket("tenant-a", "task-a")
        );
        assert_ne!(
            stable_bucket("tenant-a", "task-a"),
            stable_bucket("tenant-a", "task-b")
        );
    }
}
