use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use urouter_ai::{
    admission::{AdmissionResult, eligible_models},
    capabilities::{CapabilityRequirement, Modality, ThinkingSupport},
    catalog::CatalogSnapshot,
};
use urouter_types::ModelId;

pub use urouter_contracts::{FallbackCause, RetryPolicy, UpstreamErrorKind};
use urouter_contracts::{
    RuleEvaluation, RuleOutcome, TierCandidate, TierDecisionError, TierDecisionInput,
    TierSelection, select_tier_with_cascade,
};

pub mod capacity;
pub mod circuit;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteDeployment {
    pub id: String,
    pub model: ModelId,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub order: u16,
    #[serde(default)]
    pub provider_scope: Option<String>,
    #[serde(default)]
    pub credential_scope: Option<String>,
    #[serde(default = "bool_true")]
    pub enabled: bool,
    #[serde(default = "bool_true")]
    pub credential_available: bool,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub residency: Vec<String>,
    #[serde(default)]
    pub tenant_allowlist: Vec<String>,
    #[serde(default)]
    pub quota_usage_millis: Option<u16>,
    #[serde(default = "bool_true")]
    pub accept_new_requests: bool,
    #[serde(default)]
    pub binding_grace_until_unix: Option<u64>,
}

const fn default_weight() -> u32 {
    1
}

const fn bool_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierConfig {
    pub tier: String,
    pub model: ModelId,
    #[serde(default)]
    pub deployments: Vec<RouteDeployment>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default)]
    pub fallbacks_by_error: BTreeMap<FallbackCause, Vec<String>>,
}

impl TierConfig {
    #[must_use]
    pub fn fallbacks_for(&self, cause: FallbackCause) -> &[String] {
        self.fallbacks_by_error
            .get(&cause)
            .map_or(&self.fallbacks, Vec::as_slice)
    }

    fn all_fallbacks(&self) -> impl Iterator<Item = &String> {
        self.fallbacks
            .iter()
            .chain(self.fallbacks_by_error.values().flatten())
    }

    #[must_use]
    pub fn effective_deployments(&self) -> Vec<RouteDeployment> {
        if self.deployments.is_empty() {
            vec![RouteDeployment {
                id: format!("{}:primary", self.tier),
                model: self.model.clone(),
                base_url: None,
                weight: 1,
                order: 0,
                provider_scope: None,
                credential_scope: None,
                enabled: true,
                credential_available: true,
                region: None,
                residency: Vec::new(),
                tenant_allowlist: Vec::new(),
                quota_usage_millis: None,
                accept_new_requests: true,
                binding_grace_until_unix: None,
            }]
        } else {
            self.deployments.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub id: String,
    pub tiers: Vec<TierConfig>,
    #[serde(default)]
    pub default_preference_bias_millis: i32,
    #[serde(default)]
    pub long_context_quality_threshold_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteCapabilities {
    pub context_window: u64,
    pub input_modalities: BTreeSet<Modality>,
    pub tool_calling: bool,
    pub structured_output: bool,
    pub reasoning: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceContract {
    pub id: Option<String>,
    pub conversation: Option<String>,
    pub branch: Option<String>,
    pub turn: Option<String>,
    pub parent_turn: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskContract {
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentContract {
    pub harness: Option<String>,
    pub prompt_profile_hash: Option<String>,
    pub toolset_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallRole {
    Primary,
    Auxiliary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationBoundary {
    NewTask,
    AfterCompaction,
    ToolRoundCompleted,
    BeforeFirstAssistantToken,
    ExplicitUserRetry,
    TerminalProviderFailure,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallContract {
    pub role: Option<CallRole>,
    pub migration_boundary: Option<MigrationBoundary>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingMode {
    None,
    #[default]
    MetadataOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataPolicyContract {
    pub recording: RecordingMode,
    pub allow_training: bool,
    pub allow_remote_judge: bool,
    pub allow_exploration: bool,
    pub exploration_budget_nano_usd: u64,
    pub retention_days: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingPolicyContract {
    pub region: Option<String>,
    pub residency: Option<String>,
}

impl Default for DataPolicyContract {
    fn default() -> Self {
        Self {
            recording: RecordingMode::MetadataOnly,
            allow_training: false,
            allow_remote_judge: false,
            allow_exploration: false,
            exploration_budget_nano_usd: 0,
            retention_days: 7,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HintContract {
    pub value_class: Option<String>,
    pub workload: Option<String>,
    pub difficulty: Option<String>,
    pub retry_of: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreferenceContract {
    pub bias: Option<serde_json::Number>,
    pub floor_tier: Option<String>,
    pub pin_tier: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignalContract {
    pub turn: Option<String>,
    pub kind: Option<String>,
    pub strength: Option<serde_json::Number>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestContract {
    pub contract_version: Option<u16>,
    pub task: TaskContract,
    pub agent: AgentContract,
    pub call: CallContract,
    pub data_policy: Option<DataPolicyContract>,
    pub policy: RoutingPolicyContract,
    pub trace: TraceContract,
    pub hint: HintContract,
    pub preference: PreferenceContract,
    pub signals: Vec<SignalContract>,
}

impl RequestContract {
    #[must_use]
    pub const fn is_primary_call(&self) -> bool {
        !matches!(self.call.role, Some(CallRole::Auxiliary))
    }

    #[must_use]
    pub fn compatibility_mode(&self) -> bool {
        self.task.id.is_none()
            || self.agent.harness.is_none()
            || self.call.role.is_none()
            || self.trace.turn.is_none()
            || self.data_policy.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteDecision {
    pub route_id: String,
    pub tier: String,
    pub model: ModelId,
    pub reason: String,
    pub semantic: SemanticClassification,
    pub cascade_trace: Vec<RuleEvaluation>,
    pub alternatives: Vec<ModelId>,
    pub requirement: CapabilityRequirement,
    pub admission: AdmissionResult,
    pub trace_turn: Option<String>,
    pub trace_id: Option<String>,
    pub parent_turn: Option<String>,
    pub task_id: Option<String>,
    pub conversation_id: Option<String>,
    pub branch_id: Option<String>,
    pub agent_harness: Option<String>,
    pub prompt_profile_hash: Option<String>,
    pub toolset_hash: Option<String>,
    pub call_role: Option<CallRole>,
    pub migration_boundary: Option<MigrationBoundary>,
    pub data_policy: Option<DataPolicyContract>,
    pub policy: RoutingPolicyContract,
    pub compatibility_mode: bool,
    pub signals: Vec<SignalContract>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticTask {
    Greeting,
    RealtimeWeather,
    EquationSolving,
    General,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticClassification {
    pub task: SemanticTask,
    pub confidence_millis: u16,
    pub abstained: bool,
    pub requires_tools: bool,
    pub requires_reasoning: bool,
    pub required_tool: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RouteError {
    #[error("route id and tier names must not be empty")]
    EmptyName,
    #[error("route must contain at least one tier")]
    EmptyRoute,
    #[error("route contains duplicate tier: {0}")]
    DuplicateTier(String),
    #[error("route references unknown model: {0}")]
    UnknownModel(ModelId),
    #[error("route contains duplicate deployment: {0}")]
    DuplicateDeployment(String),
    #[error("deployment id must not be empty")]
    EmptyDeployment,
    #[error("deployment weight must be greater than zero: {0}")]
    ZeroDeploymentWeight(String),
    #[error("deployment base URL is invalid: {0}")]
    InvalidDeploymentUrl(String),
    #[error("deployment failure scope is invalid: {0}")]
    InvalidFailureScope(String),
    #[error("deployment policy metadata is invalid: {0}")]
    InvalidPolicyMetadata(String),
    #[error("deployment model capabilities differ within tier: {0}")]
    InconsistentTierCapabilities(String),
    #[error("tier fallback references unknown tier: {0}")]
    UnknownFallbackTier(String),
    #[error("tier fallback graph contains a cycle at: {0}")]
    FallbackCycle(String),
    #[error("default preference bias must be in [-1000, 1000]")]
    InvalidDefaultBias,
    #[error("request model is missing or invalid")]
    InvalidRequestModel,
    #[error("urouter contract version must be 1 or 2")]
    UnsupportedContractVersion,
    #[error("invalid urouter contract: {0}")]
    InvalidContract(String),
    #[error("request exceeds the Auto route capability contract: {0}")]
    AutoCapabilityViolation(String),
    #[error("requested tier does not exist: {0}")]
    UnknownTier(String),
    #[error("no configured tier is eligible for the request")]
    NoEligibleTier,
    #[error("semantic task requires an available host tool: {0}")]
    RequiredToolUnavailable(String),
}

impl RouteConfig {
    #[must_use]
    pub fn revision(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("RouteConfig serialization cannot fail");
        format!("sha256:{:x}", Sha256::digest(encoded))
    }

    pub fn validate(&self, catalog: &CatalogSnapshot) -> Result<(), RouteError> {
        if self.id.trim().is_empty() || self.tiers.iter().any(|tier| tier.tier.trim().is_empty()) {
            return Err(RouteError::EmptyName);
        }
        if self.tiers.is_empty() {
            return Err(RouteError::EmptyRoute);
        }
        if !(-1_000..=1_000).contains(&self.default_preference_bias_millis) {
            return Err(RouteError::InvalidDefaultBias);
        }
        let mut names = BTreeSet::new();
        let mut deployment_ids = BTreeSet::new();
        for tier in &self.tiers {
            if !names.insert(&tier.tier) {
                return Err(RouteError::DuplicateTier(tier.tier.clone()));
            }
            if catalog.model(&tier.model).is_none() {
                return Err(RouteError::UnknownModel(tier.model.clone()));
            }
            let representative = catalog
                .model(&tier.model)
                .ok_or_else(|| RouteError::UnknownModel(tier.model.clone()))?;
            for deployment in tier.effective_deployments() {
                if deployment.id.trim().is_empty() {
                    return Err(RouteError::EmptyDeployment);
                }
                if !deployment_ids.insert(deployment.id.clone()) {
                    return Err(RouteError::DuplicateDeployment(deployment.id));
                }
                if deployment.weight == 0 {
                    return Err(RouteError::ZeroDeploymentWeight(deployment.id));
                }
                for scope in [
                    deployment.provider_scope.as_deref(),
                    deployment.credential_scope.as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    if scope.trim().is_empty()
                        || scope.len() > 128
                        || scope.chars().any(char::is_control)
                    {
                        return Err(RouteError::InvalidFailureScope(deployment.id.clone()));
                    }
                }
                if deployment
                    .region
                    .as_ref()
                    .is_some_and(|value| value.trim().is_empty())
                    || deployment
                        .residency
                        .iter()
                        .any(|value| value.trim().is_empty())
                    || deployment
                        .tenant_allowlist
                        .iter()
                        .any(|value| value.trim().is_empty())
                {
                    return Err(RouteError::InvalidPolicyMetadata(deployment.id));
                }
                if deployment
                    .quota_usage_millis
                    .is_some_and(|value| value > 1_000)
                {
                    return Err(RouteError::InvalidPolicyMetadata(deployment.id));
                }
                let model = catalog
                    .model(&deployment.model)
                    .ok_or_else(|| RouteError::UnknownModel(deployment.model.clone()))?;
                if model.capabilities != representative.capabilities {
                    return Err(RouteError::InconsistentTierCapabilities(tier.tier.clone()));
                }
                if let Some(base_url) = &deployment.base_url {
                    let url = url::Url::parse(base_url)
                        .map_err(|_| RouteError::InvalidDeploymentUrl(deployment.id.clone()))?;
                    if !matches!(url.scheme(), "http" | "https") {
                        return Err(RouteError::InvalidDeploymentUrl(deployment.id));
                    }
                }
            }
        }
        validate_fallbacks(&self.tiers)?;
        Ok(())
    }

    pub fn capabilities(&self, catalog: &CatalogSnapshot) -> Result<RouteCapabilities, RouteError> {
        self.validate(catalog)?;
        let mut models = self
            .tiers
            .iter()
            .map(|tier| {
                catalog
                    .model(&tier.model)
                    .ok_or_else(|| RouteError::UnknownModel(tier.model.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter();
        let first = models.next().ok_or(RouteError::EmptyRoute)?;
        let mut result = RouteCapabilities {
            context_window: first.capabilities.context_window,
            input_modalities: first.capabilities.input_modalities.clone(),
            tool_calling: first.capabilities.tool_calling,
            structured_output: first.capabilities.structured_output,
            reasoning: !matches!(first.capabilities.reasoning, ThinkingSupport::Unsupported),
        };
        for model in models {
            result.context_window = result.context_window.min(model.capabilities.context_window);
            result.input_modalities = result
                .input_modalities
                .intersection(&model.capabilities.input_modalities)
                .cloned()
                .collect();
            result.tool_calling &= model.capabilities.tool_calling;
            result.structured_output &= model.capabilities.structured_output;
            result.reasoning &=
                !matches!(model.capabilities.reasoning, ThinkingSupport::Unsupported);
        }
        Ok(result)
    }

    pub fn routable_capabilities(
        &self,
        catalog: &CatalogSnapshot,
    ) -> Result<RouteCapabilities, RouteError> {
        self.validate(catalog)?;
        let mut result = RouteCapabilities {
            context_window: 0,
            input_modalities: BTreeSet::new(),
            tool_calling: false,
            structured_output: false,
            reasoning: false,
        };
        for tier in &self.tiers {
            let model = catalog
                .model(&tier.model)
                .ok_or_else(|| RouteError::UnknownModel(tier.model.clone()))?;
            result.context_window = result.context_window.max(model.capabilities.context_window);
            result
                .input_modalities
                .extend(model.capabilities.input_modalities.iter().cloned());
            result.tool_calling |= model.capabilities.tool_calling;
            result.structured_output |= model.capabilities.structured_output;
            result.reasoning |=
                !matches!(model.capabilities.reasoning, ThinkingSupport::Unsupported);
        }
        Ok(result)
    }

    pub fn decide(
        &self,
        catalog: &CatalogSnapshot,
        request: &Value,
    ) -> Result<RouteDecision, RouteError> {
        self.validate(catalog)?;
        let requested_model = request
            .get("model")
            .and_then(Value::as_str)
            .ok_or(RouteError::InvalidRequestModel)?;
        let contract = parse_contract(request)?;
        let compatibility_mode = contract.compatibility_mode();
        let (semantic, requirement) = semantic_requirement(request)?;
        if requested_model != self.id {
            let requested =
                ModelId::new(requested_model).map_err(|_| RouteError::InvalidRequestModel)?;
            let model = catalog
                .model(&requested)
                .ok_or_else(|| RouteError::UnknownModel(requested.clone()))?;
            let admission = eligible_models(catalog, &requirement);
            if !admission.eligible.contains(&model.id) {
                return Err(RouteError::NoEligibleTier);
            }
            return Ok(RouteDecision {
                route_id: self.id.clone(),
                tier: "pinned".to_owned(),
                model: model.id.clone(),
                reason: "explicit_model".to_owned(),
                semantic,
                cascade_trace: vec![selected_rule("explicit_model", "explicit_model")],
                alternatives: Vec::new(),
                requirement,
                admission,
                trace_turn: contract.trace.turn,
                trace_id: contract.trace.id,
                parent_turn: contract.trace.parent_turn,
                task_id: contract.task.id,
                conversation_id: contract.trace.conversation,
                branch_id: contract.trace.branch,
                agent_harness: contract.agent.harness,
                prompt_profile_hash: contract.agent.prompt_profile_hash,
                toolset_hash: contract.agent.toolset_hash,
                call_role: contract.call.role,
                migration_boundary: contract.call.migration_boundary,
                data_policy: contract.data_policy,
                policy: contract.policy,
                compatibility_mode,
                signals: contract.signals,
            });
        }

        let admission = eligible_models(catalog, &requirement);
        let eligible = self
            .tiers
            .iter()
            .enumerate()
            .filter(|(_, tier)| admission.eligible.contains(&tier.model))
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            return Err(RouteError::NoEligibleTier);
        }
        let mut selection = select_tier(self, &contract, request, &eligible)?;
        let selected_index = selection.index;
        let mut reason = selection.reason;
        if eligible.len() < self.tiers.len()
            && eligible
                .first()
                .is_some_and(|(index, _)| *index == selected_index)
        {
            "capability_required".clone_into(&mut reason);
            selection
                .evaluations
                .push(selected_rule("capability_filter", "capability_required"));
        }
        let selected = &self.tiers[selected_index];
        let alternatives = eligible
            .iter()
            .filter(|(index, _)| *index != selected_index)
            .map(|(_, tier)| tier.model.clone())
            .collect();
        Ok(RouteDecision {
            route_id: self.id.clone(),
            tier: selected.tier.clone(),
            model: selected.model.clone(),
            reason,
            semantic,
            cascade_trace: selection.evaluations,
            alternatives,
            requirement,
            admission,
            trace_turn: contract.trace.turn,
            trace_id: contract.trace.id,
            parent_turn: contract.trace.parent_turn,
            task_id: contract.task.id,
            conversation_id: contract.trace.conversation,
            branch_id: contract.trace.branch,
            agent_harness: contract.agent.harness,
            prompt_profile_hash: contract.agent.prompt_profile_hash,
            toolset_hash: contract.agent.toolset_hash,
            call_role: contract.call.role,
            migration_boundary: contract.call.migration_boundary,
            data_policy: contract.data_policy,
            policy: contract.policy,
            compatibility_mode,
            signals: contract.signals,
        })
    }

    pub fn bind_decision(
        &self,
        catalog: &CatalogSnapshot,
        mut decision: RouteDecision,
        model_id: &ModelId,
    ) -> Result<RouteDecision, RouteError> {
        if !decision.admission.eligible.contains(model_id) {
            return Err(RouteError::NoEligibleTier);
        }
        let tier = self
            .tiers
            .iter()
            .find(|tier| {
                tier.model == *model_id
                    || tier
                        .effective_deployments()
                        .iter()
                        .any(|deployment| deployment.model == *model_id)
            })
            .ok_or_else(|| RouteError::UnknownModel(model_id.clone()))?;
        if catalog.model(model_id).is_none() {
            return Err(RouteError::UnknownModel(model_id.clone()));
        }
        decision.tier.clone_from(&tier.tier);
        decision.model.clone_from(model_id);
        decision.alternatives = self
            .tiers
            .iter()
            .map(|tier| tier.model.clone())
            .filter(|model| model != model_id && decision.admission.eligible.contains(model))
            .collect();
        "task_binding".clone_into(&mut decision.reason);
        decision
            .cascade_trace
            .push(selected_rule("task_binding", "task_binding"));
        Ok(decision)
    }
}

fn selected_rule(rule: &str, reason: &str) -> RuleEvaluation {
    RuleEvaluation {
        rule: rule.to_owned(),
        outcome: RuleOutcome::Selected,
        reason: reason.to_owned(),
    }
}

fn validate_fallbacks(tiers: &[TierConfig]) -> Result<(), RouteError> {
    let indexes = tiers
        .iter()
        .enumerate()
        .map(|(index, tier)| (tier.tier.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    for tier in tiers {
        for fallback in tier.all_fallbacks() {
            if !indexes.contains_key(fallback.as_str()) {
                return Err(RouteError::UnknownFallbackTier(fallback.clone()));
            }
        }
    }
    let mut states = vec![0_u8; tiers.len()];
    for index in 0..tiers.len() {
        visit_fallback(index, tiers, &indexes, &mut states)?;
    }
    Ok(())
}

fn visit_fallback(
    index: usize,
    tiers: &[TierConfig],
    indexes: &BTreeMap<&str, usize>,
    states: &mut [u8],
) -> Result<(), RouteError> {
    if states[index] == 2 {
        return Ok(());
    }
    if states[index] == 1 {
        return Err(RouteError::FallbackCycle(tiers[index].tier.clone()));
    }
    states[index] = 1;
    for fallback in tiers[index].all_fallbacks() {
        visit_fallback(indexes[fallback.as_str()], tiers, indexes, states)?;
    }
    states[index] = 2;
    Ok(())
}

fn parse_contract(request: &Value) -> Result<RequestContract, RouteError> {
    let Some(value) = request.get("urouter") else {
        return Ok(RequestContract::default());
    };
    let contract: RequestContract = serde_json::from_value(value.clone())
        .map_err(|error| RouteError::InvalidContract(error.to_string()))?;
    if contract
        .contract_version
        .is_some_and(|version| !matches!(version, 1 | 2))
    {
        return Err(RouteError::UnsupportedContractVersion);
    }
    if contract
        .data_policy
        .as_ref()
        .is_some_and(|policy| !(1..=365).contains(&policy.retention_days))
    {
        return Err(RouteError::InvalidContract(
            "data_policy.retention_days must be in 1..=365".to_owned(),
        ));
    }
    for (name, value) in [
        ("policy.region", contract.policy.region.as_deref()),
        ("policy.residency", contract.policy.residency.as_deref()),
    ] {
        if value.is_some_and(|value| {
            value.trim().is_empty() || value.len() > 64 || value.chars().any(char::is_control)
        }) {
            return Err(RouteError::InvalidContract(format!(
                "{name} must contain 1..=64 non-control characters"
            )));
        }
    }
    if contract.contract_version == Some(2) && contract.is_primary_call() {
        let required = [
            ("task.id", contract.task.id.as_deref()),
            ("agent.harness", contract.agent.harness.as_deref()),
            (
                "agent.prompt_profile_hash",
                contract.agent.prompt_profile_hash.as_deref(),
            ),
            ("agent.toolset_hash", contract.agent.toolset_hash.as_deref()),
            ("trace.conversation", contract.trace.conversation.as_deref()),
            ("trace.branch", contract.trace.branch.as_deref()),
            ("trace.turn", contract.trace.turn.as_deref()),
        ];
        let missing = required
            .iter()
            .filter_map(|(name, value)| value.is_none().then_some(*name))
            .collect::<Vec<_>>();
        if contract.call.role != Some(CallRole::Primary) {
            return Err(RouteError::InvalidContract(
                "contract v2 primary calls require call.role=primary".to_owned(),
            ));
        }
        if contract.data_policy.is_none() {
            return Err(RouteError::InvalidContract(
                "contract v2 primary calls require data_policy".to_owned(),
            ));
        }
        if !missing.is_empty() {
            return Err(RouteError::InvalidContract(format!(
                "contract v2 primary call is missing {}",
                missing.join(", ")
            )));
        }
        for (name, value) in [
            (
                "agent.prompt_profile_hash",
                contract.agent.prompt_profile_hash.as_deref(),
            ),
            ("agent.toolset_hash", contract.agent.toolset_hash.as_deref()),
        ] {
            let valid = value.is_some_and(|value| {
                value.strip_prefix("sha256:").is_some_and(|hex| {
                    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            });
            if !valid {
                return Err(RouteError::InvalidContract(format!(
                    "{name} must be a sha256: value with 64 hexadecimal characters"
                )));
            }
        }
    }
    if contract.trace.conversation.is_some() != contract.trace.branch.is_some() {
        return Err(RouteError::InvalidContract(
            "trace.conversation and trace.branch must be supplied together".to_owned(),
        ));
    }
    Ok(contract)
}

fn semantic_requirement(
    request: &Value,
) -> Result<(SemanticClassification, CapabilityRequirement), RouteError> {
    let semantic = classify_semantic_task(request);
    if let Some(required) = semantic.required_tool.as_deref()
        && !host_provides_tool(request, required)
    {
        return Err(RouteError::RequiredToolUnavailable(required.to_owned()));
    }
    let requirement = analyze_requirement(request, &semantic);
    Ok((semantic, requirement))
}

fn analyze_requirement(
    request: &Value,
    semantic: &SemanticClassification,
) -> CapabilityRequirement {
    let mut requirement = CapabilityRequirement::default();
    requirement.input_modalities.insert(Modality::Text);
    let mut characters = 0_u64;
    if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        for message in messages {
            analyze_content(message.get("content"), &mut requirement, &mut characters);
        }
    }
    requirement.tool_calling = semantic.requires_tools
        || request
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
    requirement.structured_output = request
        .pointer("/response_format/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "json_schema" | "json_object"));
    requirement.reasoning = semantic.requires_reasoning
        || request
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .is_some_and(|effort| effort != "none")
        || request
            .pointer("/chat_template_kwargs/enable_thinking")
            .and_then(Value::as_bool)
            == Some(true);
    let prompt_tokens = characters.div_ceil(4);
    let output_tokens = request
        .get("max_tokens")
        .or_else(|| request.get("max_completion_tokens"))
        .or_else(|| request.get("max_output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    requirement.min_context_window = prompt_tokens.saturating_add(output_tokens);
    requirement
}

#[must_use]
pub fn classify_semantic_task(request: &Value) -> SemanticClassification {
    let text = latest_user_text(request).to_lowercase();
    let normalized = text.trim_matches(|character: char| {
        character.is_whitespace()
            || matches!(character, '!' | '?' | '.' | ',' | '。' | '！' | '？' | '，')
    });
    if matches!(normalized, "你好" | "您好" | "hello" | "hi" | "hey") {
        return semantic_classification(SemanticTask::Greeting, 980, false, false, None);
    }
    if ["天气", "气温", "weather", "forecast"]
        .iter()
        .any(|keyword| normalized.contains(keyword))
    {
        return semantic_classification(
            SemanticTask::RealtimeWeather,
            950,
            true,
            false,
            Some("weather"),
        );
    }
    if [
        "二元一次方程",
        "方程组",
        "solve the equation",
        "solve equation",
    ]
    .iter()
    .any(|keyword| normalized.contains(keyword))
        || (normalized.contains('=') && normalized.contains('x') && normalized.contains('y'))
    {
        return semantic_classification(SemanticTask::EquationSolving, 930, false, true, None);
    }
    semantic_classification(SemanticTask::General, 0, false, false, None)
}

fn semantic_classification(
    task: SemanticTask,
    confidence_millis: u16,
    requires_tools: bool,
    requires_reasoning: bool,
    required_tool: Option<&str>,
) -> SemanticClassification {
    SemanticClassification {
        task,
        confidence_millis,
        abstained: confidence_millis == 0,
        requires_tools,
        requires_reasoning,
        required_tool: required_tool.map(str::to_owned),
    }
}

fn latest_user_text(request: &Value) -> String {
    request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|message| message.get("content"))
        .map(content_text)
        .unwrap_or_default()
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn host_provides_tool(request: &Value, required: &str) -> bool {
    let declared = request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
        .any(|name| {
            let name = name.to_lowercase();
            name.contains(required) || (required == "weather" && name.contains("天气"))
        });
    declared
        || request
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            })
}

fn analyze_content(
    content: Option<&Value>,
    requirement: &mut CapabilityRequirement,
    characters: &mut u64,
) {
    match content {
        Some(Value::String(text)) => {
            *characters = characters.saturating_add(text.len() as u64);
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("image_url" | "input_image") => {
                        requirement.input_modalities.insert(Modality::Image);
                    }
                    Some("input_audio" | "audio") => {
                        requirement.input_modalities.insert(Modality::Audio);
                    }
                    Some("text" | "input_text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            *characters = characters.saturating_add(text.len() as u64);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn select_tier(
    route: &RouteConfig,
    contract: &RequestContract,
    request: &Value,
    eligible: &[(usize, &TierConfig)],
) -> Result<TierSelection, RouteError> {
    let floor = contract
        .preference
        .floor_tier
        .as_ref()
        .map(|name| {
            route
                .tiers
                .iter()
                .position(|tier| tier.tier == *name)
                .ok_or_else(|| RouteError::UnknownTier(name.clone()))
        })
        .transpose()?
        .unwrap_or(0);
    let auxiliary = contract.call.role == Some(CallRole::Auxiliary);
    let signal_quality = signal_score(
        contract,
        &["severity", "spinning", "exploring", "production_intensity"],
    ) > 500;
    let signal_low_cost = signal_score(contract, &["cost_sensitive", "disposable"]) > 500;
    let long_context_quality = route.long_context_quality_threshold_tokens > 0
        && estimated_input_tokens(request) >= route.long_context_quality_threshold_tokens;
    let high_quality = contract.hint.difficulty.as_deref() == Some("hard")
        || contract.hint.workload.as_deref() == Some("plan")
        || preference_bias_millis(contract, route.default_preference_bias_millis) < -500
        || signal_quality
        || long_context_quality;
    let low_cost = auxiliary
        || matches!(
            contract.hint.value_class.as_deref(),
            Some("auxiliary" | "disposable")
        )
        || preference_bias_millis(contract, route.default_preference_bias_millis) > 500
        || signal_low_cost;
    let mut selection = select_tier_with_cascade(&TierDecisionInput {
        candidates: eligible
            .iter()
            .map(|(index, tier)| TierCandidate {
                index: *index,
                tier: tier.tier.clone(),
            })
            .collect(),
        pin_tier: contract.preference.pin_tier.clone(),
        floor_index: floor,
        auxiliary,
        high_quality,
        low_cost,
    })
    .map_err(|error| match error {
        TierDecisionError::UnknownPinnedTier(tier) => RouteError::UnknownTier(tier),
        TierDecisionError::NoEligibleTier => RouteError::NoEligibleTier,
    })?;
    if long_context_quality {
        selection.evaluations.insert(
            0,
            selected_rule("structural_decider", "long_context_quality"),
        );
    }
    if signal_quality || signal_low_cost {
        selection.evaluations.insert(
            0,
            selected_rule(
                "signal_decider",
                if signal_quality {
                    "signal_quality"
                } else {
                    "signal_low_cost"
                },
            ),
        );
    }
    Ok(selection)
}

fn estimated_input_tokens(request: &Value) -> u64 {
    let mut requirement = CapabilityRequirement::default();
    let mut characters = 0_u64;
    if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        for message in messages {
            analyze_content(message.get("content"), &mut requirement, &mut characters);
        }
    }
    characters.div_ceil(4)
}

fn signal_score(contract: &RequestContract, kinds: &[&str]) -> i32 {
    contract
        .signals
        .iter()
        .filter(|signal| {
            signal
                .kind
                .as_deref()
                .is_some_and(|kind| kinds.contains(&kind))
        })
        .filter_map(|signal| {
            signal
                .strength
                .as_ref()
                .and_then(serde_json::Number::as_f64)
        })
        .map(quantize_bias)
        .max()
        .unwrap_or(0)
}

fn preference_bias_millis(contract: &RequestContract, default: i32) -> i32 {
    contract
        .preference
        .bias
        .as_ref()
        .and_then(serde_json::Number::as_f64)
        .map_or(default, quantize_bias)
}

#[allow(clippy::cast_possible_truncation)]
fn quantize_bias(bias: f64) -> i32 {
    (bias.clamp(-1.0, 1.0) * 1_000.0).round() as i32
}

#[must_use]
pub fn model_map(catalog: &CatalogSnapshot) -> BTreeMap<ModelId, &urouter_ai::ModelSpec> {
    catalog
        .models()
        .map(|model| (model.id.clone(), model))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> CatalogSnapshot {
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap()
    }

    #[test]
    fn retry_policy_only_retries_transient_errors_within_budget() {
        let policy = RetryPolicy {
            max_retries: 2,
            base_backoff_ms: 50,
            max_backoff_ms: 1_000,
        };
        assert!(policy.should_retry(UpstreamErrorKind::Timeout, 0));
        assert!(policy.should_retry(UpstreamErrorKind::ServerError, 1));
        assert!(!policy.should_retry(UpstreamErrorKind::ServerError, 2));
        assert!(!policy.should_retry(UpstreamErrorKind::BadRequest, 0));
        assert_eq!(policy.backoff_ms(0, None), 50);
        assert_eq!(policy.backoff_ms(1, None), 100);
        assert_eq!(policy.backoff_ms(1, Some(750)), 750);
    }

    fn route() -> RouteConfig {
        serde_json::from_str(include_str!("../../../gateway/route.json")).unwrap()
    }

    #[test]
    fn simple_auto_request_selects_efficient() {
        let decision = route()
            .decide(
                &catalog(),
                &json!({"model": "urouter/auto", "messages": [{"role": "user", "content": "hello"}]}),
            )
            .unwrap();
        assert_eq!(decision.tier, "efficient");
        assert_eq!(decision.reason, "default_efficient");
    }

    #[test]
    fn semantic_greeting_stays_efficient() {
        let decision = route()
            .decide(
                &catalog(),
                &json!({"model": "urouter/auto", "messages": [{"role": "user", "content": "你好"}]}),
            )
            .unwrap();
        assert_eq!(decision.tier, "efficient");
        assert_eq!(decision.semantic.task, SemanticTask::Greeting);
        assert!(!decision.semantic.abstained);
    }

    #[test]
    fn semantic_equation_requires_reasoning_model() {
        let decision = route()
            .decide(
                &catalog(),
                &json!({"model": "urouter/auto", "messages": [{"role": "user", "content": "求解二元一次方程组：x+y=5, x-y=1"}]}),
            )
            .unwrap();
        assert_eq!(decision.tier, "capable");
        assert_eq!(decision.semantic.task, SemanticTask::EquationSolving);
        assert!(decision.requirement.reasoning);
    }

    #[test]
    fn semantic_weather_requires_a_real_host_tool() {
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "今天武汉的天气怎么样？"}]
        });
        assert!(matches!(
            route().decide(&catalog(), &request),
            Err(RouteError::RequiredToolUnavailable(tool)) if tool == "weather"
        ));

        let mut with_tool = request;
        with_tool["tools"] = json!([{
            "type": "function",
            "function": {"name": "get_weather", "parameters": {"type": "object"}}
        }]);
        let decision = route().decide(&catalog(), &with_tool).unwrap();
        assert_eq!(decision.tier, "capable");
        assert_eq!(decision.semantic.task, SemanticTask::RealtimeWeather);
        assert!(decision.requirement.tool_calling);

        let continuation = json!({
            "model": "urouter/auto",
            "messages": [
                {"role": "user", "content": "今天武汉的天气怎么样？"},
                {"role": "assistant", "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"武汉\"}"}}]},
                {"role": "tool", "tool_call_id": "call-1", "content": "晴，30摄氏度"}
            ]
        });
        let continued = route().decide(&catalog(), &continuation).unwrap();
        assert_eq!(continued.tier, "capable");
        assert_eq!(continued.semantic.task, SemanticTask::RealtimeWeather);
    }

    #[test]
    fn unknown_semantics_abstain_to_existing_policy() {
        let classification = classify_semantic_task(&json!({
            "messages": [{"role": "user", "content": "整理这段内容"}]
        }));
        assert_eq!(classification.task, SemanticTask::General);
        assert!(classification.abstained);
        assert_eq!(classification.confidence_millis, 0);
    }

    #[test]
    fn hard_request_selects_capable() {
        let decision = route()
            .decide(
                &catalog(),
                &json!({
                    "model": "urouter/auto",
                    "messages": [{"role": "user", "content": "solve this"}],
                    "urouter": {"contract_version": 1, "hint": {"difficulty": "hard"}}
                }),
            )
            .unwrap();
        assert_eq!(decision.tier, "capable");
        assert_eq!(decision.reason, "quality_guard");
    }

    #[test]
    fn configured_long_context_threshold_selects_quality_tier_and_is_traced() {
        let mut route = route();
        route.long_context_quality_threshold_tokens = 8;
        let decision = route
            .decide(
                &catalog(),
                &json!({
                    "model": "urouter/auto",
                    "messages": [{"role": "user", "content": "0123456789abcdef0123456789abcdef"}],
                    "max_tokens": 1
                }),
            )
            .unwrap();
        assert_eq!(decision.tier, "capable");
        assert_eq!(decision.reason, "quality_guard");
        assert_eq!(decision.cascade_trace[0].rule, "structural_decider");
        assert_eq!(decision.cascade_trace[0].reason, "long_context_quality");
    }

    #[test]
    fn high_confidence_operational_signal_selects_quality_tier_and_is_traced() {
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "analyze this"}],
            "urouter": {
                "signals": [{"kind": "severity", "strength": 0.9}]
            }
        });
        let decision = route().decide(&catalog(), &request).unwrap();
        assert_eq!(decision.tier, "capable");
        assert_eq!(decision.cascade_trace[0].rule, "signal_decider");
        assert_eq!(decision.cascade_trace[0].reason, "signal_quality");
    }

    #[test]
    fn m0_ignores_reserved_and_future_contract_fields() {
        let decision = route()
            .decide(
                &catalog(),
                &json!({
                    "model": "urouter/auto",
                    "messages": [{"role": "user", "content": "hello"}],
                    "urouter": {
                        "contract_version": 1,
                        "scope": {"budget": "b:test"},
                        "hint": {"value_class": "auxiliary", "future_hint": true},
                        "observation": {"last_action_ok": true},
                        "signals": []
                    }
                }),
            )
            .unwrap();
        assert_eq!(decision.tier, "efficient");
        assert_eq!(decision.reason, "cost_preference");
    }

    #[test]
    fn auto_routes_capability_outside_baseline_to_an_eligible_tier() {
        let route = route();
        let catalog = catalog();
        assert!(
            !route
                .capabilities(&catalog)
                .unwrap()
                .input_modalities
                .contains(&Modality::Image)
        );
        assert!(
            route
                .routable_capabilities(&catalog)
                .unwrap()
                .input_modalities
                .contains(&Modality::Image)
        );
        let decision = route
            .decide(
                &catalog,
                &json!({
                    "model": "urouter/auto",
                    "messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}]
                }),
            )
            .unwrap();
        assert_eq!(decision.tier, "capable");
        assert_eq!(decision.reason, "capability_required");
    }

    #[test]
    fn explicit_capable_model_accepts_image() {
        let decision = route()
            .decide(
                &catalog(),
                &json!({
                    "model": "local-vllm-qwen38/qwen3.8-27b",
                    "messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}]
                }),
            )
            .unwrap();
        assert_eq!(decision.tier, "pinned");
    }

    #[test]
    fn rejects_fallback_cycles() {
        let mut route = route();
        route
            .tiers
            .iter_mut()
            .find(|tier| tier.tier == "capable")
            .unwrap()
            .fallbacks = vec!["efficient".to_owned()];
        assert!(matches!(
            route.validate(&catalog()),
            Err(RouteError::FallbackCycle(_))
        ));
    }

    #[test]
    fn auxiliary_call_bypasses_high_quality_hint() {
        let decision = route()
            .decide(
                &catalog(),
                &json!({
                    "model": "urouter/auto",
                    "messages": [{"role": "user", "content": "make a title"}],
                    "urouter": {
                        "contract_version": 1,
                        "task": {"id": "task-1"},
                        "agent": {"harness": "aionui"},
                        "call": {"role": "auxiliary"},
                        "data_policy": {
                            "recording": "metadata_only",
                            "allow_training": false,
                            "allow_remote_judge": false,
                            "retention_days": 7
                        },
                        "trace": {"turn": "turn-title"},
                        "hint": {"difficulty": "hard", "workload": "plan"}
                    }
                }),
            )
            .unwrap();
        assert_eq!(decision.tier, "efficient");
        assert_eq!(decision.reason, "cost_preference");
        assert!(!decision.compatibility_mode);
    }

    #[test]
    fn binding_replaces_per_call_selection_with_exact_model() {
        let route = route();
        let catalog = catalog();
        let decision = route
            .decide(
                &catalog,
                &json!({
                    "model": "urouter/auto",
                    "messages": [{"role": "user", "content": "simple"}]
                }),
            )
            .unwrap();
        assert_eq!(decision.tier, "efficient");
        let capable = ModelId::new("local-vllm-qwen38/qwen3.8-27b").unwrap();
        let bound = route.bind_decision(&catalog, decision, &capable).unwrap();
        assert_eq!(bound.tier, "capable");
        assert_eq!(bound.model, capable);
        assert_eq!(bound.reason, "task_binding");
    }

    #[test]
    fn contract_v2_requires_complete_session_identity() {
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "continue"}],
            "urouter": {
                "contract_version": 2,
                "task": {"id": "task-1"},
                "agent": {
                    "harness": "aionui",
                    "prompt_profile_hash": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "toolset_hash": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                },
                "call": {"role": "primary"},
                "trace": {
                    "conversation": "conversation-1",
                    "branch": "main",
                    "turn": "turn-1"
                },
                "data_policy": {"retention_days": 7}
            }
        });
        let decision = route().decide(&catalog(), &request).unwrap();
        assert_eq!(decision.conversation_id.as_deref(), Some("conversation-1"));
        assert_eq!(decision.branch_id.as_deref(), Some("main"));

        let mut incomplete = request;
        incomplete["urouter"]["trace"]
            .as_object_mut()
            .unwrap()
            .remove("branch");
        assert!(matches!(
            route().decide(&catalog(), &incomplete),
            Err(RouteError::InvalidContract(_))
        ));
    }
}
