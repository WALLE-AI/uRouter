use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env,
    fmt::Write as _,
    fs,
    future::{Future, IntoFuture},
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use futures_util::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    sync::{RwLock, broadcast, mpsc, oneshot},
    time::sleep,
};
use urouter_ai::{
    auth::AuthPlan,
    catalog::{CatalogManifest, CatalogSnapshot, ModelSpec},
    compat::MaxTokensField,
    endpoint::EndpointPlan,
    evidence::CatalogEvidence,
    pricing::{CostBreakdown, PriceSource, calculate_actual_cost},
};
use urouter_artifact::{
    ArtifactController, ArtifactDecision, CanaryObservation, DecisionSource, RollbackThresholds,
    RolloutPolicy, RouterArtifact,
};
use urouter_contracts::{
    CapacitySnapshot, DecisionRecordContext, DeploymentDisposition, DeploymentEvaluation,
    DeploymentPicker, FEATURE_SCHEMA_VERSION, FallbackTierSpec, FeatureFrame, RetryDirective,
    RevisionSet, RoutingTrace, RuleEvaluation, RuleOutcome, VectorRef, plan_fallback_tiers,
};
use urouter_gateway::{
    CallRole, DataPolicyContract, FallbackCause, MigrationBoundary, RecordingMode, RetryPolicy,
    RouteConfig, RouteDecision, RouteDeployment, RouteError, SignalContract, TierConfig,
    UpstreamErrorKind,
    capacity::{CapacityError, CapacityLease, CapacityManager, CooldownPolicy},
    circuit::{
        CircuitPermit, LocalCircuitRepository, RedisCircuitRepository, SharedCircuitRepository,
    },
    vector_store::VectorSideStore,
};
use urouter_protocol::{
    LossPolicy, TransportCapabilities, from_anthropic_messages, from_openai_chat,
    from_openai_responses, to_anthropic_messages, to_openai_chat, to_openai_responses,
};
use urouter_types::{ModelId, Usage, WireApi};

mod adapter;
mod binding;
mod budget;
mod control;
mod credential;
mod idempotency;
mod management_auth;
mod quota;
mod record;
mod shared_state;

use adapter::adapt_agent_request;
use binding::{
    BindingWrite, MemoryTaskBindingRepository, RedisTaskBindingRepository, TaskBinding,
    TaskBindingRepository,
};
use budget::{
    BudgetAdmission, BudgetError, BudgetLease, BudgetRepository, MemoryBudgetRepository,
    RedisBudgetRepository,
};
use control::{ControlFailurePolicy, ControlPlane, ControlSnapshot};
use credential::CredentialManager;
use idempotency::{
    IdempotencyClaim, IdempotencyRepository, MemoryIdempotencyRepository,
    RedisIdempotencyRepository,
};
use management_auth::{ManagementAuth, ManagementAuthError, ManagementRole};
use quota::{
    MemoryQuotaRepository, QuotaAdmission, QuotaError, QuotaLease, QuotaRejection, QuotaRepository,
    RedisQuotaRepository,
};
use record::{
    DecisionRecordRepository, MemoryDecisionRecordRepository, RecordDelete, RecordRepositoryError,
    RedisDecisionRecordRepository,
};
use shared_state::RedisSharedState;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Parser)]
#[command(about = "uRouter M0 Auto gateway")]
#[allow(clippy::struct_excessive_bools)]
struct Args {
    #[arg(long, default_value = "catalog/catalog.json")]
    catalog: PathBuf,
    #[arg(long, default_value = "gateway/route.json")]
    route: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    records: Option<PathBuf>,
    #[arg(long)]
    feedback_records: Option<PathBuf>,
    #[arg(long)]
    vector_store: Option<PathBuf>,
    #[arg(long, default_value_t = 268_435_456)]
    vector_shard_max_bytes: u64,
    #[arg(long, default_value_t = 1_000)]
    record_capacity: usize,
    #[arg(long, default_value_t = 256)]
    record_queue_capacity: usize,
    #[arg(long, default_value_t = 67_108_864)]
    record_max_bytes: u64,
    #[arg(long, default_value_t = 5_000)]
    connect_timeout_ms: u64,
    #[arg(long, default_value_t = 120_000)]
    request_timeout_ms: u64,
    #[arg(long, default_value_t = 1)]
    max_retries: u8,
    #[arg(long, default_value_t = 100)]
    retry_base_backoff_ms: u64,
    #[arg(long, default_value_t = 2_000)]
    retry_max_backoff_ms: u64,
    #[arg(long, default_value_t = 5_000)]
    cooldown_ms: u64,
    #[arg(long, default_value_t = 60_000)]
    cooldown_window_ms: u64,
    #[arg(long, default_value_t = 500)]
    cooldown_failure_threshold_millis: u16,
    #[arg(long, default_value_t = 5)]
    max_fallback_depth: u8,
    #[arg(long, default_value = "weighted")]
    deployment_picker: String,
    #[arg(long, default_value_t = 10_000)]
    task_binding_capacity: usize,
    #[arg(long, default_value_t = 10_000)]
    idempotency_capacity: usize,
    #[arg(long, default_value_t = 86_400)]
    idempotency_ttl_seconds: u64,
    #[arg(long, default_value_t = 0)]
    tenant_max_in_flight: usize,
    #[arg(long, default_value_t = 0)]
    tenant_requests_per_minute: usize,
    #[arg(long, default_value_t = 0)]
    tenant_tokens_per_minute: u64,
    #[arg(long, default_value_t = 4_096)]
    quota_default_max_output_tokens: u64,
    #[arg(long, default_value_t = 86_400)]
    quota_lease_ttl_seconds: u64,
    #[arg(long, default_value_t = 0)]
    tenant_budget_nano_usd: u64,
    #[arg(long, default_value_t = 0)]
    tenant_budget_soft_nano_usd: u64,
    #[arg(long, default_value_t = 2_592_000)]
    budget_period_seconds: u64,
    #[arg(long)]
    redis_url: Option<String>,
    #[arg(long)]
    on_state_unavailable: Option<String>,
    #[arg(long, default_value = "urouter")]
    redis_prefix: String,
    #[arg(long, default_value_t = 604_800)]
    task_binding_ttl_seconds: u64,
    #[arg(long, default_value_t = false)]
    require_tenant_header: bool,
    #[arg(long)]
    management_keyring: Option<PathBuf>,
    #[arg(long)]
    management_audit: Option<PathBuf>,
    #[arg(long, default_value_t = 256)]
    management_audit_queue_capacity: usize,
    #[arg(long, default_value_t = 5)]
    management_keyring_reload_seconds: u64,
    #[arg(long, default_value_t = 30)]
    shutdown_grace_seconds: u64,
    #[arg(long)]
    control_manifest: Option<PathBuf>,
    #[arg(long)]
    control_signing_key_env: Option<String>,
    #[arg(long, default_value_t = 5)]
    control_reload_seconds: u64,
    #[arg(long, default_value = "last_good")]
    control_failure_policy: String,
    #[arg(long)]
    control_required_revision: Option<String>,
    #[arg(long)]
    artifact_active: Option<PathBuf>,
    #[arg(long)]
    artifact_candidate: Option<PathBuf>,
    #[arg(long)]
    artifact_signing_key_env: Option<String>,
    #[arg(long, default_value_t = false)]
    artifact_shadow: bool,
    #[arg(long, default_value_t = 0)]
    artifact_canary_basis_points: u16,
    #[arg(long, default_value_t = 100)]
    artifact_minimum_samples: u64,
    #[arg(long, default_value_t = 6)]
    artifact_operation_limit: u32,
    #[arg(long, default_value_t = false)]
    artifact_kill_switch: bool,
    #[arg(long, default_value_t = 0)]
    exploration_epsilon_millionths: u32,
    #[arg(long, default_value_t = 0)]
    exploration_max_budget_nano_usd: u64,
}

#[derive(Clone)]
struct AppState {
    catalog: Arc<CatalogSnapshot>,
    route: Arc<RouteConfig>,
    client: reqwest::Client,
    credentials: CredentialManager,
    records: RecordStore,
    record_repository: Arc<dyn DecisionRecordRepository>,
    feedback: FeedbackStore,
    metrics: GatewayMetrics,
    request_timeout: Duration,
    retry_policy: RetryPolicy,
    capacity: Arc<CapacityManager>,
    shared_circuits: Arc<dyn SharedCircuitRepository>,
    max_fallback_depth: u8,
    bindings: Arc<dyn TaskBindingRepository>,
    idempotency: Arc<dyn IdempotencyRepository>,
    idempotency_ttl_seconds: u64,
    quota: Arc<dyn QuotaRepository>,
    quota_default_max_output_tokens: u64,
    budget: Arc<dyn BudgetRepository>,
    shared_state: Option<RedisSharedState>,
    require_tenant_header: bool,
    management_auth: ManagementAuth,
    accepting: Arc<AtomicBool>,
    control: ControlPlane,
    control_source: Option<ControlSource>,
    artifact: Option<ArtifactRuntime>,
    exploration: ExplorationPolicy,
    cache_affinity: CacheAffinityStore,
    vector_store: Option<Arc<VectorSideStore>>,
}

#[derive(Debug, Clone, Copy)]
struct ExplorationPolicy {
    epsilon_millionths: u32,
    maximum_budget_nano_usd: u64,
}

#[derive(Clone)]
struct CacheAffinityStore {
    inner: Arc<StdMutex<CacheAffinityState>>,
    capacity: usize,
}

#[derive(Default)]
struct CacheAffinityState {
    deployments: BTreeMap<String, String>,
    order: VecDeque<String>,
}

impl CacheAffinityStore {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(StdMutex::new(CacheAffinityState::default())),
            capacity,
        }
    }

    fn preferred(&self, profile_hash: Option<&str>) -> Option<String> {
        let profile_hash = profile_hash?;
        self.inner
            .lock()
            .expect("cache affinity lock poisoned")
            .deployments
            .get(profile_hash)
            .cloned()
    }

    fn remember(&self, profile_hash: Option<&str>, deployment: &str) {
        let Some(profile_hash) = profile_hash else {
            return;
        };
        let mut state = self.inner.lock().expect("cache affinity lock poisoned");
        if !state.deployments.contains_key(profile_hash) {
            state.order.push_back(profile_hash.to_owned());
        }
        state
            .deployments
            .insert(profile_hash.to_owned(), deployment.to_owned());
        while state.deployments.len() > self.capacity {
            if let Some(oldest) = state.order.pop_front() {
                state.deployments.remove(&oldest);
            }
        }
    }
}

#[derive(Clone)]
struct ArtifactRuntime {
    controller: ArtifactController,
    catalog_revision: String,
    route_revision: String,
}

#[derive(Clone)]
struct ControlSource {
    catalog: PathBuf,
    route: PathBuf,
    manifest: PathBuf,
    signing_key: Option<Vec<u8>>,
}

struct LoadedControl {
    catalog: Arc<CatalogSnapshot>,
    route: Arc<RouteConfig>,
    plane: ControlPlane,
    source: Option<ControlSource>,
    signing_key: Option<Vec<u8>>,
}

impl AppState {
    fn with_active_control(mut self) -> Self {
        let snapshot = self.control.snapshot();
        self.catalog = snapshot.catalog;
        self.route = snapshot.route;
        self
    }
}

#[derive(Clone)]
struct RequestGovernance {
    tenant_key: String,
    policy: DataPolicyContract,
    compatibility_mode: bool,
    tenant_generation: u64,
    task_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DecisionRecord {
    #[serde(default)]
    context: Option<DecisionRecordContext>,
    decision_id: String,
    trace_turn: Option<String>,
    #[serde(default)]
    trace_id: Option<String>,
    #[serde(default)]
    parent_turn: Option<String>,
    #[serde(default)]
    task_key: Option<String>,
    #[serde(default)]
    session_key: Option<String>,
    #[serde(default)]
    agent_harness: Option<String>,
    #[serde(default)]
    call_role: Option<CallRole>,
    #[serde(default)]
    migration_boundary: Option<MigrationBoundary>,
    #[serde(default)]
    compatibility_mode: bool,
    #[serde(default)]
    tenant_key: String,
    #[serde(default)]
    recording: RecordingMode,
    #[serde(default)]
    training_eligible: bool,
    #[serde(default)]
    remote_judge_eligible: bool,
    #[serde(default)]
    expires_at_unix_s: u64,
    #[serde(default)]
    messages_hash: String,
    #[serde(default)]
    created_at_unix_s: u64,
    #[serde(default)]
    redaction_profile: String,
    #[serde(default)]
    semantic_task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vector_ref: Option<VectorRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exploration: Option<ExplorationRecord>,
    route_id: String,
    tier: String,
    reason: String,
    alternatives: Vec<ModelId>,
    evidence: CatalogEvidence,
    requirement: urouter_ai::capabilities::CapabilityRequirement,
    admission: urouter_ai::admission::AdmissionResult,
    execution: ExecutionRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact: Option<ArtifactDecision>,
    #[serde(default)]
    override_record: Option<OverrideRecord>,
    #[serde(default)]
    outcome_signals: Vec<FeedbackSignal>,
    #[serde(skip)]
    tenant_generation: u64,
    #[serde(skip)]
    task_generation: u64,
}

impl DecisionRecord {
    fn normalize_after_load(mut self) -> Self {
        if !self
            .context
            .as_ref()
            .is_some_and(DecisionRecordContext::training_complete)
        {
            self.training_eligible = false;
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OverrideRecord {
    parent_decision_id: Option<String>,
    parent_tier: Option<String>,
    chosen_tier: String,
    kind: Option<String>,
    parent_completed: bool,
    paired: bool,
    rejected_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeedbackSignal {
    kind: String,
    strength: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplorationRecord {
    epsilon_millionths: u32,
    propensity_millionths: u32,
    eligible_set: Vec<String>,
    selected_by_exploration: bool,
    authorized_budget_nano_usd: u64,
}

#[derive(Debug, Deserialize)]
struct FeedbackRequest {
    contract_version: Option<u16>,
    turn: String,
    signals: Vec<FeedbackInput>,
    data_policy: Option<DataPolicyContract>,
}

#[derive(Debug, Deserialize)]
struct FeedbackInput {
    kind: String,
    strength: Option<f64>,
}

#[derive(Clone, Default)]
struct FeedbackStore {
    values: Arc<RwLock<BTreeMap<String, BTreeMap<String, FeedbackSignal>>>>,
    writer: Option<mpsc::Sender<FeedbackCommand>>,
    dropped: Arc<AtomicU64>,
    write_errors: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeedbackEvent {
    #[serde(default)]
    tenant_key: String,
    turn: String,
    signals: Vec<FeedbackSignal>,
}

enum FeedbackCommand {
    Append(FeedbackEvent),
    Rewrite {
        events: Vec<FeedbackEvent>,
        completed: oneshot::Sender<bool>,
    },
}

#[derive(Clone)]
struct GatewayMetrics {
    requests: Arc<AtomicU64>,
    upstream_attempts: Arc<AtomicU64>,
    usage_unavailable: Arc<AtomicU64>,
    cache_affinity_hits: Arc<AtomicU64>,
    vector_writes: Arc<AtomicU64>,
    vector_write_errors: Arc<AtomicU64>,
    successes: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
    retries: Arc<AtomicU64>,
    feedback_signals: Arc<AtomicU64>,
    paired: Arc<AtomicU64>,
    paired_rejected: Arc<AtomicU64>,
    streams_completed: Arc<AtomicU64>,
    stream_failures: Arc<AtomicU64>,
    fallbacks: Arc<AtomicU64>,
    bindings_created: Arc<AtomicU64>,
    bindings_applied: Arc<AtomicU64>,
    binding_migrations: Arc<AtomicU64>,
    compatibility_requests: Arc<AtomicU64>,
    binding_conflicts: Arc<AtomicU64>,
    quota_rejections: Arc<AtomicU64>,
    budget_rejections: Arc<AtomicU64>,
    filter_rejections: Arc<StdMutex<BTreeMap<String, u64>>>,
    request_duration_ms: Histogram,
    upstream_duration_ms: Histogram,
    ttft_ms: Histogram,
    cost_nano_usd: Histogram,
    fallback_depth: Histogram,
}

impl Default for GatewayMetrics {
    fn default() -> Self {
        Self {
            requests: Arc::default(),
            upstream_attempts: Arc::default(),
            usage_unavailable: Arc::default(),
            cache_affinity_hits: Arc::default(),
            vector_writes: Arc::default(),
            vector_write_errors: Arc::default(),
            successes: Arc::default(),
            errors: Arc::default(),
            retries: Arc::default(),
            feedback_signals: Arc::default(),
            paired: Arc::default(),
            paired_rejected: Arc::default(),
            streams_completed: Arc::default(),
            stream_failures: Arc::default(),
            fallbacks: Arc::default(),
            bindings_created: Arc::default(),
            bindings_applied: Arc::default(),
            binding_migrations: Arc::default(),
            compatibility_requests: Arc::default(),
            binding_conflicts: Arc::default(),
            quota_rejections: Arc::default(),
            budget_rejections: Arc::default(),
            filter_rejections: Arc::default(),
            request_duration_ms: Histogram::new(&[
                5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000,
            ]),
            upstream_duration_ms: Histogram::new(&[
                5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000,
            ]),
            ttft_ms: Histogram::new(&[
                10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000,
            ]),
            cost_nano_usd: Histogram::new(&[
                1_000,
                10_000,
                100_000,
                1_000_000,
                10_000_000,
                100_000_000,
                1_000_000_000,
            ]),
            fallback_depth: Histogram::new(&[0, 1, 2, 3, 5, 8]),
        }
    }
}

#[derive(Clone)]
struct Histogram {
    bounds: &'static [u64],
    state: Arc<StdMutex<HistogramState>>,
}

#[derive(Default)]
struct HistogramState {
    buckets: Vec<u64>,
    count: u64,
    sum: u128,
    exemplar: Option<(String, u64)>,
}

impl Histogram {
    fn new(bounds: &'static [u64]) -> Self {
        Self {
            bounds,
            state: Arc::new(StdMutex::new(HistogramState {
                buckets: vec![0; bounds.len()],
                ..HistogramState::default()
            })),
        }
    }

    fn observe(&self, value: u64) {
        self.observe_with_exemplar(value, None);
    }

    fn observe_with_exemplar(&self, value: u64, trace_id: Option<&str>) {
        let mut state = self.state.lock().expect("histogram metric lock poisoned");
        for (index, bound) in self.bounds.iter().enumerate() {
            if value <= *bound {
                state.buckets[index] = state.buckets[index].saturating_add(1);
            }
        }
        state.count = state.count.saturating_add(1);
        state.sum = state.sum.saturating_add(u128::from(value));
        if let Some(trace_id) = trace_id {
            state.exemplar = Some((trace_id.to_owned(), value));
        }
    }

    fn render(&self, body: &mut String, name: &str, help: &str) {
        let state = self.state.lock().expect("histogram metric lock poisoned");
        let _ = writeln!(body, "# HELP {name} {help}");
        let _ = writeln!(body, "# TYPE {name} histogram");
        for (bound, count) in self.bounds.iter().zip(&state.buckets) {
            let _ = writeln!(body, "{name}_bucket{{le=\"{bound}\"}} {count}");
        }
        let _ = write!(body, "{name}_bucket{{le=\"+Inf\"}} {}", state.count);
        if let Some((trace_id, value)) = &state.exemplar {
            let _ = write!(body, " # {{trace_id=\"{trace_id}\"}} {value}");
        }
        body.push('\n');
        let _ = writeln!(body, "{name}_sum {}", state.sum);
        let _ = writeln!(body, "{name}_count {}", state.count);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExecutionRecord {
    #[serde(default = "bool_true")]
    ok: bool,
    stream: bool,
    upstream_status: u16,
    upstream_latency_ms: u128,
    usage: Option<Usage>,
    cost: Option<CostBreakdown>,
    usage_unavailable: bool,
    #[serde(default)]
    attempts: Vec<AttemptRecord>,
    #[serde(default)]
    error_kind: Option<UpstreamErrorKind>,
    #[serde(default)]
    deployment: String,
    #[serde(default)]
    fallback_depth: u8,
    #[serde(default)]
    runtime_filter_trace: Vec<DeploymentEvaluation>,
}

impl ExecutionRecord {
    fn success(
        stream: bool,
        upstream_latency_ms: u128,
        usage: Option<Usage>,
        cost: Option<CostBreakdown>,
        attempts: Vec<AttemptRecord>,
        deployment: String,
        fallback_depth: u8,
    ) -> Self {
        let runtime_filter_trace = attempts
            .iter()
            .flat_map(|attempt| attempt.selection_trace.iter().cloned())
            .collect();
        Self {
            ok: true,
            stream,
            upstream_status: 200,
            upstream_latency_ms,
            usage,
            cost,
            usage_unavailable: usage.is_none(),
            attempts,
            error_kind: None,
            deployment,
            fallback_depth,
            runtime_filter_trace,
        }
    }
}

const fn bool_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttemptRecord {
    #[serde(default)]
    attempt_id: Option<String>,
    attempt: u8,
    #[serde(default)]
    tier: String,
    #[serde(default)]
    deployment: String,
    #[serde(default)]
    model: Option<ModelId>,
    status: Option<u16>,
    latency_ms: u128,
    error_kind: Option<UpstreamErrorKind>,
    retry: bool,
    #[serde(default)]
    selection_trace: Vec<DeploymentEvaluation>,
    #[serde(default)]
    capacity_snapshot: CapacitySnapshot,
}

#[derive(Clone, Copy)]
struct AttemptOutcome {
    status: Option<u16>,
    latency_ms: u128,
    error_kind: Option<UpstreamErrorKind>,
    retry: bool,
}

impl AttemptOutcome {
    fn success(status: u16, latency_ms: u128) -> Self {
        Self {
            status: Some(status),
            latency_ms,
            error_kind: None,
            retry: false,
        }
    }

    fn failure(failure: &AttemptFailure, retry: bool) -> Self {
        Self {
            status: failure.status.map(|status| status.as_u16()),
            latency_ms: failure.latency_ms,
            error_kind: Some(failure.kind),
            retry,
        }
    }
}

impl AttemptRecord {
    #[allow(clippy::too_many_arguments)]
    fn new(
        request_id: &str,
        attempt: u8,
        tier: &str,
        deployment: &RouteDeployment,
        model: &ModelSpec,
        selection_trace: Vec<DeploymentEvaluation>,
        capacity_snapshot: CapacitySnapshot,
        outcome: AttemptOutcome,
    ) -> Self {
        Self {
            attempt_id: Some(attempt_id(request_id, attempt)),
            attempt,
            tier: tier.to_owned(),
            deployment: deployment.id.clone(),
            model: Some(model.id.clone()),
            status: outcome.status,
            latency_ms: outcome.latency_ms,
            error_kind: outcome.error_kind,
            retry: outcome.retry,
            selection_trace,
            capacity_snapshot,
        }
    }
}

#[derive(Clone)]
struct RecordSeed {
    request_id: String,
    messages_hash: String,
    features: FeatureFrame,
    route_revision: String,
    artifact: Option<ArtifactDecision>,
    exploration: Option<ExplorationRecord>,
    vector_ref: Option<VectorRef>,
}

struct ResponseContext {
    decision_id: String,
    decision: RouteDecision,
    governance: RequestGovernance,
    record: RecordSeed,
    headers: HeaderMap,
    quota: QuotaLease,
    quota_input_tokens: u64,
    budget: BudgetAccounting,
}

struct BudgetAccounting {
    lease: BudgetLease,
    input_tokens: u64,
    output_tokens: u64,
}

impl BudgetAccounting {
    async fn settle(
        &self,
        state: &AppState,
        cost: Option<&CostBreakdown>,
        attempts: &[AttemptRecord],
    ) {
        settle_budget_usage(
            state,
            &self.lease,
            cost,
            attempts,
            self.input_tokens,
            self.output_tokens,
        )
        .await;
    }
}

struct RequestAdmission {
    governance: RequestGovernance,
    quota: QuotaLease,
    quota_input_tokens: u64,
    budget: BudgetLease,
    budget_input_tokens: u64,
    budget_output_tokens: u64,
}

struct FailedRequestContext {
    decision: RouteDecision,
    governance: RequestGovernance,
    record: RecordSeed,
    budget: BudgetLease,
    stream: bool,
    started: Instant,
}

struct UpstreamExecution {
    response: reqwest::Response,
    elapsed_ms: u128,
    attempts: Vec<AttemptRecord>,
    lease: ExecutionLease,
    tier: String,
    model: ModelSpec,
    fallback_depth: u8,
}

#[derive(Debug)]
struct RoutedFailure {
    error: GatewayError,
    attempts: Vec<AttemptRecord>,
    tier: String,
    model: ModelSpec,
    fallback_depth: u8,
    runtime_filter_trace: Vec<DeploymentEvaluation>,
}

#[derive(Clone)]
struct ExecutionTier {
    tier: String,
    deployments: Vec<RouteDeployment>,
}

struct RequestExecutionIdentity<'a> {
    request_id: &'a str,
    tenant_key: &'a str,
}

struct TierSuccess {
    response: reqwest::Response,
    lease: ExecutionLease,
    model: ModelSpec,
}

struct PreparedDeployment {
    model: ModelSpec,
    url: String,
    headers: BTreeMap<String, String>,
    request: Value,
}

struct ExecutionLease {
    local: Option<CapacityLease>,
    deployment: RouteDeployment,
    shared: Arc<dyn SharedCircuitRepository>,
    permit: Option<CircuitPermit>,
    tier_size: usize,
    selection_trace: Vec<DeploymentEvaluation>,
    capacity_snapshot: CapacitySnapshot,
}

impl ExecutionLease {
    async fn complete(mut self, result: Result<(), UpstreamErrorKind>) -> Result<(), GatewayError> {
        if let Some(local) = self.local.take() {
            local.complete(result);
        }
        if let Some(permit) = self.permit.take() {
            self.shared
                .finish(permit, result, self.tier_size)
                .await
                .map_err(state_backend_unavailable)?;
        }
        Ok(())
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = shared.abandon(permit).await;
            });
        }
    }
}

struct TierExhausted {
    error: Option<GatewayError>,
    model: ModelSpec,
}

struct AttemptFailure {
    kind: UpstreamErrorKind,
    status: Option<reqwest::StatusCode>,
    detail: String,
    retry_after_ms: Option<u64>,
    latency_ms: u128,
}

#[derive(Clone)]
struct RecordStore {
    records: Arc<RwLock<VecDeque<DecisionRecord>>>,
    capacity: usize,
    writer: Option<mpsc::Sender<RecordCommand>>,
    dropped: Arc<AtomicU64>,
    write_errors: Arc<AtomicU64>,
}

enum RecordCommand {
    Append(Box<DecisionRecord>),
    Rewrite {
        records: Vec<DecisionRecord>,
        completed: oneshot::Sender<bool>,
    },
}

#[derive(Debug, Default, Deserialize)]
struct DecisionQuery {
    after: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Default)]
struct StreamCapture {
    complete: Vec<u8>,
    tail: Vec<u8>,
    failed: bool,
    first_chunk_observed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DecisionDisclosure {
    decision_id: String,
    turn: Option<String>,
    tier: String,
    model: ModelId,
    provider: urouter_types::ProviderId,
    source: &'static str,
    reason: String,
    degraded: bool,
    alternatives: Vec<ModelId>,
    cost: Option<CostBreakdown>,
    compatibility_mode: bool,
}

#[derive(Debug)]
struct GatewayError {
    status: StatusCode,
    code: &'static str,
    message: String,
    request_id: Option<String>,
    decision_id: Option<String>,
}

impl GatewayError {
    fn bad_request(code: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: error.to_string(),
            request_id: None,
            decision_id: None,
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: error.to_string(),
            request_id: None,
            decision_id: None,
        }
    }

    fn with_request_id(mut self, request_id: &str) -> Self {
        self.request_id = Some(request_id.to_owned());
        self
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                    "type": "urouter_error"
                }
            })),
        )
            .into_response();
        if let Some(request_id) = self.request_id
            && let Ok(value) = HeaderValue::from_str(&request_id)
        {
            response
                .headers_mut()
                .insert(HeaderName::from_static("x-urouter-request-id"), value);
        }
        if let Some(decision_id) = self.decision_id
            && let Ok(value) = HeaderValue::from_str(&decision_id)
        {
            response
                .headers_mut()
                .insert(HeaderName::from_static("x-urouter-decision-id"), value);
        }
        response
    }
}

impl From<RouteError> for GatewayError {
    fn from(error: RouteError) -> Self {
        match error {
            RouteError::RequiredToolUnavailable(tool) => Self::bad_request(
                "missing_required_tool",
                format!("the Agent Host must provide the required {tool} tool"),
            ),
            error => Self::bad_request("route_rejected", error),
        }
    }
}

impl From<ManagementAuthError> for GatewayError {
    fn from(error: ManagementAuthError) -> Self {
        let (status, code, message) = match error {
            ManagementAuthError::MissingCredential | ManagementAuthError::InvalidCredential => (
                StatusCode::UNAUTHORIZED,
                "management_unauthorized",
                "a valid management Bearer credential is required",
            ),
            ManagementAuthError::TenantForbidden | ManagementAuthError::RoleForbidden => (
                StatusCode::FORBIDDEN,
                "management_forbidden",
                "the management credential cannot perform this action",
            ),
            ManagementAuthError::AuditUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "management_audit_unavailable",
                "the management audit event could not be persisted",
            ),
            ManagementAuthError::InvalidKeyring(_) | ManagementAuthError::KeyringIo(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "management_auth_unavailable",
                "management authorization is unavailable",
            ),
        };
        Self {
            status,
            code,
            message: message.to_owned(),
            request_id: None,
            decision_id: None,
        }
    }
}

fn state_backend_unavailable(_error: impl std::fmt::Display) -> GatewayError {
    GatewayError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "state_backend_unavailable",
        message: "the shared state backend is temporarily unavailable".to_owned(),
        request_id: None,
        decision_id: None,
    }
}

fn record_repository_error(error: RecordRepositoryError) -> GatewayError {
    match error {
        RecordRepositoryError::Local(message) => GatewayError::internal(message),
        RecordRepositoryError::Shared(error) => state_backend_unavailable(error),
    }
}

fn quota_repository_error(error: QuotaError) -> GatewayError {
    match error {
        QuotaError::Backend(error) => state_backend_unavailable(error),
        QuotaError::LockPoisoned | QuotaError::InvalidRejection(_) => {
            GatewayError::internal("quota state is invalid")
        }
    }
}

fn budget_repository_error(error: BudgetError) -> GatewayError {
    match error {
        BudgetError::Backend(error) => state_backend_unavailable(error),
        BudgetError::LockPoisoned => GatewayError::internal("budget state is invalid"),
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<(), BoxError> {
    let args = Args::parse();
    if args.dry_run {
        let report = configuration_dry_run(&args);
        println!("{}", serde_json::to_string_pretty(&report)?);
        if !report.valid {
            std::process::exit(2);
        }
        return Ok(());
    }
    validate_args(&args)?;
    let deployment_picker = configured_picker(&args)?;
    let loaded_control = load_control(&args)?;
    let catalog = Arc::clone(&loaded_control.catalog);
    let route = Arc::clone(&loaded_control.route);
    let artifact = load_artifact_runtime(&args, &catalog, &route)?;
    let bindings = build_binding_repository(&args).await?;
    let cooldown_policy = configured_cooldown_policy(&args);
    let shared_circuits = build_circuit_repository(&args, cooldown_policy).await?;
    let shared_state = build_shared_state(&args).await?;
    let idempotency = build_idempotency_repository(&args, shared_state.as_ref());
    let quota = build_quota_repository(&args).await?;
    let budget = build_budget_repository(&args).await?;
    let management_auth = ManagementAuth::open(
        args.management_keyring.clone(),
        args.management_audit.clone(),
        args.management_audit_queue_capacity,
        Duration::from_secs(args.management_keyring_reload_seconds),
    )
    .await?;
    let feedback_path = args.feedback_records.clone().or_else(|| {
        args.records
            .as_ref()
            .map(|path| PathBuf::from(format!("{}.feedback", path.display())))
    });
    let records = RecordStore::open(
        args.records.clone(),
        args.record_capacity,
        args.record_queue_capacity,
        args.record_max_bytes,
    )
    .await?;
    let vector_store = match &args.vector_store {
        Some(path) => Some(VectorSideStore::open(path, args.vector_shard_max_bytes).await?),
        None => None,
    };
    let record_repository =
        build_record_repository(shared_state.as_ref(), records.clone(), args.record_capacity);
    let feedback_store = FeedbackStore::open(
        feedback_path,
        args.record_queue_capacity,
        args.record_max_bytes,
    )
    .await?;
    reconcile_feedback(&records, &feedback_store).await;
    let capacity = CapacityManager::with_picker(cooldown_policy, deployment_picker);
    for tier in &route.tiers {
        capacity.register(&tier.effective_deployments());
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_millis(args.connect_timeout_ms))
        .build()?;
    let state = AppState {
        catalog,
        route,
        client: client.clone(),
        credentials: CredentialManager::new(client),
        records,
        record_repository,
        feedback: feedback_store,
        metrics: GatewayMetrics::default(),
        request_timeout: Duration::from_millis(args.request_timeout_ms),
        retry_policy: RetryPolicy {
            max_retries: args.max_retries,
            base_backoff_ms: args.retry_base_backoff_ms,
            max_backoff_ms: args.retry_max_backoff_ms,
        },
        capacity,
        shared_circuits,
        max_fallback_depth: args.max_fallback_depth,
        bindings,
        idempotency,
        idempotency_ttl_seconds: args.idempotency_ttl_seconds,
        quota,
        quota_default_max_output_tokens: args.quota_default_max_output_tokens,
        budget,
        shared_state,
        require_tenant_header: args.require_tenant_header,
        management_auth,
        accepting: Arc::new(AtomicBool::new(true)),
        control: loaded_control.plane,
        control_source: loaded_control.source,
        artifact,
        exploration: ExplorationPolicy {
            epsilon_millionths: args.exploration_epsilon_millionths,
            maximum_budget_nano_usd: args.exploration_max_budget_nano_usd,
        },
        cache_affinity: CacheAffinityStore::new(args.task_binding_capacity),
        vector_store,
    };
    spawn_retention_sweeper(state.records.clone(), state.vector_store.clone());
    spawn_control_reloader(&args, &state, loaded_control.signing_key);
    let app = app_router(state.clone());
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    println!("urouter-gateway listening on http://{}", args.bind);
    serve_with_drain(
        listener,
        app,
        Arc::clone(&state.accepting),
        args.shutdown_grace_seconds,
    )
    .await?;
    Ok(())
}

fn load_control(args: &Args) -> Result<LoadedControl, BoxError> {
    let signing_key = control_signing_key(args)?;
    let initial = if let Some(manifest) = &args.control_manifest {
        ControlSnapshot::load(&args.catalog, &args.route, manifest, signing_key.as_deref())?
    } else {
        let catalog = Arc::new(CatalogSnapshot::from_json_str(&fs::read_to_string(
            &args.catalog,
        )?)?);
        let route: RouteConfig = serde_json::from_str(&fs::read_to_string(&args.route)?)?;
        route.validate(&catalog)?;
        ControlSnapshot::from_validated(catalog, Arc::new(route))
    };
    let source = args
        .control_manifest
        .as_ref()
        .map(|manifest| ControlSource {
            catalog: args.catalog.clone(),
            route: args.route.clone(),
            manifest: manifest.clone(),
            signing_key: signing_key.clone(),
        });
    Ok(LoadedControl {
        catalog: Arc::clone(&initial.catalog),
        route: Arc::clone(&initial.route),
        plane: ControlPlane::new(
            initial,
            configured_control_failure_policy(args)?,
            args.control_required_revision.clone(),
        ),
        source,
        signing_key,
    })
}

fn load_artifact_runtime(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> Result<Option<ArtifactRuntime>, BoxError> {
    if args.artifact_active.is_none() && args.artifact_candidate.is_none() {
        return Ok(None);
    }
    let key_env = args
        .artifact_signing_key_env
        .as_deref()
        .ok_or("artifact files require --artifact-signing-key-env")?;
    let signing_key =
        env::var(key_env).map_err(|_| "artifact signing key environment variable is not set")?;
    if signing_key.is_empty() {
        return Err("artifact signing key must not be empty".into());
    }
    let catalog_revision = catalog.hashes().content.to_string();
    let route_revision = route.revision();
    let load = |path: &FsPath| -> Result<RouterArtifact, BoxError> {
        let artifact: RouterArtifact = serde_json::from_str(&fs::read_to_string(path)?)?;
        artifact.verify(
            FEATURE_SCHEMA_VERSION,
            &catalog_revision,
            &route_revision,
            Some(signing_key.as_bytes()),
        )?;
        for tier in [&artifact.model.baseline_tier, &artifact.model.promoted_tier] {
            if !route.tiers.iter().any(|candidate| &candidate.tier == tier) {
                return Err(format!("artifact references unknown tier: {tier}").into());
            }
        }
        Ok(artifact)
    };
    let active = args.artifact_active.as_deref().map(load).transpose()?;
    let controller = ArtifactController::new(
        active,
        RolloutPolicy {
            shadow: args.artifact_shadow,
            canary_basis_points: args.artifact_canary_basis_points,
            minimum_samples: args.artifact_minimum_samples,
            operation_limit: args.artifact_operation_limit,
        },
    );
    if let Some(candidate) = args.artifact_candidate.as_deref().map(load).transpose()? {
        controller.load_candidate(candidate);
    }
    if args.artifact_kill_switch {
        controller.set_kill_switch(true, "startup configuration");
    }
    Ok(Some(ArtifactRuntime {
        controller,
        catalog_revision,
        route_revision,
    }))
}

async fn serve_with_drain(
    listener: tokio::net::TcpListener,
    app: Router,
    accepting: Arc<AtomicBool>,
    grace_seconds: u64,
) -> Result<(), std::io::Error> {
    serve_with_shutdown(listener, app, accepting, grace_seconds, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

async fn serve_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    app: Router,
    accepting: Arc<AtomicBool>,
    grace_seconds: u64,
    shutdown: F,
) -> Result<(), std::io::Error>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (drain_tx, _) = broadcast::channel::<()>(1);
    let mut graceful_rx = drain_tx.subscribe();
    let mut deadline_rx = drain_tx.subscribe();
    tokio::spawn(async move {
        shutdown.await;
        accepting.store(false, Ordering::SeqCst);
        let _ = drain_tx.send(());
    });

    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = graceful_rx.recv().await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        () = async move {
            let _ = deadline_rx.recv().await;
            sleep(Duration::from_secs(grace_seconds)).await;
        } => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DryRunStatus {
    Pass,
    Fail,
    Warning,
    NotApplicable,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ValidationCostClass {
    Free,
    Local,
    Remote,
    PostHoc,
}

const CASCADE_COST_CLASSES: [ValidationCostClass; 5] = [
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Local,
    ValidationCostClass::Remote,
    ValidationCostClass::PostHoc,
];

const FILTER_COST_CLASSES: [ValidationCostClass; 10] = [
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Local,
    ValidationCostClass::Local,
    ValidationCostClass::Local,
    ValidationCostClass::Local,
];

#[derive(Debug, Serialize)]
struct DryRunCheck {
    number: u8,
    id: &'static str,
    status: DryRunStatus,
    message: String,
}

#[derive(Debug, Serialize)]
struct DryRunError {
    code: &'static str,
    path: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct DryRunReport {
    schema_version: u16,
    valid: bool,
    catalog: Option<Value>,
    route: Option<Value>,
    checks: Vec<DryRunCheck>,
    errors: Vec<DryRunError>,
}

const DRY_RUN_CHECK_IDS: [&str; 16] = [
    "cascade_cost_class_order",
    "filter_cost_class_order",
    "artifact_feature_schema",
    "artifact_tier_subset",
    "target_catalog_and_cost",
    "referenced_provider_auth",
    "exploration_requires_recording",
    "tier_targets_and_fallbacks",
    "budget_soft_below_hard",
    "multi_instance_state_policy",
    "tool_route_model_decider",
    "tier_capability_equivalence",
    "catalog_manifest",
    "custom_provider_compat",
    "endpoint_placeholders",
    "cost_override_reason",
];

fn check(
    number: u8,
    id: &'static str,
    status: DryRunStatus,
    message: impl Into<String>,
) -> DryRunCheck {
    DryRunCheck {
        number,
        id,
        status,
        message: message.into(),
    }
}

fn blocked_checks(message: &str) -> Vec<DryRunCheck> {
    (1_u8..)
        .zip(DRY_RUN_CHECK_IDS.iter())
        .map(|(number, id)| check(number, id, DryRunStatus::Blocked, message))
        .collect()
}

fn configuration_dry_run(args: &Args) -> DryRunReport {
    if let Err(error) = validate_args(args) {
        return failed_dry_run(
            "invalid_arguments",
            "arguments",
            error.to_string(),
            "configuration arguments are invalid",
        );
    }
    let catalog_source = match fs::read(&args.catalog) {
        Ok(source) => source,
        Err(error) => {
            return failed_dry_run(
                "catalog_read_failed",
                args.catalog.display().to_string(),
                error.to_string(),
                "catalog could not be loaded",
            );
        }
    };
    let catalog_text = match std::str::from_utf8(&catalog_source) {
        Ok(text) => text,
        Err(error) => {
            return failed_dry_run(
                "catalog_encoding_invalid",
                args.catalog.display().to_string(),
                error.to_string(),
                "catalog is not UTF-8",
            );
        }
    };
    let catalog = match CatalogSnapshot::from_json_str(catalog_text) {
        Ok(catalog) => catalog,
        Err(error) => {
            return failed_dry_run(
                "catalog_invalid",
                args.catalog.display().to_string(),
                error.to_string(),
                "catalog parsing or validation failed",
            );
        }
    };
    let route_source = match fs::read_to_string(&args.route) {
        Ok(source) => source,
        Err(error) => {
            return failed_dry_run(
                "route_read_failed",
                args.route.display().to_string(),
                error.to_string(),
                "route configuration could not be loaded",
            );
        }
    };
    let route: RouteConfig = match serde_json::from_str(&route_source) {
        Ok(route) => route,
        Err(error) => {
            return failed_dry_run(
                "route_parse_failed",
                args.route.display().to_string(),
                error.to_string(),
                "route configuration is invalid JSON",
            );
        }
    };
    dry_run_report(args, &catalog_source, &catalog, &route)
}

fn failed_dry_run(
    code: &'static str,
    path: impl Into<String>,
    message: String,
    blocked_message: &str,
) -> DryRunReport {
    DryRunReport {
        schema_version: 1,
        valid: false,
        catalog: None,
        route: None,
        checks: blocked_checks(blocked_message),
        errors: vec![DryRunError {
            code,
            path: path.into(),
            message,
        }],
    }
}

fn dry_run_report(
    args: &Args,
    catalog_source: &[u8],
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> DryRunReport {
    let deployment_count = route
        .tiers
        .iter()
        .map(|tier| tier.effective_deployments().len())
        .sum::<usize>();
    let route_validation = route.validate(catalog);
    let route_status = if route_validation.is_ok() {
        DryRunStatus::Pass
    } else {
        DryRunStatus::Fail
    };
    let route_message = route_validation
        .as_ref()
        .map_or_else(std::string::ToString::to_string, |()| {
            "route validation passed".to_owned()
        });
    let (auth_status, auth_message) = referenced_auth_check(catalog, route);
    let (manifest_status, manifest_message) = catalog_manifest_check(args, catalog_source, catalog);
    let mut checks = schema_dry_run_checks(args, catalog, route);
    checks.extend(route_dry_run_checks(
        args,
        catalog,
        route,
        route_status,
        &route_message,
        auth_status,
        auth_message,
    ));
    checks.extend(catalog_dry_run_checks(
        catalog_source,
        manifest_status,
        manifest_message,
    ));
    let valid = checks.iter().all(|item| item.status != DryRunStatus::Fail);
    DryRunReport {
        schema_version: 1,
        valid,
        catalog: Some(json!({
            "schema_version": catalog.schema_version(),
            "content_revision": catalog.hashes().content,
            "models": catalog.models().count()
        })),
        route: Some(json!({
            "id": route.id,
            "revision": route.revision(),
            "tiers": route.tiers.len(),
            "deployments": deployment_count
        })),
        checks,
        errors: Vec::new(),
    }
}

fn referenced_auth_check(catalog: &CatalogSnapshot, route: &RouteConfig) -> (DryRunStatus, String) {
    let missing_auth = route
        .tiers
        .iter()
        .flat_map(TierConfig::effective_deployments)
        .filter_map(|deployment| catalog.model(&deployment.model))
        .filter_map(|model| catalog.provider(&model.provider))
        .filter_map(|provider| match &provider.auth {
            urouter_ai::auth::AuthSpec::ApiKeyEnv { env, .. }
                if std::env::var_os(env).is_none() =>
            {
                Some(env.clone())
            }
            urouter_ai::auth::AuthSpec::OAuthClientCredentials {
                client_id_env,
                client_secret_env,
                ..
            } if std::env::var_os(client_id_env).is_none()
                || std::env::var_os(client_secret_env).is_none() =>
            {
                Some(format!("{client_id_env},{client_secret_env}"))
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    if missing_auth.is_empty() {
        (
            DryRunStatus::Pass,
            "all referenced provider credentials are resolvable or explicitly disabled".to_owned(),
        )
    } else {
        (
            DryRunStatus::Fail,
            format!(
                "missing credential environment variables: {}",
                missing_auth.into_iter().collect::<Vec<_>>().join(", ")
            ),
        )
    }
}

fn catalog_manifest_check(
    args: &Args,
    catalog_source: &[u8],
    catalog: &CatalogSnapshot,
) -> (DryRunStatus, String) {
    let manifest_path = args.catalog.with_file_name("manifest.json");
    match fs::read_to_string(&manifest_path)
        .map_err(|error| error.to_string())
        .and_then(|source| {
            serde_json::from_str::<CatalogManifest>(&source).map_err(|error| error.to_string())
        }) {
        Ok(manifest) if manifest.matches(catalog_source, catalog) => (
            DryRunStatus::Pass,
            format!("manifest matches {}", args.catalog.display()),
        ),
        Ok(_) => (
            DryRunStatus::Fail,
            "manifest hashes do not match the catalog".to_owned(),
        ),
        Err(error) => (
            DryRunStatus::Fail,
            format!(
                "manifest {} is unavailable or invalid: {error}",
                manifest_path.display()
            ),
        ),
    }
}

fn schema_dry_run_checks(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> Vec<DryRunCheck> {
    let artifact_configured = args.artifact_active.is_some() || args.artifact_candidate.is_some();
    let (artifact_status, artifact_message) = if artifact_configured {
        match load_artifact_runtime(args, catalog, route) {
            Ok(Some(_)) => (
                DryRunStatus::Pass,
                "artifact schema, signature, revisions, gates, and tiers passed".to_owned(),
            ),
            Ok(None) => (
                DryRunStatus::Fail,
                "artifact configuration did not produce a runtime".to_owned(),
            ),
            Err(error) => (DryRunStatus::Fail, error.to_string()),
        }
    } else {
        (
            DryRunStatus::NotApplicable,
            "no router artifact is configured".to_owned(),
        )
    };
    vec![
        check(
            1,
            DRY_RUN_CHECK_IDS[0],
            monotonic_cost_class_status(&CASCADE_COST_CLASSES),
            "compiled Rule/Signal/Model/Judge/Escalation cascade is cost-monotonic",
        ),
        check(
            2,
            DRY_RUN_CHECK_IDS[1],
            monotonic_cost_class_status(&FILTER_COST_CLASSES),
            "compiled tenant/auth/catalog/capacity/policy filter chain is cost-monotonic",
        ),
        check(
            3,
            DRY_RUN_CHECK_IDS[2],
            artifact_status,
            artifact_message.clone(),
        ),
        check(4, DRY_RUN_CHECK_IDS[3], artifact_status, artifact_message),
    ]
}

fn route_dry_run_checks(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
    route_status: DryRunStatus,
    route_message: &str,
    auth_status: DryRunStatus,
    auth_message: String,
) -> Vec<DryRunCheck> {
    vec![
        check(5, DRY_RUN_CHECK_IDS[4], route_status, route_message),
        check(6, DRY_RUN_CHECK_IDS[5], auth_status, auth_message),
        check(
            7,
            DRY_RUN_CHECK_IDS[6],
            if args.exploration_epsilon_millionths == 0
                || args.records.is_some()
                || args.redis_url.is_some()
            {
                DryRunStatus::Pass
            } else {
                DryRunStatus::Fail
            },
            if args.exploration_epsilon_millionths == 0 {
                "controlled exploration is disabled"
            } else if args.records.is_some() || args.redis_url.is_some() {
                "controlled exploration has a persistent DecisionRecord sink"
            } else {
                "controlled exploration requires --records so propensity evidence is persisted"
            },
        ),
        check(8, DRY_RUN_CHECK_IDS[7], route_status, route_message),
        check(
            9,
            DRY_RUN_CHECK_IDS[8],
            budget_boundary_status(args),
            budget_boundary_message(args),
        ),
        check(
            10,
            DRY_RUN_CHECK_IDS[9],
            if args.redis_url.is_none()
                || args.on_state_unavailable.as_deref() == Some("fail_closed")
            {
                DryRunStatus::Pass
            } else {
                DryRunStatus::Fail
            },
            if args.redis_url.is_none() {
                "single-instance state does not require a cross-instance failure policy"
            } else if args.on_state_unavailable.as_deref() == Some("fail_closed") {
                "multi-instance correctness state explicitly fails closed"
            } else {
                "Redis requires --on-state-unavailable fail_closed"
            },
        ),
        check(
            11,
            DRY_RUN_CHECK_IDS[10],
            tool_decider_status(args, catalog, route),
            tool_decider_message(args, catalog, route),
        ),
        check(12, DRY_RUN_CHECK_IDS[11], route_status, route_message),
    ]
}

fn catalog_dry_run_checks(
    catalog_source: &[u8],
    manifest_status: DryRunStatus,
    manifest_message: String,
) -> Vec<DryRunCheck> {
    let (override_status, override_message) = cost_override_reason_check(catalog_source);
    vec![
        check(13, DRY_RUN_CHECK_IDS[12], manifest_status, manifest_message),
        check(
            14,
            DRY_RUN_CHECK_IDS[13],
            DryRunStatus::Pass,
            "catalog validation requires explicit compatibility for custom providers",
        ),
        check(
            15,
            DRY_RUN_CHECK_IDS[14],
            DryRunStatus::Pass,
            "catalog validation resolved every endpoint placeholder from provider env",
        ),
        check(16, DRY_RUN_CHECK_IDS[15], override_status, override_message),
    ]
}

fn monotonic_cost_class_status(classes: &[ValidationCostClass]) -> DryRunStatus {
    if classes.windows(2).all(|pair| pair[0] <= pair[1]) {
        DryRunStatus::Pass
    } else {
        DryRunStatus::Fail
    }
}

fn budget_boundary_status(args: &Args) -> DryRunStatus {
    if (args.tenant_budget_nano_usd == 0 && args.tenant_budget_soft_nano_usd == 0)
        || (args.tenant_budget_soft_nano_usd > 0
            && args.tenant_budget_soft_nano_usd < args.tenant_budget_nano_usd)
    {
        DryRunStatus::Pass
    } else {
        DryRunStatus::Fail
    }
}

fn budget_boundary_message(args: &Args) -> &'static str {
    if args.tenant_budget_nano_usd == 0 && args.tenant_budget_soft_nano_usd == 0 {
        "tenant budget is disabled"
    } else if args.tenant_budget_soft_nano_usd > 0
        && args.tenant_budget_soft_nano_usd < args.tenant_budget_nano_usd
    {
        "tenant soft budget is positive and below the hard budget"
    } else {
        "enabled budget requires 0 < --tenant-budget-soft-nano-usd < --tenant-budget-nano-usd"
    }
}

fn route_supports_tools(catalog: &CatalogSnapshot, route: &RouteConfig) -> bool {
    route.tiers.iter().any(|tier| {
        catalog
            .model(&tier.model)
            .is_some_and(|model| model.capabilities.tool_calling)
    })
}

fn tool_decider_status(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> DryRunStatus {
    let expects_tools = route_supports_tools(catalog, route);
    if !expects_tools || args.artifact_active.is_some() || args.artifact_candidate.is_some() {
        DryRunStatus::Pass
    } else {
        DryRunStatus::Warning
    }
}

fn tool_decider_message(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> &'static str {
    let expects_tools = route_supports_tools(catalog, route);
    if !expects_tools {
        "route has no enabled targets"
    } else if args.artifact_active.is_some() || args.artifact_candidate.is_some() {
        "route has a configured model decider artifact"
    } else {
        "route has enabled targets but no model decider artifact; rule/signal routing remains active"
    }
}

fn cost_override_reason_check(catalog_source: &[u8]) -> (DryRunStatus, String) {
    let Ok(value) = serde_json::from_slice::<Value>(catalog_source) else {
        return (
            DryRunStatus::Fail,
            "catalog could not be decoded for cost override validation".to_owned(),
        );
    };
    let mut overrides = 0_usize;
    let mut missing_reason = 0_usize;
    visit_cost_overrides(&value, &mut overrides, &mut missing_reason);
    if missing_reason == 0 {
        (
            DryRunStatus::Pass,
            format!("validated {overrides} catalog cost override(s)"),
        )
    } else {
        (
            DryRunStatus::Fail,
            format!("{missing_reason} of {overrides} cost override(s) lack a non-empty reason"),
        )
    }
}

fn visit_cost_overrides(value: &Value, overrides: &mut usize, missing_reason: &mut usize) {
    match value {
        Value::Object(object) => {
            if let Some(cost_override) = object.get("cost_override") {
                *overrides += 1;
                if cost_override
                    .get("reason")
                    .and_then(Value::as_str)
                    .is_none_or(|reason| reason.trim().is_empty())
                {
                    *missing_reason += 1;
                }
            }
            for child in object.values() {
                visit_cost_overrides(child, overrides, missing_reason);
            }
        }
        Value::Array(array) => {
            for child in array {
                visit_cost_overrides(child, overrides, missing_reason);
            }
        }
        _ => {}
    }
}

fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/health/live", get(health))
        .route("/health/ready", get(readiness))
        .route("/openapi.json", get(openapi))
        .route("/v1/models", get(models))
        .route("/v1/catalog", get(catalog_status))
        .route("/v1/catalog/refresh", post(refresh_catalog))
        .route("/v1/catalog/rollback", post(rollback_catalog))
        .route("/v1/explain", post(explain))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(openai_responses))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/artifacts", get(artifact_status))
        .route("/v1/artifacts/promote", post(promote_artifact))
        .route("/v1/artifacts/rollback", post(rollback_artifact))
        .route("/v1/artifacts/kill", post(kill_artifact))
        .route("/v1/artifacts/rollout", post(update_artifact_rollout))
        .route("/v1/artifacts/observe", post(observe_artifact))
        .route(
            "/v1/adapters/{harness}/chat/completions",
            post(adapter_chat_completions),
        )
        .route("/v1/decisions", get(decisions))
        .route("/v1/stats", get(stats))
        .route(
            "/v1/decisions/{id}",
            get(decision_by_id).delete(delete_decision),
        )
        .route(
            "/v1/tasks/{id}/records",
            axum::routing::delete(delete_task_records),
        )
        .route(
            "/v1/tenant/records",
            axum::routing::delete(delete_tenant_records),
        )
        .route("/v1/feedback", post(feedback))
        .route("/v1/feedback/{turn}", get(feedback_by_turn))
        .route("/metrics", get(metrics))
        .route("/v1/tiers", get(tiers))
        .route(
            "/v1/tasks/{id}/binding",
            get(task_binding).delete(delete_task_binding),
        )
        .route(
            "/v1/sessions/{conversation}/{branch}/binding",
            get(session_binding).delete(delete_session_binding),
        )
        .with_state(state)
}

fn validate_args(args: &Args) -> Result<(), BoxError> {
    if args.record_capacity == 0
        || args.record_queue_capacity == 0
        || args.task_binding_capacity == 0
        || args.idempotency_capacity == 0
        || args.management_audit_queue_capacity == 0
    {
        return Err(
            "record, queue, task binding, idempotency, and audit capacities must be greater than zero"
                .into(),
        );
    }
    if args.vector_store.is_some() && args.vector_shard_max_bytes < 4 {
        return Err("vector shard size must be at least four bytes".into());
    }
    if args.cooldown_failure_threshold_millis > 1_000 {
        return Err("cooldown threshold must be <= 1000".into());
    }
    if args.task_binding_ttl_seconds == 0 {
        return Err("task binding TTL must be greater than zero".into());
    }
    if args.idempotency_ttl_seconds == 0 {
        return Err("idempotency TTL must be greater than zero".into());
    }
    if args.quota_lease_ttl_seconds == 0 {
        return Err("quota lease TTL must be greater than zero".into());
    }
    if args.quota_default_max_output_tokens == 0 {
        return Err("quota default max output tokens must be greater than zero".into());
    }
    if args.budget_period_seconds == 0 {
        return Err("budget period must be greater than zero".into());
    }
    if budget_boundary_status(args) == DryRunStatus::Fail {
        return Err(budget_boundary_message(args).into());
    }
    if args.management_keyring_reload_seconds == 0 {
        return Err("management keyring reload interval must be greater than zero".into());
    }
    if args.shutdown_grace_seconds == 0 {
        return Err("shutdown grace period must be greater than zero".into());
    }
    if args.control_manifest.is_some() && args.control_reload_seconds == 0 {
        return Err("control reload interval must be greater than zero".into());
    }
    if args.control_manifest.is_none() && args.control_signing_key_env.is_some() {
        return Err("control signing key requires --control-manifest".into());
    }
    if args.artifact_canary_basis_points > 10_000 {
        return Err("artifact canary basis points must be <= 10000".into());
    }
    if args.artifact_operation_limit < 6 {
        return Err("artifact operation limit must be at least 6".into());
    }
    if args.exploration_epsilon_millionths > 1_000_000 {
        return Err("exploration epsilon millionths must be <= 1000000".into());
    }
    if args.exploration_epsilon_millionths > 0 && args.exploration_max_budget_nano_usd == 0 {
        return Err("enabled exploration requires a non-zero maximum budget".into());
    }
    if args.exploration_epsilon_millionths > 0 && args.records.is_none() && args.redis_url.is_none()
    {
        return Err("enabled exploration requires --records or Redis authoritative state".into());
    }
    if (args.artifact_active.is_some() || args.artifact_candidate.is_some())
        && args.artifact_signing_key_env.is_none()
    {
        return Err("artifact files require --artifact-signing-key-env".into());
    }
    if args.artifact_active.is_none()
        && args.artifact_candidate.is_none()
        && args.artifact_signing_key_env.is_some()
    {
        return Err("artifact signing key requires an artifact file".into());
    }
    configured_control_failure_policy(args)?;
    configured_picker(args)?;
    if args.redis_url.is_some() && (args.records.is_some() || args.feedback_records.is_some()) {
        return Err(
            "--records/--feedback-records cannot be combined with Redis authoritative state".into(),
        );
    }
    if args.redis_url.is_some() && args.on_state_unavailable.as_deref() != Some("fail_closed") {
        return Err("Redis requires --on-state-unavailable fail_closed".into());
    }
    Ok(())
}

fn configured_control_failure_policy(args: &Args) -> Result<ControlFailurePolicy, BoxError> {
    match args.control_failure_policy.as_str() {
        "last_good" => Ok(ControlFailurePolicy::LastGood),
        "fail_closed" => Ok(ControlFailurePolicy::FailClosed),
        _ => Err("control failure policy must be last_good or fail_closed".into()),
    }
}

fn control_signing_key(args: &Args) -> Result<Option<Vec<u8>>, BoxError> {
    args.control_signing_key_env
        .as_ref()
        .map(|variable| {
            let value = env::var(variable)
                .map_err(|_| "control signing key environment variable is not set")?;
            if value.is_empty() {
                return Err("control signing key must not be empty".into());
            }
            Ok(value.into_bytes())
        })
        .transpose()
}

fn spawn_control_reloader(args: &Args, state: &AppState, signing_key: Option<Vec<u8>>) {
    let Some(manifest_path) = args.control_manifest.clone() else {
        return;
    };
    let catalog_path = args.catalog.clone();
    let route_path = args.route.clone();
    let interval = Duration::from_secs(args.control_reload_seconds);
    let control = state.control.clone();
    let capacity = Arc::clone(&state.capacity);
    tokio::spawn(async move {
        loop {
            sleep(interval).await;
            match ControlSnapshot::load(
                &catalog_path,
                &route_path,
                &manifest_path,
                signing_key.as_deref(),
            ) {
                Ok(candidate) => {
                    for tier in &candidate.route.tiers {
                        capacity.register(&tier.effective_deployments());
                    }
                    control.publish(candidate);
                }
                Err(error) => control.reject(error),
            }
        }
    });
}

fn configured_picker(args: &Args) -> Result<DeploymentPicker, BoxError> {
    serde_json::from_value(Value::String(args.deployment_picker.clone()))
        .map_err(|_| "deployment picker must be weighted, least_loaded, lowest_latency, or lowest_quota_usage".into())
}

async fn build_binding_repository(args: &Args) -> Result<Arc<dyn TaskBindingRepository>, BoxError> {
    if let Some(url) = &args.redis_url {
        return Ok(Arc::new(
            RedisTaskBindingRepository::connect(
                url,
                args.redis_prefix.clone(),
                args.task_binding_ttl_seconds,
            )
            .await?,
        ));
    }
    Ok(Arc::new(MemoryTaskBindingRepository::new(
        args.task_binding_capacity,
    )))
}

fn configured_cooldown_policy(args: &Args) -> CooldownPolicy {
    CooldownPolicy {
        cooldown: Duration::from_millis(args.cooldown_ms),
        window: Duration::from_millis(args.cooldown_window_ms),
        failure_threshold_millis: args.cooldown_failure_threshold_millis,
    }
}

async fn build_circuit_repository(
    args: &Args,
    policy: CooldownPolicy,
) -> Result<Arc<dyn SharedCircuitRepository>, BoxError> {
    if let Some(url) = &args.redis_url {
        return Ok(Arc::new(
            RedisCircuitRepository::connect(url, args.redis_prefix.clone(), policy).await?,
        ));
    }
    Ok(Arc::new(LocalCircuitRepository::default()))
}

async fn build_shared_state(args: &Args) -> Result<Option<RedisSharedState>, BoxError> {
    match &args.redis_url {
        Some(url) => Ok(Some(
            RedisSharedState::connect(url, args.redis_prefix.clone()).await?,
        )),
        None => Ok(None),
    }
}

fn build_idempotency_repository(
    args: &Args,
    shared_state: Option<&RedisSharedState>,
) -> Arc<dyn IdempotencyRepository> {
    match shared_state {
        Some(state) => RedisIdempotencyRepository::new(state.clone()),
        None => MemoryIdempotencyRepository::new(args.idempotency_capacity),
    }
}

fn build_record_repository(
    shared_state: Option<&RedisSharedState>,
    local: RecordStore,
    capacity: usize,
) -> Arc<dyn DecisionRecordRepository> {
    match shared_state {
        Some(state) => RedisDecisionRecordRepository::new(state.clone(), local, capacity),
        None => MemoryDecisionRecordRepository::new(local),
    }
}

async fn build_quota_repository(args: &Args) -> Result<Arc<dyn QuotaRepository>, BoxError> {
    if let Some(url) = &args.redis_url {
        return Ok(RedisQuotaRepository::connect(
            url,
            args.redis_prefix.clone(),
            args.tenant_max_in_flight,
            args.tenant_requests_per_minute,
            args.tenant_tokens_per_minute,
            Duration::from_secs(args.quota_lease_ttl_seconds),
        )
        .await?);
    }
    Ok(MemoryQuotaRepository::new(
        args.tenant_max_in_flight,
        args.tenant_requests_per_minute,
        args.tenant_tokens_per_minute,
    ))
}

async fn build_budget_repository(args: &Args) -> Result<Arc<dyn BudgetRepository>, BoxError> {
    if let Some(url) = &args.redis_url {
        return Ok(RedisBudgetRepository::connect(
            url,
            args.redis_prefix.clone(),
            args.tenant_budget_nano_usd,
            args.budget_period_seconds,
        )
        .await?);
    }
    Ok(MemoryBudgetRepository::new(
        args.tenant_budget_nano_usd,
        args.budget_period_seconds,
    ))
}

async fn reconcile_feedback(records: &RecordStore, feedback: &FeedbackStore) {
    let feedback = feedback.values.read().await;
    let mut records = records.records.write().await;
    for record in &mut *records {
        if let Some(signals) = record
            .trace_turn
            .as_ref()
            .and_then(|turn| feedback.get(&feedback_scope_key(&record.tenant_key, turn)))
        {
            record.outcome_signals = signals.values().cloned().collect();
        }
    }
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn readiness(State(state): State<AppState>) -> Response {
    if !state.accepting.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "draining"})),
        )
            .into_response();
    }
    let control = state.control.status();
    if !control.ready {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "reason": "control_revision_unavailable", "control": control})),
        )
            .into_response();
    }
    if let Some(shared) = &state.shared_state
        && shared.ping().await.is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "reason": "state_backend_unavailable"})),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({"status": "ready", "control": control})),
    )
        .into_response()
}

async fn openapi() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        include_str!("../../../gateway/openapi.json"),
    )
        .into_response()
}

async fn tiers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
    let state = state.with_active_control();
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "tiers.read",
        None,
    )
    .await?;
    let health = state
        .capacity
        .snapshot()
        .into_iter()
        .map(|item| (item.deployment.clone(), item))
        .collect::<BTreeMap<_, _>>();
    let mut tiers = Vec::new();
    for tier in &state.route.tiers {
        let mut deployments = Vec::new();
        for deployment in tier.effective_deployments() {
            let shared_circuits = state
                .shared_circuits
                .snapshot(&deployment)
                .await
                .map_err(state_backend_unavailable)?;
            deployments.push(json!({
                "id": deployment.id,
                "model": deployment.model,
                "base_url_override": deployment.base_url.is_some(),
                "weight": deployment.weight,
                "order": deployment.order,
                "health": health.get(&deployment.id),
                "shared_circuits": shared_circuits
            }));
        }
        tiers.push(json!({
                "tier": tier.tier,
                "fallbacks": tier.fallbacks,
                "deployments": deployments
        }));
    }
    Ok(Json(json!({"object": "list", "data": tiers})))
}

async fn task_binding(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<TaskBinding>, GatewayError> {
    validate_scope_value("task.id", &id)?;
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "task_binding.read",
        Some(task_scope_key(&tenant_key, &id)),
    )
    .await?;
    state
        .bindings
        .get(&task_scope_key(&tenant_key, &id))
        .await
        .map_err(state_backend_unavailable)?
        .map(Json)
        .ok_or_else(|| GatewayError {
            status: StatusCode::NOT_FOUND,
            code: "task_binding_not_found",
            message: "task binding was not found".to_owned(),
            request_id: None,
            decision_id: None,
        })
}

async fn delete_task_binding(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, GatewayError> {
    validate_scope_value("task.id", &id)?;
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Operator,
        "task_binding.delete",
        Some(task_scope_key(&tenant_key, &id)),
    )
    .await?;
    if state
        .bindings
        .remove_task(&tenant_key, &task_scope_key(&tenant_key, &id))
        .await
        .map_err(state_backend_unavailable)?
        > 0
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
}

async fn session_binding(
    State(state): State<AppState>,
    Path((conversation, branch)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<TaskBinding>, GatewayError> {
    validate_scope_value("trace.conversation", &conversation)?;
    validate_scope_value("trace.branch", &branch)?;
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    let scope_key = session_scope_key(&tenant_key, &conversation, &branch);
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "session_binding.read",
        Some(scope_key.clone()),
    )
    .await?;
    state
        .bindings
        .get(&scope_key)
        .await
        .map_err(state_backend_unavailable)?
        .map(Json)
        .ok_or_else(|| GatewayError {
            status: StatusCode::NOT_FOUND,
            code: "session_binding_not_found",
            message: "session binding was not found".to_owned(),
            request_id: None,
            decision_id: None,
        })
}

async fn delete_session_binding(
    State(state): State<AppState>,
    Path((conversation, branch)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, GatewayError> {
    validate_scope_value("trace.conversation", &conversation)?;
    validate_scope_value("trace.branch", &branch)?;
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    let scope_key = session_scope_key(&tenant_key, &conversation, &branch);
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Operator,
        "session_binding.delete",
        Some(scope_key.clone()),
    )
    .await?;
    if state
        .bindings
        .remove(&scope_key)
        .await
        .map_err(state_backend_unavailable)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
}

#[allow(clippy::too_many_lines)]
async fn metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    authorize_metrics(&state, &headers).await?;
    let metric = &state.metrics;
    let resident_memory_bytes = process_resident_memory_bytes().await.unwrap_or(0);
    let mut body = format!(
        concat!(
            "# TYPE urouter_requests_total counter\n",
            "urouter_requests_total {}\n",
            "# TYPE urouter_llm_calls_total counter\n",
            "urouter_llm_calls_total {}\n",
            "# TYPE urouter_upstream_attempts_total counter\n",
            "urouter_upstream_attempts_total {}\n",
            "# TYPE urouter_usage_unavailable_total counter\n",
            "urouter_usage_unavailable_total {}\n",
            "# TYPE urouter_cache_affinity_hit_total counter\n",
            "urouter_cache_affinity_hit_total {}\n",
            "# TYPE urouter_vector_write_total counter\n",
            "urouter_vector_write_total {}\n",
            "# TYPE urouter_vector_write_errors_total counter\n",
            "urouter_vector_write_errors_total {}\n",
            "# TYPE urouter_success_total counter\n",
            "urouter_success_total {}\n",
            "# TYPE urouter_errors_total counter\n",
            "urouter_errors_total {}\n",
            "# TYPE urouter_retry_total counter\n",
            "urouter_retry_total {}\n",
            "# TYPE urouter_feedback_signals_total counter\n",
            "urouter_feedback_signals_total {}\n",
            "# TYPE urouter_paired_total counter\n",
            "urouter_paired_total {}\n",
            "# TYPE urouter_paired_rejected_total counter\n",
            "urouter_paired_rejected_total {}\n",
            "# TYPE urouter_streams_completed_total counter\n",
            "urouter_streams_completed_total {}\n",
            "# TYPE urouter_stream_failures_total counter\n",
            "urouter_stream_failures_total {}\n",
            "# TYPE urouter_fallback_total counter\n",
            "urouter_fallback_total {}\n",
            "# TYPE urouter_task_binding_created_total counter\n",
            "urouter_task_binding_created_total {}\n",
            "# TYPE urouter_task_binding_applied_total counter\n",
            "urouter_task_binding_applied_total {}\n",
            "# TYPE urouter_task_binding_migration_total counter\n",
            "urouter_task_binding_migration_total {}\n",
            "# TYPE urouter_compatibility_request_total counter\n",
            "urouter_compatibility_request_total {}\n",
            "# TYPE urouter_task_binding_conflict_total counter\n",
            "urouter_task_binding_conflict_total {}\n",
            "# TYPE urouter_quota_rejection_total counter\n",
            "urouter_quota_rejection_total {}\n",
            "# TYPE urouter_budget_rejection_total counter\n",
            "urouter_budget_rejection_total {}\n",
            "# TYPE urouter_deployment_filter_rejection_total counter\n",
            "# TYPE urouter_record_dropped_total counter\n",
            "urouter_record_dropped_total {}\n",
            "# TYPE urouter_record_write_errors_total counter\n",
            "urouter_record_write_errors_total {}\n",
            "# TYPE urouter_feedback_dropped_total counter\n",
            "urouter_feedback_dropped_total {}\n",
            "# TYPE urouter_feedback_write_errors_total counter\n",
            "urouter_feedback_write_errors_total {}\n",
            "# TYPE process_resident_memory_bytes gauge\n",
            "process_resident_memory_bytes {}\n"
        ),
        metric.requests.load(Ordering::Relaxed),
        metric.requests.load(Ordering::Relaxed),
        metric.upstream_attempts.load(Ordering::Relaxed),
        metric.usage_unavailable.load(Ordering::Relaxed),
        metric.cache_affinity_hits.load(Ordering::Relaxed),
        metric.vector_writes.load(Ordering::Relaxed),
        metric.vector_write_errors.load(Ordering::Relaxed),
        metric.successes.load(Ordering::Relaxed),
        metric.errors.load(Ordering::Relaxed),
        metric.retries.load(Ordering::Relaxed),
        metric.feedback_signals.load(Ordering::Relaxed),
        metric.paired.load(Ordering::Relaxed),
        metric.paired_rejected.load(Ordering::Relaxed),
        metric.streams_completed.load(Ordering::Relaxed),
        metric.stream_failures.load(Ordering::Relaxed),
        metric.fallbacks.load(Ordering::Relaxed),
        metric.bindings_created.load(Ordering::Relaxed),
        metric.bindings_applied.load(Ordering::Relaxed),
        metric.binding_migrations.load(Ordering::Relaxed),
        metric.compatibility_requests.load(Ordering::Relaxed),
        metric.binding_conflicts.load(Ordering::Relaxed),
        metric.quota_rejections.load(Ordering::Relaxed),
        metric.budget_rejections.load(Ordering::Relaxed),
        state.records.dropped.load(Ordering::Relaxed),
        state.records.write_errors.load(Ordering::Relaxed),
        state.feedback.dropped.load(Ordering::Relaxed),
        state.feedback.write_errors.load(Ordering::Relaxed),
        resident_memory_bytes,
    );
    for (reason, count) in metric
        .filter_rejections
        .lock()
        .expect("filter metric lock poisoned")
        .iter()
    {
        let _ = writeln!(
            body,
            "urouter_deployment_filter_rejection_total{{reason=\"{reason}\"}} {count}"
        );
    }
    render_histogram_metrics(metric, &mut body);
    body.push_str("# EOF\n");
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
    );
    Ok(response)
}

fn render_histogram_metrics(metric: &GatewayMetrics, body: &mut String) {
    metric.request_duration_ms.render(
        body,
        "urouter_request_duration_milliseconds",
        "End-to-end Gateway request duration in milliseconds",
    );
    metric.upstream_duration_ms.render(
        body,
        "urouter_upstream_duration_milliseconds",
        "Upstream execution duration in milliseconds",
    );
    metric.ttft_ms.render(
        body,
        "urouter_time_to_first_token_milliseconds",
        "Streaming time to first upstream data chunk in milliseconds",
    );
    metric.cost_nano_usd.render(
        body,
        "urouter_request_cost_nano_usd",
        "Settled request cost in integer nano-USD",
    );
    metric.fallback_depth.render(
        body,
        "urouter_fallback_depth",
        "Fallback depth reached by a routed request",
    );
}

async fn authorize_metrics(state: &AppState, headers: &HeaderMap) -> Result<(), GatewayError> {
    let tenant_key = resolve_tenant_key(state, headers)?;
    authorize_management(
        state,
        headers,
        &tenant_key,
        ManagementRole::Admin,
        "metrics.read",
        None,
    )
    .await
}

async fn process_resident_memory_bytes() -> Option<u64> {
    let status = tokio::fs::read_to_string("/proc/self/status").await.ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kibibytes| kibibytes.saturating_mul(1_024))
    })
}

async fn models(State(state): State<AppState>) -> Result<Json<Value>, GatewayError> {
    let state = state.with_active_control();
    let control_status = state.control.status();
    let capabilities = state
        .route
        .capabilities(&state.catalog)
        .map_err(GatewayError::from)?;
    let routable_capabilities = state
        .route
        .routable_capabilities(&state.catalog)
        .map_err(GatewayError::from)?;
    let tiers = state
        .route
        .tiers
        .iter()
        .map(|tier| json!({"tier": tier.tier, "model": tier.model}))
        .collect::<Vec<_>>();
    let mut data = vec![json!({
        "id": state.route.id,
        "object": "model",
        "owned_by": "urouter",
        "urouter": {
            "kind": "auto",
            "control_revision": control_status.revision,
            "contract_version": 2,
            "contract_versions": [1, 2],
            "tiers": tiers,
            "capabilities": capabilities,
            "routable_capabilities": routable_capabilities,
            "preference_bias": {"supported": true, "default": f64::from(state.route.default_preference_bias_millis) / 1000.0, "range": [-1.0, 1.0]},
            "override": {"supported": true, "granularity": ["turn", "session"]}
            ,"task_binding": {
                "supported": true,
                "backend": state.bindings.backend_name(),
                "circuit_backend": state.shared_circuits.backend_name(),
                "idempotency_backend": state.idempotency.backend_name(),
                "record_backend": state.record_repository.backend_name(),
                "quota_backend": state.quota.backend_name(),
                "tenant_max_in_flight": state.quota.max_in_flight(),
                "tenant_requests_per_minute": state.quota.requests_per_minute(),
                "tenant_tokens_per_minute": state.quota.tokens_per_minute(),
                "budget_backend": state.budget.backend_name(),
                "tenant_budget_nano_usd": state.budget.limit_nano_usd(),
                "deployment_picker": state.capacity.picker(),
                "record_feedback_backend": if state.shared_state.is_some() { "redis" } else { "local" },
                "call_roles": ["primary", "auxiliary"],
                "migration_boundaries": [
                    "new_task", "after_compaction", "tool_round_completed",
                    "before_first_assistant_token", "explicit_user_retry",
                    "terminal_provider_failure"
                ]
            },
            "session_binding": {
                "supported": true,
                "scope": ["conversation", "branch"],
                "identity": ["model", "provider", "api", "prompt_profile_hash", "toolset_hash"],
                "legacy_task_binding_contract_version": 1
            },
            "agent_adapters": {
                "harnesses": ["aionui", "workbuddy"],
                "base_paths": ["/v1/adapters/aionui", "/v1/adapters/workbuddy"],
                "call_kinds": ["primary", "plan", "verify", "title", "summary", "compress"]
            }
        }
    })];
    data.extend(state.catalog.models().map(|model| {
        json!({
            "id": model.id,
            "object": "model",
            "owned_by": model.provider,
            "urouter": {"kind": "model", "capabilities": model.capabilities}
        })
    }));
    Ok(Json(json!({"object": "list", "data": data})))
}

async fn catalog_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(HeaderMap, Json<Value>), GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "catalog.read",
        None,
    )
    .await?;
    let snapshot = state.control.snapshot();
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", snapshot.revision))
            .map_err(|_| GatewayError::internal("control revision is not a valid ETag"))?,
    );
    Ok((
        response_headers,
        Json(json!({
            "schema_version": 1,
            "control": state.control.status(),
            "catalog_hash": snapshot.catalog.hashes().content,
            "route_revision": snapshot.route.revision(),
            "providers": snapshot.catalog.providers().count(),
            "models": snapshot.catalog.models().count(),
            "hot_reload": state.control_source.is_some()
        })),
    ))
}

async fn refresh_catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Admin,
        "catalog.refresh",
        None,
    )
    .await?;
    let source = state.control_source.as_ref().ok_or_else(|| GatewayError {
        status: StatusCode::CONFLICT,
        code: "control_reload_not_configured",
        message: "Gateway was not started with --control-manifest".to_owned(),
        request_id: None,
        decision_id: None,
    })?;
    let candidate = ControlSnapshot::load(
        &source.catalog,
        &source.route,
        &source.manifest,
        source.signing_key.as_deref(),
    )
    .map_err(|error| {
        state.control.reject(&error);
        GatewayError {
            status: StatusCode::CONFLICT,
            code: "control_candidate_rejected",
            message: error.to_string(),
            request_id: None,
            decision_id: None,
        }
    })?;
    for tier in &candidate.route.tiers {
        state.capacity.register(&tier.effective_deployments());
    }
    let changed = state.control.publish(candidate);
    Ok(Json(
        json!({"changed": changed, "control": state.control.status()}),
    ))
}

async fn rollback_catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Admin,
        "catalog.rollback",
        None,
    )
    .await?;
    if !state.control.rollback() {
        return Err(GatewayError {
            status: StatusCode::CONFLICT,
            code: "control_rollback_unavailable",
            message: "no previous control revision is available".to_owned(),
            request_id: None,
            decision_id: None,
        });
    }
    Ok(Json(
        json!({"rolled_back": true, "control": state.control.status()}),
    ))
}

async fn explain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let state = state.with_active_control();
    let (tenant_key, tenant_compatibility) = resolve_tenant(&state, &headers)?;
    let feature_frame = FeatureFrame::from_openai_chat(&request);
    let mut decision = state.route.decide(&state.catalog, &request)?;
    let task_key = artifact_task_key(&decision, "explain");
    let artifact = apply_artifact_policy(&state, &request, &tenant_key, &task_key, &mut decision)?;
    decision.compatibility_mode |= tenant_compatibility;
    validate_decision_scope(&decision)?;
    let governance = request_governance(&state, tenant_key, &decision).await?;
    apply_task_binding_inner(&state, &governance, &mut decision, false).await?;
    let retained = governance.policy.recording != RecordingMode::None;
    let routing_trace = RoutingTrace::current_admission_summary(
        decision.model.to_string(),
        decision.alternatives.iter().map(ToString::to_string),
        decision
            .admission
            .excluded
            .iter()
            .map(|excluded| (excluded.model.to_string(), excluded.reasons.clone())),
        decision.reason.clone(),
    )
    .with_decisions(decision.cascade_trace.clone());
    Ok(Json(json!({
        "schema_version": 1,
        "feature_frame": feature_frame,
        "routing_trace": routing_trace,
        "route_id": decision.route_id,
        "tier": decision.tier,
        "model": decision.model,
        "reason": decision.reason,
        "artifact": artifact,
        "semantic": decision.semantic,
        "alternatives": decision.alternatives,
        "requirement": decision.requirement,
        "admission": decision.admission,
        "compatibility_mode": governance.compatibility_mode,
        "task_binding_applied": decision.reason == "task_binding",
        "data_policy": {
            "recording": governance.policy.recording,
            "retention_days": governance.policy.retention_days,
            "training_eligible": retained && governance.policy.allow_training && !governance.compatibility_mode,
            "remote_judge_eligible": retained && governance.policy.allow_remote_judge && !governance.compatibility_mode
        }
    })))
}

async fn artifact_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "artifacts.status",
        None,
    )
    .await?;
    let runtime = state.artifact.as_ref().ok_or_else(|| GatewayError {
        status: StatusCode::NOT_FOUND,
        code: "artifact_runtime_not_configured",
        message: "gateway was not started with an artifact".to_owned(),
        request_id: None,
        decision_id: None,
    })?;
    let current_catalog = state
        .control
        .snapshot()
        .catalog
        .hashes()
        .content
        .to_string();
    let current_route = state.control.snapshot().route.revision();
    Ok(Json(json!({
        "status": runtime.controller.status(),
        "bound_revisions": {
            "catalog": runtime.catalog_revision,
            "route": runtime.route_revision
        },
        "stale": current_catalog != runtime.catalog_revision || current_route != runtime.route_revision,
        "audit": runtime.controller.audit_events()
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactAction {
    #[serde(default = "default_artifact_action_reason")]
    reason: String,
}

fn default_artifact_action_reason() -> String {
    "operator action".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactKillAction {
    killed: bool,
    #[serde(default = "default_artifact_action_reason")]
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactRolloutAction {
    rollout: RolloutPolicy,
    #[serde(default = "default_artifact_action_reason")]
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactObservationAction {
    observation: CanaryObservation,
    thresholds: RollbackThresholds,
}

async fn promote_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(action): Json<ArtifactAction>,
) -> Result<Json<Value>, GatewayError> {
    authorize_artifact_action(&state, &headers, "artifacts.promote").await?;
    let runtime = configured_artifact(&state)?;
    if !runtime.controller.promote(&action.reason) {
        return Err(GatewayError {
            status: StatusCode::CONFLICT,
            code: "artifact_candidate_unavailable",
            message: "no candidate artifact is available for promotion".to_owned(),
            request_id: None,
            decision_id: None,
        });
    }
    Ok(Json(
        json!({"promoted": true, "status": runtime.controller.status()}),
    ))
}

async fn rollback_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(action): Json<ArtifactAction>,
) -> Result<Json<Value>, GatewayError> {
    authorize_artifact_action(&state, &headers, "artifacts.rollback").await?;
    let runtime = configured_artifact(&state)?;
    if !runtime.controller.rollback(&action.reason) {
        return Err(GatewayError {
            status: StatusCode::CONFLICT,
            code: "artifact_rollback_unavailable",
            message: "no last-good artifact is available".to_owned(),
            request_id: None,
            decision_id: None,
        });
    }
    Ok(Json(
        json!({"rolled_back": true, "status": runtime.controller.status()}),
    ))
}

async fn kill_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(action): Json<ArtifactKillAction>,
) -> Result<Json<Value>, GatewayError> {
    authorize_artifact_action(&state, &headers, "artifacts.kill").await?;
    let runtime = configured_artifact(&state)?;
    runtime
        .controller
        .set_kill_switch(action.killed, &action.reason);
    Ok(Json(
        json!({"killed": action.killed, "status": runtime.controller.status()}),
    ))
}

async fn update_artifact_rollout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(action): Json<ArtifactRolloutAction>,
) -> Result<Json<Value>, GatewayError> {
    authorize_artifact_action(&state, &headers, "artifacts.rollout").await?;
    let runtime = configured_artifact(&state)?;
    runtime
        .controller
        .update_rollout(action.rollout, &action.reason)
        .map_err(|error| GatewayError::bad_request("invalid_artifact_rollout", error))?;
    Ok(Json(
        json!({"updated": true, "status": runtime.controller.status()}),
    ))
}

async fn observe_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(action): Json<ArtifactObservationAction>,
) -> Result<Json<Value>, GatewayError> {
    authorize_artifact_action(&state, &headers, "artifacts.observe").await?;
    let runtime = configured_artifact(&state)?;
    let rolled_back = runtime
        .controller
        .observe_and_maybe_rollback(action.observation, action.thresholds);
    Ok(Json(json!({
        "rolled_back": rolled_back,
        "status": runtime.controller.status(),
        "audit": runtime.controller.audit_events()
    })))
}

fn configured_artifact(state: &AppState) -> Result<&ArtifactRuntime, GatewayError> {
    state.artifact.as_ref().ok_or_else(|| GatewayError {
        status: StatusCode::NOT_FOUND,
        code: "artifact_runtime_not_configured",
        message: "gateway was not started with an artifact".to_owned(),
        request_id: None,
        decision_id: None,
    })
}

async fn authorize_artifact_action(
    state: &AppState,
    headers: &HeaderMap,
    action: &str,
) -> Result<(), GatewayError> {
    let tenant_key = resolve_tenant_key(state, headers)?;
    authorize_management(
        state,
        headers,
        &tenant_key,
        ManagementRole::Admin,
        action,
        None,
    )
    .await
}

fn artifact_task_key(decision: &RouteDecision, fallback: &str) -> String {
    decision
        .task_id
        .as_deref()
        .or(decision.conversation_id.as_deref())
        .or(decision.trace_id.as_deref())
        .unwrap_or(fallback)
        .to_owned()
}

fn semantic_task_name(decision: &RouteDecision) -> &'static str {
    use urouter_gateway::SemanticTask;
    match decision.semantic.task {
        SemanticTask::Greeting => "greeting",
        SemanticTask::RealtimeWeather => "realtime_weather",
        SemanticTask::EquationSolving => "equation_solving",
        SemanticTask::General => "general",
    }
}

fn apply_artifact_policy(
    state: &AppState,
    request: &Value,
    tenant_key: &str,
    task_key: &str,
    decision: &mut RouteDecision,
) -> Result<Option<ArtifactDecision>, GatewayError> {
    let Some(runtime) = &state.artifact else {
        return Ok(None);
    };
    if decision.reason == "explicit_model" {
        return Ok(None);
    }
    if runtime.catalog_revision != state.catalog.hashes().content.to_string()
        || runtime.route_revision != state.route.revision()
    {
        return Ok(Some(ArtifactDecision {
            applied_tier: decision.tier.clone(),
            source: DecisionSource::Rule,
            active_revision: runtime.controller.status().active_revision,
            candidate_revision: runtime.controller.status().candidate_revision,
            shadow_tier: None,
            fallback_reason: Some("control_revision_changed".to_owned()),
            canary_bucket: None,
        }));
    }
    let artifact = runtime.controller.decide(
        &FeatureFrame::from_openai_chat(request),
        semantic_task_name(decision),
        tenant_key,
        task_key,
        &decision.tier,
    );
    if artifact.source != DecisionSource::Rule && artifact.applied_tier != decision.tier {
        *decision = route_pinned_tier(state, request, &artifact.applied_tier)?;
        match artifact.source {
            DecisionSource::ActiveArtifact => "artifact_active",
            DecisionSource::CandidateCanary => "artifact_candidate_canary",
            DecisionSource::Rule => "artifact_rule_fallback",
        }
        .clone_into(&mut decision.reason);
    }
    Ok(Some(artifact))
}

fn route_pinned_tier(
    state: &AppState,
    request: &Value,
    tier: &str,
) -> Result<RouteDecision, GatewayError> {
    let mut pinned = request.clone();
    let root = pinned.as_object_mut().ok_or_else(|| {
        GatewayError::bad_request("invalid_request", "request must be a JSON object")
    })?;
    let urouter = root
        .entry("urouter")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| GatewayError::bad_request("invalid_request", "urouter must be an object"))?;
    let preference = urouter
        .entry("preference")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            GatewayError::bad_request("invalid_request", "urouter.preference must be an object")
        })?;
    preference.insert("pin_tier".to_owned(), tier.into());
    Ok(state.route.decide(&state.catalog, &pinned)?)
}

fn apply_controlled_exploration(
    state: &AppState,
    request: &Value,
    tenant_key: &str,
    task_key: &str,
    compatibility_mode: bool,
    decision: &mut RouteDecision,
) -> Result<Option<ExplorationRecord>, GatewayError> {
    let policy = state.exploration;
    let Some(data_policy) = decision.data_policy.as_ref() else {
        return Ok(None);
    };
    if policy.epsilon_millionths == 0
        || compatibility_mode
        || decision.reason == "explicit_model"
        || data_policy.recording == RecordingMode::None
        || !data_policy.allow_training
        || !data_policy.allow_exploration
        || data_policy.exploration_budget_nano_usd == 0
        || data_policy.exploration_budget_nano_usd > policy.maximum_budget_nano_usd
    {
        return Ok(None);
    }
    let authorized_budget_nano_usd = data_policy.exploration_budget_nano_usd;
    let baseline_tier = decision.tier.clone();
    let mut eligible = Vec::new();
    for tier in &state.route.tiers {
        if route_pinned_tier(state, request, &tier.tier).is_ok() {
            eligible.push(tier.tier.clone());
        }
    }
    eligible.sort();
    eligible.dedup();
    if eligible.len() < 2 || !eligible.contains(&baseline_tier) {
        return Ok(None);
    }
    let digest = Sha256::digest(format!("{tenant_key}\0{task_key}\0exploration-v1").as_bytes());
    let draw = u32::from_be_bytes(digest[..4].try_into().expect("SHA-256 prefix")) % 1_000_000;
    let selected_by_exploration = draw < policy.epsilon_millionths;
    let selected_tier = if selected_by_exploration {
        let bucket = u32::from_be_bytes(digest[4..8].try_into().expect("SHA-256 bucket"));
        let index = usize::try_from(bucket).unwrap_or(usize::MAX) % eligible.len();
        eligible[index].clone()
    } else {
        baseline_tier.clone()
    };
    let random_probability =
        policy.epsilon_millionths / u32::try_from(eligible.len()).unwrap_or(u32::MAX);
    let propensity_millionths = if selected_tier == baseline_tier {
        1_000_000_u32
            .saturating_sub(policy.epsilon_millionths)
            .saturating_add(random_probability)
    } else {
        random_probability
    }
    .max(1);
    if selected_tier != decision.tier {
        *decision = route_pinned_tier(state, request, &selected_tier)?;
        "controlled_exploration".clone_into(&mut decision.reason);
    }
    Ok(Some(ExplorationRecord {
        epsilon_millionths: policy.epsilon_millionths,
        propensity_millionths,
        eligible_set: eligible,
        selected_by_exploration,
        authorized_budget_nano_usd,
    }))
}

async fn decisions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DecisionQuery>,
) -> Result<Json<Value>, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "decisions.list",
        None,
    )
    .await?;
    let records = records_for_tenant(&state, &tenant_key).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let start = query
        .after
        .as_ref()
        .and_then(|cursor| {
            records
                .iter()
                .position(|record| record.decision_id == *cursor)
                .map(|index| index.saturating_add(1))
        })
        .unwrap_or(0);
    let data = records
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let next_cursor = (start + data.len() < records.len())
        .then(|| data.last().map(|record| record.decision_id.clone()))
        .flatten();
    Ok(Json(json!({
        "object": "list",
        "data": data,
        "next_cursor": next_cursor,
        "retained": records.len(),
        "dropped": state.records.dropped.load(Ordering::Relaxed),
        "write_errors": state.records.write_errors.load(Ordering::Relaxed)
    })))
}

#[derive(Debug, Default, Serialize)]
struct ValueStats {
    records: u64,
    successful_records: u64,
    usage_available: u64,
    usage_unavailable: u64,
    catalog_revision_mismatch: u64,
    actual_cost_nano_usd: u64,
    always_capable_cost_nano_usd: u64,
    downgrade_savings_nano_usd: u64,
    cache_savings_nano_usd: u64,
    total_savings_nano_usd: u64,
    feedback_signals: u64,
    quality_mean: Option<f64>,
    quality_loss_vs_perfect: Option<f64>,
}

async fn stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
    let state = state.with_active_control();
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "stats.read",
        None,
    )
    .await?;
    let records = records_for_tenant(&state, &tenant_key).await?;
    let capable_model = state
        .route
        .tiers
        .last()
        .and_then(|tier| state.catalog.model(&tier.model));
    let value = calculate_value_stats(&records, &state.catalog, capable_model);
    Ok(Json(json!({
        "schema_version": 1,
        "tenant": tenant_key,
        "route": state.route.id,
        "value": value,
        "methodology": {
            "downgrade": "same_usage_uncached_reprice_against_last_route_tier",
            "cache": "selected_model_same_usage_reprice_without_cache",
            "quality": "mean_of_observed_feedback_strengths",
            "limitations": [
                "counterfactual pricing assumes identical token usage",
                "records from another catalog revision are excluded from repricing",
                "quality is null until feedback exists"
            ]
        }
    })))
}

#[allow(clippy::cast_precision_loss)]
fn calculate_value_stats(
    records: &[DecisionRecord],
    catalog: &CatalogSnapshot,
    capable_model: Option<&ModelSpec>,
) -> ValueStats {
    let mut stats = ValueStats {
        records: u64::try_from(records.len()).unwrap_or(u64::MAX),
        ..ValueStats::default()
    };
    let mut feedback_sum = 0.0;
    for record in records {
        stats.successful_records += u64::from(record.execution.ok);
        stats.feedback_signals = stats
            .feedback_signals
            .saturating_add(u64::try_from(record.outcome_signals.len()).unwrap_or(u64::MAX));
        feedback_sum += record
            .outcome_signals
            .iter()
            .map(|signal| signal.strength)
            .sum::<f64>();
        let Some(usage) = record.execution.usage else {
            stats.usage_unavailable = stats.usage_unavailable.saturating_add(1);
            continue;
        };
        stats.usage_available = stats.usage_available.saturating_add(1);
        if record.evidence.content_hash != catalog.hashes().content {
            stats.catalog_revision_mismatch = stats.catalog_revision_mismatch.saturating_add(1);
            continue;
        }
        let Some(selected) = catalog.model(&record.evidence.model_id) else {
            stats.catalog_revision_mismatch = stats.catalog_revision_mismatch.saturating_add(1);
            continue;
        };
        let Ok(actual) = calculate_actual_cost(&selected.cost, usage) else {
            continue;
        };
        let uncached_usage = Usage {
            input: usage
                .input
                .saturating_add(usage.cache_read)
                .saturating_add(usage.cache_write),
            output: usage.output,
            reasoning: usage.reasoning,
            ..Usage::default()
        };
        let Ok(selected_uncached) = calculate_actual_cost(&selected.cost, uncached_usage) else {
            continue;
        };
        let actual_nano = money_to_u64(actual.total);
        let selected_uncached_nano = money_to_u64(selected_uncached.total);
        stats.actual_cost_nano_usd = stats.actual_cost_nano_usd.saturating_add(actual_nano);
        stats.cache_savings_nano_usd = stats
            .cache_savings_nano_usd
            .saturating_add(selected_uncached_nano.saturating_sub(actual_nano));
        let capable_uncached_nano = capable_model
            .and_then(|model| calculate_actual_cost(&model.cost, uncached_usage).ok())
            .map_or(selected_uncached_nano, |cost| money_to_u64(cost.total));
        stats.always_capable_cost_nano_usd = stats
            .always_capable_cost_nano_usd
            .saturating_add(capable_uncached_nano);
        stats.downgrade_savings_nano_usd = stats
            .downgrade_savings_nano_usd
            .saturating_add(capable_uncached_nano.saturating_sub(selected_uncached_nano));
    }
    stats.total_savings_nano_usd = stats
        .downgrade_savings_nano_usd
        .saturating_add(stats.cache_savings_nano_usd);
    if stats.feedback_signals > 0 {
        let mean = feedback_sum / stats.feedback_signals as f64;
        stats.quality_mean = Some(mean);
        stats.quality_loss_vs_perfect = Some(1.0 - mean);
    }
    stats
}

async fn decision_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DecisionRecord>, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "decision.read",
        Some(management_target_key(&tenant_key, "decision", &id)),
    )
    .await?;
    record_for_id(&state, &tenant_key, &id)
        .await?
        .map(Json)
        .ok_or_else(|| GatewayError {
            status: StatusCode::NOT_FOUND,
            code: "decision_not_found",
            message: format!("decision {id} was not found in retained records"),
            request_id: None,
            decision_id: None,
        })
}

async fn delete_decision(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Operator,
        "decision.delete",
        Some(management_target_key(&tenant_key, "decision", &id)),
    )
    .await?;
    let record = record_for_id(&state, &tenant_key, &id).await?;
    let turn = record.as_ref().and_then(|record| record.trace_turn.clone());
    let deleted = state
        .record_repository
        .delete(&tenant_key, RecordDelete::Decisions(vec![id.clone()]))
        .await
        .map_err(record_repository_error)?;
    if deleted > 0
        && let Some(record) = record.as_ref()
    {
        tombstone_record_vectors(state.vector_store.as_deref(), std::slice::from_ref(record))
            .await?;
    }
    if let Some(turn) = turn
        && !records_for_tenant(&state, &tenant_key)
            .await?
            .iter()
            .any(|record| record.trace_turn.as_deref() == Some(&turn))
    {
        delete_feedback_turns(&state, &tenant_key, std::slice::from_ref(&turn)).await?;
    }
    Ok(if deleted == 0 {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::NO_CONTENT
    })
}

async fn delete_task_records(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
    validate_scope_value("task.id", &id)?;
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    let task_key = task_scope_key(&tenant_key, &id);
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Operator,
        "task.records.delete",
        Some(task_key.clone()),
    )
    .await?;
    let matching = records_for_tenant(&state, &tenant_key)
        .await?
        .into_iter()
        .filter(|record| {
            record.tenant_key == tenant_key && record.task_key.as_ref() == Some(&task_key)
        })
        .collect::<Vec<_>>();
    let turns = matching
        .iter()
        .filter_map(|record| record.trace_turn.clone())
        .collect::<Vec<_>>();
    let decision_ids = matching
        .iter()
        .map(|record| record.decision_id.clone())
        .collect::<Vec<_>>();
    let bindings_deleted = state
        .bindings
        .remove_task(&tenant_key, &task_key)
        .await
        .map_err(state_backend_unavailable)?;
    let (_, current_task_generation) = state
        .bindings
        .scope_generation(&tenant_key, Some(&task_key))
        .await
        .map_err(state_backend_unavailable)?;
    let deleted = state
        .record_repository
        .delete(
            &tenant_key,
            RecordDelete::Task {
                decision_ids,
                task_key,
                before_generation: current_task_generation,
            },
        )
        .await
        .map_err(record_repository_error)?;
    if deleted > 0 {
        tombstone_record_vectors(state.vector_store.as_deref(), &matching).await?;
    }
    let retained_turns = records_for_tenant(&state, &tenant_key)
        .await?
        .into_iter()
        .filter_map(|record| record.trace_turn.clone())
        .collect::<BTreeSet<_>>();
    let removable_turns = turns
        .into_iter()
        .filter(|turn| !retained_turns.contains(turn))
        .collect::<Vec<_>>();
    let feedback_deleted = delete_feedback_turns(&state, &tenant_key, &removable_turns).await?;
    Ok(Json(
        json!({"deleted": deleted, "feedback_deleted": feedback_deleted, "bindings_deleted": bindings_deleted}),
    ))
}

async fn delete_tenant_records(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Admin,
        "tenant.records.delete",
        None,
    )
    .await?;
    let existing = records_for_tenant(&state, &tenant_key).await?;
    let decision_ids = existing
        .iter()
        .map(|record| record.decision_id.clone())
        .collect::<Vec<_>>();
    let turns = existing
        .iter()
        .filter_map(|record| record.trace_turn.clone())
        .collect::<Vec<_>>();
    let bindings_deleted = state
        .bindings
        .remove_tenant(&tenant_key)
        .await
        .map_err(state_backend_unavailable)?;
    let (current_tenant_generation, _) = state
        .bindings
        .scope_generation(&tenant_key, None)
        .await
        .map_err(state_backend_unavailable)?;
    let deleted = state
        .record_repository
        .delete(
            &tenant_key,
            RecordDelete::Tenant {
                decision_ids,
                before_generation: current_tenant_generation,
            },
        )
        .await
        .map_err(record_repository_error)?;
    if deleted > 0 {
        tombstone_record_vectors(state.vector_store.as_deref(), &existing).await?;
    }
    let local_feedback_deleted = state
        .feedback
        .delete_matching(|tenant, turn| {
            tenant == tenant_key && turns.iter().any(|candidate| candidate == turn)
        })
        .await?;
    let shared_feedback_deleted = if let Some(shared) = &state.shared_state {
        shared
            .delete_feedback(&tenant_key, &turns)
            .await
            .map_err(state_backend_unavailable)?
    } else {
        0
    };
    let feedback_deleted = shared_feedback_deleted.max(local_feedback_deleted);
    Ok(Json(json!({
        "deleted": deleted,
        "feedback_deleted": feedback_deleted,
        "bindings_deleted": bindings_deleted
    })))
}

async fn feedback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<FeedbackRequest>,
) -> Result<Json<Value>, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    if request.contract_version.is_some_and(|version| version != 1) {
        return Err(GatewayError::bad_request(
            "unsupported_contract_version",
            "feedback contract version must be 1",
        ));
    }
    if request.turn.trim().is_empty() || request.signals.is_empty() {
        return Err(GatewayError::bad_request(
            "invalid_feedback",
            "turn and at least one signal are required",
        ));
    }
    let policy = request.data_policy.unwrap_or_default();
    if !(1..=365).contains(&policy.retention_days) {
        return Err(GatewayError::bad_request(
            "invalid_data_policy",
            "data_policy.retention_days must be in 1..=365",
        ));
    }
    if policy.recording == RecordingMode::None {
        return Ok(Json(json!({
            "ok": true,
            "turn": request.turn,
            "recorded": false
        })));
    }
    let signals = request
        .signals
        .into_iter()
        .map(|signal| normalize_signal(&signal.kind, signal.strength))
        .collect::<Result<Vec<_>, _>>()?;
    ingest_feedback(
        &state,
        &tenant_key,
        &request.turn,
        signals,
        policy.retention_days,
    )
    .await?;
    Ok(Json(
        json!({"ok": true, "turn": request.turn, "recorded": true}),
    ))
}

async fn feedback_by_turn(
    State(state): State<AppState>,
    Path(turn): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Reader,
        "feedback.read",
        Some(management_target_key(&tenant_key, "feedback", &turn)),
    )
    .await?;
    let signals = feedback_for_turn(&state, &tenant_key, &turn)
        .await?
        .ok_or_else(|| GatewayError {
            status: StatusCode::NOT_FOUND,
            code: "feedback_not_found",
            message: format!("feedback for turn {turn} was not found"),
            request_id: None,
            decision_id: None,
        })?;
    Ok(Json(json!({
        "turn": turn,
        "signals": signals
    })))
}

fn normalize_signal(kind: &str, strength: Option<f64>) -> Result<FeedbackSignal, GatewayError> {
    let default = match kind {
        "accepted" | "rejected" => 1.0,
        "endorsed" | "disputed" => 0.8,
        "reattempted" => 0.5,
        "abandoned" | "advanced" => 0.3,
        "task_succeeded" | "task_failed" => 0.9,
        _ => {
            return Err(GatewayError::bad_request(
                "invalid_feedback_kind",
                format!("unsupported feedback signal kind: {kind}"),
            ));
        }
    };
    let strength = strength.unwrap_or(default);
    if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
        return Err(GatewayError::bad_request(
            "invalid_feedback_strength",
            "feedback strength must be in [0, 1]",
        ));
    }
    Ok(FeedbackSignal {
        kind: kind.to_owned(),
        strength,
    })
}

async fn ingest_feedback(
    state: &AppState,
    tenant_key: &str,
    turn: &str,
    signals: Vec<FeedbackSignal>,
    retention_days: u16,
) -> Result<(), GatewayError> {
    state
        .metrics
        .feedback_signals
        .fetch_add(signals.len() as u64, Ordering::Relaxed);
    let merged = if let Some(shared) = &state.shared_state {
        shared
            .upsert_feedback(
                tenant_key,
                turn,
                &signals,
                u64::from(retention_days).saturating_mul(24 * 60 * 60),
            )
            .await
            .map_err(state_backend_unavailable)?
    } else {
        signals.clone()
    };
    let merged = state.feedback.upsert(tenant_key, turn, merged).await;
    let mut records = state.records.records.write().await;
    for record in records.iter_mut().filter(|record| {
        record.tenant_key == tenant_key && record.trace_turn.as_deref() == Some(turn)
    }) {
        record.outcome_signals.clone_from(&merged);
    }
    Ok(())
}

impl FeedbackStore {
    async fn open(
        path: Option<PathBuf>,
        queue_capacity: usize,
        max_bytes: u64,
    ) -> Result<Self, BoxError> {
        let mut values = BTreeMap::<String, BTreeMap<String, FeedbackSignal>>::new();
        if let Some(path) = &path
            && let Ok(contents) = tokio::fs::read_to_string(path).await
        {
            for line in contents.lines() {
                if let Ok(event) = serde_json::from_str::<FeedbackEvent>(line) {
                    let tenant = if event.tenant_key.is_empty() {
                        tenant_key("local")
                    } else {
                        event.tenant_key.clone()
                    };
                    let by_kind = values
                        .entry(feedback_scope_key(&tenant, &event.turn))
                        .or_default();
                    for signal in event.signals {
                        by_kind.insert(signal.kind.clone(), signal);
                    }
                }
            }
        }
        let dropped = Arc::new(AtomicU64::new(0));
        let write_errors = Arc::new(AtomicU64::new(0));
        let writer = path.map(|path| {
            let (sender, receiver) = mpsc::channel(queue_capacity);
            tokio::spawn(feedback_writer(
                path,
                max_bytes,
                receiver,
                Arc::clone(&write_errors),
            ));
            sender
        });
        Ok(Self {
            values: Arc::new(RwLock::new(values)),
            writer,
            dropped,
            write_errors,
        })
    }

    async fn upsert(
        &self,
        tenant_key: &str,
        turn: &str,
        signals: Vec<FeedbackSignal>,
    ) -> Vec<FeedbackSignal> {
        let event = FeedbackEvent {
            tenant_key: tenant_key.to_owned(),
            turn: turn.to_owned(),
            signals,
        };
        let merged = {
            let mut feedback = self.values.write().await;
            let by_kind = feedback
                .entry(feedback_scope_key(tenant_key, turn))
                .or_default();
            for signal in &event.signals {
                by_kind.insert(signal.kind.clone(), signal.clone());
            }
            by_kind.values().cloned().collect()
        };
        if let Some(writer) = &self.writer
            && writer.try_send(FeedbackCommand::Append(event)).is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        merged
    }

    async fn delete_matching(
        &self,
        predicate: impl Fn(&str, &str) -> bool,
    ) -> Result<usize, GatewayError> {
        let (deleted, events) = {
            let mut values = self.values.write().await;
            let keys = values
                .keys()
                .filter_map(|key| {
                    let (tenant, turn) = key.split_once('\0')?;
                    predicate(tenant, turn).then(|| key.clone())
                })
                .collect::<Vec<_>>();
            for key in &keys {
                values.remove(key);
            }
            let events = values
                .iter()
                .filter_map(|(key, signals)| {
                    let (tenant_key, turn) = key.split_once('\0')?;
                    Some(FeedbackEvent {
                        tenant_key: tenant_key.to_owned(),
                        turn: turn.to_owned(),
                        signals: signals.values().cloned().collect(),
                    })
                })
                .collect::<Vec<_>>();
            (keys.len(), events)
        };
        if deleted == 0 {
            return Ok(0);
        }
        if let Some(writer) = &self.writer {
            let (completed, receiver) = oneshot::channel();
            writer
                .send(FeedbackCommand::Rewrite { events, completed })
                .await
                .map_err(GatewayError::internal)?;
            if !receiver.await.map_err(GatewayError::internal)? {
                return Err(GatewayError::internal("feedback rewrite failed"));
            }
        }
        Ok(deleted)
    }
}

async fn ingest_piggyback(
    state: &AppState,
    governance: &RequestGovernance,
    signals: &[SignalContract],
) -> Result<(), GatewayError> {
    if governance.policy.recording == RecordingMode::None {
        return Ok(());
    }
    for signal in signals {
        let Some(turn) = signal.turn.as_deref() else {
            return Err(GatewayError::bad_request(
                "invalid_feedback",
                "piggyback signal turn is required",
            ));
        };
        let Some(kind) = signal.kind.as_deref() else {
            return Err(GatewayError::bad_request(
                "invalid_feedback",
                "piggyback signal kind is required",
            ));
        };
        let normalized = normalize_signal(
            kind,
            signal
                .strength
                .as_ref()
                .and_then(serde_json::Number::as_f64),
        )?;
        ingest_feedback(
            state,
            &governance.tenant_key,
            turn,
            vec![normalized],
            governance.policy.retention_days,
        )
        .await?;
    }
    Ok(())
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Response, GatewayError> {
    let state = state.with_active_control();
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let candidate_request_id = next_request_id();
    let (tenant_key, tenant_compatibility) = resolve_tenant(&state, &headers)
        .map_err(|error| error.with_request_id(&candidate_request_id))?;
    let request_id = resolve_request_id(
        &state,
        &headers,
        &tenant_key,
        &request,
        candidate_request_id.clone(),
    )
    .await
    .map_err(|error| error.with_request_id(&candidate_request_id))?;
    let result = Box::pin(chat_completions_inner(
        state.clone(),
        request,
        tenant_key,
        tenant_compatibility,
        request_id.clone(),
    ))
    .await
    .map_err(|error| error.with_request_id(&request_id));
    if result.is_ok() {
        state.metrics.successes.fetch_add(1, Ordering::Relaxed);
    } else {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
    }
    result
}

async fn resolve_request_id(
    state: &AppState,
    headers: &HeaderMap,
    tenant_key: &str,
    request: &Value,
    candidate_request_id: String,
) -> Result<String, GatewayError> {
    let Some(key) = headers.get(HeaderName::from_static("idempotency-key")) else {
        return Ok(candidate_request_id);
    };
    let key = key.to_str().map_err(|_| {
        GatewayError::bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must be visible ASCII",
        )
    })?;
    if key.is_empty() || key.len() > 256 || !key.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(GatewayError::bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must contain 1..=256 visible ASCII bytes",
        ));
    }
    let request_hash = canonical_request_hash(request)?;
    let key_hash = idempotency_key_hash(tenant_key, key);
    let claim = state
        .idempotency
        .claim(
            tenant_key,
            &key_hash,
            &request_hash,
            &candidate_request_id,
            state.idempotency_ttl_seconds,
        )
        .await
        .map_err(state_backend_unavailable)?;
    match claim {
        IdempotencyClaim::Created(request_id) | IdempotencyClaim::Reused(request_id) => {
            Ok(request_id)
        }
        IdempotencyClaim::Conflict => Err(GatewayError {
            status: StatusCode::CONFLICT,
            code: "idempotency_conflict",
            message: "Idempotency-Key was already used with a different request".to_owned(),
            request_id: None,
            decision_id: None,
        }),
    }
}

fn idempotency_key_hash(tenant_key: &str, key: &str) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(format!("{tenant_key}\0{key}").as_bytes())
    )
}

fn canonical_request_hash(request: &Value) -> Result<String, GatewayError> {
    let encoded = serde_json::to_vec(request).map_err(GatewayError::internal)?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

async fn adapter_chat_completions(
    State(state): State<AppState>,
    Path(harness): Path<String>,
    headers: HeaderMap,
    Json(mut request): Json<Value>,
) -> Result<Response, GatewayError> {
    adapt_agent_request(&harness, &headers, &mut request).map_err(|error| {
        GatewayError::bad_request("invalid_agent_adapter", error)
            .with_request_id(&next_request_id())
    })?;
    Box::pin(chat_completions(State(state), headers, Json(request))).await
}

fn internal_chat_capabilities() -> TransportCapabilities {
    TransportCapabilities {
        api: WireApi::OpenAiChat,
        developer_role: true,
        tools: true,
        images: true,
        structured_output: true,
        reasoning: true,
    }
}

async fn openai_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Response, GatewayError> {
    let normalized = from_openai_responses(&request)
        .map_err(|error| GatewayError::bad_request("invalid_responses_request", error))?;
    let streaming = normalized.stream;
    let (chat, loss) = to_openai_chat(
        &normalized,
        &internal_chat_capabilities(),
        LossPolicy::Reject,
    )
    .map_err(|error| GatewayError::bad_request("protocol_semantic_loss", error))?;
    debug_assert!(loss.losses.is_empty());
    let response = Box::pin(chat_completions(State(state), headers, Json(chat))).await?;
    if streaming {
        Ok(translate_chat_stream_response(
            response,
            ProtocolResponse::Responses,
        ))
    } else {
        translate_chat_response(response, ProtocolResponse::Responses).await
    }
}

async fn anthropic_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Response, GatewayError> {
    let normalized = from_anthropic_messages(&request)
        .map_err(|error| GatewayError::bad_request("invalid_anthropic_request", error))?;
    let streaming = normalized.stream;
    let (chat, loss) = to_openai_chat(
        &normalized,
        &internal_chat_capabilities(),
        LossPolicy::Reject,
    )
    .map_err(|error| GatewayError::bad_request("protocol_semantic_loss", error))?;
    debug_assert!(loss.losses.is_empty());
    let response = Box::pin(chat_completions(State(state), headers, Json(chat))).await?;
    if streaming {
        Ok(translate_chat_stream_response(
            response,
            ProtocolResponse::Anthropic,
        ))
    } else {
        translate_chat_response(response, ProtocolResponse::Anthropic).await
    }
}

#[derive(Clone, Copy)]
enum ProtocolResponse {
    Responses,
    Anthropic,
}

type ProtocolBodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>;

struct ProtocolStreamState {
    upstream: ProtocolBodyStream,
    buffer: Vec<u8>,
    protocol: ProtocolResponse,
    translation: ProtocolTranslationState,
    eof: bool,
}

#[derive(Default)]
struct ProtocolTranslationState {
    started: bool,
    response_id: String,
    opened_blocks: BTreeSet<u64>,
}

fn translate_chat_stream_response(response: Response, protocol: ProtocolResponse) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let state = ProtocolStreamState {
        upstream: Box::pin(body.into_data_stream()),
        buffer: Vec::new(),
        protocol,
        translation: ProtocolTranslationState::default(),
        eof: false,
    };
    let translated = stream::unfold(state, |mut state| async move {
        loop {
            if let Some(end) = find_sse_event_end(&state.buffer) {
                let event = state.buffer.drain(..end).collect::<Vec<_>>();
                while state
                    .buffer
                    .first()
                    .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
                {
                    state.buffer.remove(0);
                }
                let bytes =
                    translate_chat_sse_event(&event, state.protocol, &mut state.translation);
                if !bytes.is_empty() {
                    return Some((Ok::<Bytes, axum::Error>(Bytes::from(bytes)), state));
                }
                continue;
            }
            if state.eof {
                return None;
            }
            match state.upstream.next().await {
                Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
                Some(Err(error)) => return Some((Err(error), state)),
                None => {
                    state.eof = true;
                    if !state.buffer.is_empty() {
                        state.buffer.extend_from_slice(b"\n\n");
                    }
                }
            }
        }
    });
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    Response::from_parts(parts, Body::from_stream(translated))
}

fn find_sse_event_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\n\n")
}

fn translate_chat_sse_event(
    event: &[u8],
    protocol: ProtocolResponse,
    state: &mut ProtocolTranslationState,
) -> Vec<u8> {
    let source = String::from_utf8_lossy(event);
    let event_name = source
        .lines()
        .find_map(|line| line.strip_prefix("event:"))
        .map(str::trim);
    let data = source
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if event_name == Some("urouter.decision") {
        return match protocol {
            ProtocolResponse::Responses => named_sse("response.urouter", &data),
            ProtocolResponse::Anthropic => named_sse("urouter.decision", &data),
        };
    }
    if data == "[DONE]" {
        return finish_protocol_stream(protocol, state);
    }
    let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
        return Vec::new();
    };
    if state.response_id.is_empty() {
        chunk
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("urouter-stream")
            .clone_into(&mut state.response_id);
    }
    let mut output = Vec::new();
    if !state.started {
        state.started = true;
        match protocol {
            ProtocolResponse::Responses => append_json_sse(
                &mut output,
                "response.created",
                json!({"type": "response.created", "response": {"id": state.response_id, "status": "in_progress"}}),
            ),
            ProtocolResponse::Anthropic => append_json_sse(
                &mut output,
                "message_start",
                json!({"type": "message_start", "message": {"id": state.response_id, "type": "message", "role": "assistant", "content": [], "stop_reason": null}}),
            ),
        }
    }
    let Some(delta) = chunk.pointer("/choices/0/delta") else {
        return output;
    };
    if let Some(text) = delta.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        append_protocol_delta(&mut output, protocol, state, 0, "text", text, None, None);
    }
    if let Some(reasoning) = delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        append_protocol_delta(
            &mut output,
            protocol,
            state,
            1,
            "reasoning",
            reasoning,
            None,
            None,
        );
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool in tool_calls {
            let index = tool.get("index").and_then(Value::as_u64).unwrap_or(0) + 2;
            let arguments = tool
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            append_protocol_delta(
                &mut output,
                protocol,
                state,
                index,
                "tool",
                arguments,
                tool.get("id").and_then(Value::as_str),
                tool.pointer("/function/name").and_then(Value::as_str),
            );
        }
    }
    output
}

#[allow(clippy::too_many_arguments)]
fn append_protocol_delta(
    output: &mut Vec<u8>,
    protocol: ProtocolResponse,
    state: &mut ProtocolTranslationState,
    index: u64,
    kind: &str,
    delta: &str,
    id: Option<&str>,
    name: Option<&str>,
) {
    match protocol {
        ProtocolResponse::Responses => match kind {
            "text" => append_json_sse(
                output,
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": delta}),
            ),
            "reasoning" => append_json_sse(
                output,
                "response.reasoning_text.delta",
                json!({"type": "response.reasoning_text.delta", "output_index": 0, "delta": delta}),
            ),
            "tool" => {
                if state.opened_blocks.insert(index) {
                    append_json_sse(
                        output,
                        "response.output_item.added",
                        json!({"type": "response.output_item.added", "output_index": index - 1, "item": {"type": "function_call", "id": id.unwrap_or("call"), "name": name.unwrap_or("tool"), "arguments": ""}}),
                    );
                }
                if !delta.is_empty() {
                    append_json_sse(
                        output,
                        "response.function_call_arguments.delta",
                        json!({"type": "response.function_call_arguments.delta", "output_index": index - 1, "delta": delta}),
                    );
                }
            }
            _ => {}
        },
        ProtocolResponse::Anthropic => {
            if state.opened_blocks.insert(index) {
                let content = match kind {
                    "tool" => {
                        json!({"type": "tool_use", "id": id.unwrap_or("call"), "name": name.unwrap_or("tool"), "input": {}})
                    }
                    "reasoning" => json!({"type": "thinking", "thinking": ""}),
                    _ => json!({"type": "text", "text": ""}),
                };
                append_json_sse(
                    output,
                    "content_block_start",
                    json!({"type": "content_block_start", "index": index, "content_block": content}),
                );
            }
            if !delta.is_empty() {
                let value = match kind {
                    "tool" => json!({"type": "input_json_delta", "partial_json": delta}),
                    "reasoning" => json!({"type": "thinking_delta", "thinking": delta}),
                    _ => json!({"type": "text_delta", "text": delta}),
                };
                append_json_sse(
                    output,
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index, "delta": value}),
                );
            }
        }
    }
}

fn finish_protocol_stream(
    protocol: ProtocolResponse,
    state: &mut ProtocolTranslationState,
) -> Vec<u8> {
    let mut output = Vec::new();
    match protocol {
        ProtocolResponse::Responses => append_json_sse(
            &mut output,
            "response.completed",
            json!({"type": "response.completed", "response": {"id": state.response_id, "status": "completed"}}),
        ),
        ProtocolResponse::Anthropic => {
            for index in &state.opened_blocks {
                append_json_sse(
                    &mut output,
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                );
            }
            append_json_sse(
                &mut output,
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}}),
            );
            append_json_sse(&mut output, "message_stop", json!({"type": "message_stop"}));
        }
    }
    output
}

#[allow(clippy::needless_pass_by_value)]
fn append_json_sse(output: &mut Vec<u8>, event: &str, value: Value) {
    let data = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
    output.extend_from_slice(&named_sse(event, &data));
}

fn named_sse(event: &str, data: &str) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

async fn translate_chat_response(
    response: Response,
    protocol: ProtocolResponse,
) -> Result<Response, GatewayError> {
    if !response.status().is_success() {
        return Ok(response);
    }
    let (mut parts, body) = response.into_parts();
    let bytes = to_bytes(body, 16 * 1_024 * 1_024)
        .await
        .map_err(GatewayError::internal)?;
    let chat: Value = serde_json::from_slice(&bytes).map_err(GatewayError::internal)?;
    let translated = match protocol {
        ProtocolResponse::Responses => chat_to_responses(&chat),
        ProtocolResponse::Anthropic => chat_to_anthropic(&chat),
    };
    let body = serde_json::to_vec(&translated).map_err(GatewayError::internal)?;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(Response::from_parts(parts, Body::from(body)))
}

fn chat_to_responses(chat: &Value) -> Value {
    let message = chat
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_default();
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        content.push(json!({"type": "output_text", "text": text, "annotations": []}));
    }
    if let Some(reasoning) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
    {
        content.push(
            json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": reasoning}]}),
        );
    }
    let mut output = vec![json!({
        "type": "message",
        "id": chat.get("id").cloned().unwrap_or(Value::Null),
        "status": "completed",
        "role": "assistant",
        "content": content
    })];
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        output.extend(calls.iter().map(|call| {
            json!({
                "type": "function_call",
                "call_id": call.get("id").cloned().unwrap_or(Value::Null),
                "name": call.pointer("/function/name").cloned().unwrap_or(Value::Null),
                "arguments": call.pointer("/function/arguments").cloned().unwrap_or(Value::Null)
            })
        }));
    }
    json!({
        "id": chat.get("id").cloned().unwrap_or(Value::Null),
        "object": "response",
        "status": "completed",
        "model": chat.get("model").cloned().unwrap_or(Value::Null),
        "output": output,
        "usage": {
            "input_tokens": chat.pointer("/usage/prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": chat.pointer("/usage/completion_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": chat.pointer("/usage/total_tokens").cloned().unwrap_or(json!(0))
        },
        "urouter": chat.get("urouter").cloned().unwrap_or(Value::Null)
    })
}

fn chat_to_anthropic(chat: &Value) -> Value {
    let message = chat
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_default();
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        content.extend(calls.iter().map(|call| {
            let input = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .unwrap_or_else(|| json!({}));
            json!({
                "type": "tool_use",
                "id": call.get("id").cloned().unwrap_or(Value::Null),
                "name": call.pointer("/function/name").cloned().unwrap_or(Value::Null),
                "input": input
            })
        }));
    }
    let finish = chat
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    json!({
        "id": chat.get("id").cloned().unwrap_or(Value::Null),
        "type": "message",
        "role": "assistant",
        "model": chat.get("model").cloned().unwrap_or(Value::Null),
        "content": content,
        "stop_reason": if finish == "tool_calls" { "tool_use" } else { "end_turn" },
        "usage": {
            "input_tokens": chat.pointer("/usage/prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": chat.pointer("/usage/completion_tokens").cloned().unwrap_or(json!(0))
        },
        "urouter": chat.get("urouter").cloned().unwrap_or(Value::Null)
    })
}

async fn chat_completions_inner(
    state: AppState,
    request: Value,
    tenant_key: String,
    tenant_compatibility: bool,
    request_id: String,
) -> Result<Response, GatewayError> {
    let feature_frame = FeatureFrame::from_openai_chat(&request);
    let route_revision = state.route.revision();
    let mut decision = state.route.decide(&state.catalog, &request)?;
    let artifact = apply_artifact_policy(
        &state,
        &request,
        &tenant_key,
        &artifact_task_key(&decision, &request_id),
        &mut decision,
    )?;
    let exploration = apply_controlled_exploration(
        &state,
        &request,
        &tenant_key,
        &artifact_task_key(&decision, &request_id),
        tenant_compatibility,
        &mut decision,
    )?;
    decision.compatibility_mode |= tenant_compatibility;
    validate_decision_scope(&decision)?;
    let admission = admit_request(&state, tenant_key, &request_id, &decision, &request).await?;
    let governance = admission.governance;
    let quota = admission.quota;
    let quota_input_tokens = admission.quota_input_tokens;
    let budget = admission.budget;
    let budget_input_tokens = admission.budget_input_tokens;
    let budget_output_tokens = admission.budget_output_tokens;
    observe_compatibility(&state, decision.compatibility_mode);
    apply_task_binding(&state, &governance, &mut decision).await?;
    ingest_piggyback(&state, &governance, &decision.signals).await?;
    let messages_hash = messages_hash(&request)?;
    let vector_ref = persist_semantic_vector(&state, &request, &governance).await?;
    let record_seed = RecordSeed {
        request_id,
        messages_hash,
        features: feature_frame,
        route_revision,
        artifact,
        exploration,
        vector_ref,
    };
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let started = Instant::now();
    let execution = match execute_routed_upstream(
        &state,
        &decision,
        &request,
        &record_seed.request_id,
        &governance.tenant_key,
    )
    .await
    {
        Ok(execution) => execution,
        Err(failure) => {
            return Err(record_routed_failure(
                &state,
                failure,
                FailedRequestContext {
                    decision,
                    governance,
                    record: record_seed,
                    budget,
                    stream,
                    started,
                },
            )
            .await?);
        }
    };
    if execution.tier != decision.tier {
        decision.tier.clone_from(&execution.tier);
        decision.model = execution.model.id.clone();
        "fallback_degraded".clone_into(&mut decision.reason);
    }
    let decision_id = next_decision_id();
    let headers = decision_headers(&record_seed.request_id, &decision_id, &decision)?;
    let response_context = ResponseContext {
        decision_id,
        decision,
        governance,
        record: record_seed,
        headers,
        quota,
        quota_input_tokens,
        budget: BudgetAccounting {
            lease: budget,
            input_tokens: budget_input_tokens,
            output_tokens: budget_output_tokens,
        },
    };
    if stream {
        Ok(stream_response(state, execution, response_context))
    } else {
        non_stream_response(state, execution, response_context).await
    }
}

async fn record_routed_failure(
    state: &AppState,
    mut failure: RoutedFailure,
    mut context: FailedRequestContext,
) -> Result<GatewayError, GatewayError> {
    let elapsed_ms = context.started.elapsed().as_millis();
    state
        .metrics
        .request_duration_ms
        .observe(duration_metric_value(elapsed_ms));
    state
        .metrics
        .upstream_duration_ms
        .observe(duration_metric_value(elapsed_ms));
    state
        .metrics
        .fallback_depth
        .observe(u64::from(failure.fallback_depth));
    if failure.attempts.is_empty() {
        let _ = context.budget.release().await;
    }
    let decision_id = next_decision_id();
    let error_kind = failure
        .attempts
        .last()
        .and_then(|attempt| attempt.error_kind);
    let upstream_status = failure
        .attempts
        .last()
        .and_then(|attempt| attempt.status)
        .unwrap_or(0);
    context.decision.tier.clone_from(&failure.tier);
    context.decision.model = failure.model.id.clone();
    if failure.fallback_depth > 0 {
        "fallback_degraded".clone_into(&mut context.decision.reason);
    }
    let deployment = failure
        .attempts
        .last()
        .map(|attempt| attempt.deployment.clone())
        .unwrap_or_default();
    let record = build_record(
        &state.catalog,
        decision_id.clone(),
        context.decision,
        &failure.model,
        &context.governance,
        &context.record,
        ExecutionRecord {
            ok: false,
            stream: context.stream,
            upstream_status,
            upstream_latency_ms: elapsed_ms,
            usage: None,
            cost: None,
            usage_unavailable: true,
            attempts: failure.attempts,
            error_kind,
            deployment,
            fallback_depth: failure.fallback_depth,
            runtime_filter_trace: failure.runtime_filter_trace,
        },
    )?;
    store_record(state, record).await?;
    failure.error.decision_id = Some(decision_id);
    Ok(failure.error)
}

async fn admit_request(
    state: &AppState,
    tenant_key: String,
    request_id: &str,
    decision: &RouteDecision,
    request: &Value,
) -> Result<RequestAdmission, GatewayError> {
    let governance = request_governance(state, tenant_key, decision).await?;
    let (budget, budget_input_tokens, budget_output_tokens) =
        acquire_request_budget(state, &governance.tenant_key, request_id, decision, request)
            .await?;
    let (quota, quota_input_tokens) =
        match acquire_request_quota(state, &governance.tenant_key, request).await {
            Ok(quota) => quota,
            Err(error) => {
                let _ = budget.release().await;
                return Err(error);
            }
        };
    Ok(RequestAdmission {
        governance,
        quota,
        quota_input_tokens,
        budget,
        budget_input_tokens,
        budget_output_tokens,
    })
}

fn observe_compatibility(state: &AppState, compatibility_mode: bool) {
    if compatibility_mode {
        state
            .metrics
            .compatibility_requests
            .fetch_add(1, Ordering::Relaxed);
    }
}

async fn acquire_tenant_quota(
    state: &AppState,
    tenant_key: &str,
    estimated_tokens: u64,
) -> Result<QuotaLease, GatewayError> {
    match QuotaLease::acquire(Arc::clone(&state.quota), tenant_key, estimated_tokens)
        .await
        .map_err(quota_repository_error)?
    {
        QuotaAdmission::Granted(lease) => Ok(lease),
        QuotaAdmission::Rejected(reason) => {
            state
                .metrics
                .quota_rejections
                .fetch_add(1, Ordering::Relaxed);
            let (code, message) = match reason {
                QuotaRejection::InFlight => (
                    "tenant_concurrency_exhausted",
                    "the tenant has reached its maximum in-flight request limit",
                ),
                QuotaRejection::RequestsPerMinute => (
                    "tenant_rate_limit_exhausted",
                    "the tenant has reached its requests-per-minute limit",
                ),
                QuotaRejection::TokensPerMinute => (
                    "tenant_token_limit_exhausted",
                    "the tenant has reached its tokens-per-minute limit",
                ),
            };
            Err(GatewayError {
                status: StatusCode::TOO_MANY_REQUESTS,
                code,
                message: message.to_owned(),
                request_id: None,
                decision_id: None,
            })
        }
    }
}

async fn acquire_request_quota(
    state: &AppState,
    tenant_key: &str,
    request: &Value,
) -> Result<(QuotaLease, u64), GatewayError> {
    let (input_tokens, reserved_tokens) = estimate_quota_tokens(
        request,
        state.quota_default_max_output_tokens,
        state.retry_policy.max_retries,
    );
    let lease = acquire_tenant_quota(state, tenant_key, reserved_tokens).await?;
    Ok((lease, input_tokens))
}

fn estimate_quota_tokens(
    request: &Value,
    default_max_output_tokens: u64,
    max_retries: u8,
) -> (u64, u64) {
    let (input_tokens, output_tokens) = estimate_request_tokens(request, default_max_output_tokens);
    let attempts = u64::from(max_retries).saturating_add(1);
    (
        input_tokens,
        input_tokens
            .saturating_mul(attempts)
            .saturating_add(output_tokens),
    )
}

fn estimate_request_tokens(request: &Value, default_max_output_tokens: u64) -> (u64, u64) {
    let serialized_bytes = serde_json::to_vec(request)
        .ok()
        .and_then(|bytes| u64::try_from(bytes.len()).ok())
        .unwrap_or(1);
    let input_tokens = serialized_bytes.div_ceil(4).max(1);
    let output_tokens = request
        .get("max_tokens")
        .or_else(|| request.get("max_completion_tokens"))
        .or_else(|| request.get("max_output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(default_max_output_tokens);
    (input_tokens, output_tokens)
}

async fn acquire_request_budget(
    state: &AppState,
    tenant_key: &str,
    request_id: &str,
    decision: &RouteDecision,
    request: &Value,
) -> Result<(BudgetLease, u64, u64), GatewayError> {
    let (input_tokens, output_tokens) =
        estimate_request_tokens(request, state.quota_default_max_output_tokens);
    let estimate = estimate_request_cost(state, decision, tenant_key, input_tokens, output_tokens)?;
    match BudgetLease::acquire(Arc::clone(&state.budget), tenant_key, request_id, estimate)
        .await
        .map_err(budget_repository_error)?
    {
        BudgetAdmission::Granted(lease) => Ok((lease, input_tokens, output_tokens)),
        BudgetAdmission::Rejected => {
            state
                .metrics
                .budget_rejections
                .fetch_add(1, Ordering::Relaxed);
            Err(GatewayError {
                status: StatusCode::PAYMENT_REQUIRED,
                code: "tenant_budget_exhausted",
                message: "the tenant has reached its hard budget limit".to_owned(),
                request_id: None,
                decision_id: None,
            })
        }
    }
}

fn estimate_request_cost(
    state: &AppState,
    decision: &RouteDecision,
    tenant_key_value: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Result<u64, GatewayError> {
    let tiers = execution_tiers(state, decision)?;
    let attempts_per_tier = u64::from(state.retry_policy.max_retries).saturating_add(1);
    let attempts = u64::try_from(tiers.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(attempts_per_tier);
    let usage = Usage {
        input: input_tokens.saturating_mul(attempts),
        output: output_tokens.saturating_mul(attempts),
        ..Usage::default()
    };
    let mut maximum = 0_u64;
    for model in tiers
        .iter()
        .flat_map(|tier| tier.deployments.iter())
        .filter(|deployment| {
            deployment_filter_reasons(state, deployment, decision, tenant_key_value)
                .iter()
                .all(|reason| {
                    matches!(
                        reason.as_str(),
                        "credential_unavailable" | "endpoint_unavailable"
                    )
                })
        })
        .filter_map(|deployment| state.catalog.model(&deployment.model))
    {
        let cost = calculate_actual_cost(&model.cost, usage)
            .map_err(|error| GatewayError::internal(error.to_string()))?;
        maximum = maximum.max(money_to_u64(cost.total));
    }
    Ok(maximum)
}

fn money_to_u64(value: urouter_types::MoneyNanoUsd) -> u64 {
    u64::try_from(value.as_nano_usd()).unwrap_or(u64::MAX)
}

fn duration_metric_value(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn observe_execution_metrics(
    metrics: &GatewayMetrics,
    request_duration_ms: u128,
    upstream_duration_ms: u128,
    fallback_depth: u8,
    cost: Option<&CostBreakdown>,
    trace_id: Option<&str>,
) {
    metrics
        .request_duration_ms
        .observe_with_exemplar(duration_metric_value(request_duration_ms), trace_id);
    metrics
        .upstream_duration_ms
        .observe(duration_metric_value(upstream_duration_ms));
    metrics.fallback_depth.observe(u64::from(fallback_depth));
    if let Some(cost) = cost {
        metrics.cost_nano_usd.observe(money_to_u64(cost.total));
    }
}

fn usage_token_count(usage: Usage, estimated_input_tokens: u64, attempts: usize) -> u64 {
    let actual = usage
        .total_input()
        .unwrap_or(u64::MAX)
        .saturating_add(usage.output);
    let failed_attempts = u64::try_from(attempts.saturating_sub(1)).unwrap_or(u64::MAX);
    actual.saturating_add(estimated_input_tokens.saturating_mul(failed_attempts))
}

async fn settle_quota_usage(
    quota: &QuotaLease,
    usage: Option<Usage>,
    estimated_input_tokens: u64,
    attempts: usize,
) {
    if let Some(actual_usage) = usage {
        let actual_tokens = usage_token_count(actual_usage, estimated_input_tokens, attempts);
        let _ = quota.settle(actual_tokens).await;
    }
}

async fn settle_budget_usage(
    state: &AppState,
    budget: &BudgetLease,
    final_cost: Option<&CostBreakdown>,
    attempts: &[AttemptRecord],
    input_tokens: u64,
    output_tokens: u64,
) {
    let Some(final_cost) = final_cost else {
        return;
    };
    let mut actual = money_to_u64(final_cost.total);
    let failed_usage = Usage {
        input: input_tokens,
        output: output_tokens,
        ..Usage::default()
    };
    for model in attempts
        .iter()
        .filter(|attempt| attempt.error_kind.is_some())
        .filter_map(|attempt| attempt.model.as_ref())
        .filter_map(|model| state.catalog.model(model))
    {
        if let Ok(cost) = calculate_actual_cost(&model.cost, failed_usage) {
            actual = actual.saturating_add(money_to_u64(cost.total));
        } else {
            return;
        }
    }
    let _ = budget.settle(actual).await;
}

fn validate_scope_value(field: &str, value: &str) -> Result<(), GatewayError> {
    if value.trim().is_empty() || value.len() > 256 {
        return Err(GatewayError::bad_request(
            "invalid_agent_scope",
            format!("{field} must contain 1..=256 bytes"),
        ));
    }
    Ok(())
}

fn resolve_tenant(state: &AppState, headers: &HeaderMap) -> Result<(String, bool), GatewayError> {
    let tenant = headers
        .get(HeaderName::from_static("x-urouter-tenant-id"))
        .map(|value| value.to_str())
        .transpose()
        .map_err(|_| GatewayError::bad_request("invalid_tenant", "tenant header is not UTF-8"))?;
    if (state.require_tenant_header || state.management_auth.enabled()) && tenant.is_none() {
        return Err(GatewayError {
            status: StatusCode::UNAUTHORIZED,
            code: "tenant_required",
            message: "x-urouter-tenant-id must be injected by the authentication layer".to_owned(),
            request_id: None,
            decision_id: None,
        });
    }
    let tenant = tenant.unwrap_or("local");
    validate_scope_value("x-urouter-tenant-id", tenant)?;
    Ok((
        tenant_key(tenant),
        tenant == "local" && !state.require_tenant_header,
    ))
}

fn resolve_tenant_key(state: &AppState, headers: &HeaderMap) -> Result<String, GatewayError> {
    resolve_tenant(state, headers).map(|(key, _)| key)
}

async fn request_governance(
    state: &AppState,
    tenant_key: String,
    decision: &RouteDecision,
) -> Result<RequestGovernance, GatewayError> {
    let task_key = decision
        .task_id
        .as_deref()
        .map(|task_id| task_scope_key(&tenant_key, task_id));
    let (tenant_generation, task_generation) = state
        .bindings
        .scope_generation(&tenant_key, task_key.as_deref())
        .await
        .map_err(state_backend_unavailable)?;
    Ok(RequestGovernance {
        tenant_key,
        policy: decision.data_policy.clone().unwrap_or_default(),
        compatibility_mode: decision.compatibility_mode,
        tenant_generation,
        task_generation,
    })
}

async fn records_for_tenant(
    state: &AppState,
    tenant_key: &str,
) -> Result<Vec<DecisionRecord>, GatewayError> {
    let mut records = state
        .record_repository
        .list(tenant_key)
        .await
        .map_err(record_repository_error)?;
    if let Some(shared) = &state.shared_state {
        let turns = records
            .iter()
            .filter_map(|record| record.trace_turn.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let feedback = shared
            .feedback_many(tenant_key, &turns)
            .await
            .map_err(state_backend_unavailable)?;
        for record in &mut records {
            if let Some(turn) = &record.trace_turn
                && let Some(signals) = feedback.get(turn)
            {
                record.outcome_signals.clone_from(signals);
            }
        }
    }
    Ok(records)
}

async fn record_for_id(
    state: &AppState,
    tenant_key: &str,
    decision_id: &str,
) -> Result<Option<DecisionRecord>, GatewayError> {
    let Some(mut record) = state
        .record_repository
        .get(tenant_key, decision_id)
        .await
        .map_err(record_repository_error)?
    else {
        return Ok(None);
    };
    if let Some(shared) = &state.shared_state
        && let Some(turn) = &record.trace_turn
        && let Some(signals) = shared
            .feedback(tenant_key, turn)
            .await
            .map_err(state_backend_unavailable)?
    {
        record.outcome_signals = signals;
    }
    Ok(Some(record))
}

async fn feedback_for_turn(
    state: &AppState,
    tenant_key: &str,
    turn: &str,
) -> Result<Option<Vec<FeedbackSignal>>, GatewayError> {
    if let Some(shared) = &state.shared_state {
        return shared
            .feedback(tenant_key, turn)
            .await
            .map_err(state_backend_unavailable);
    }
    Ok(state
        .feedback
        .values
        .read()
        .await
        .get(&feedback_scope_key(tenant_key, turn))
        .map(|signals| signals.values().cloned().collect()))
}

async fn delete_feedback_turns(
    state: &AppState,
    tenant_key: &str,
    turns: &[String],
) -> Result<usize, GatewayError> {
    let shared_deleted = if let Some(shared) = &state.shared_state {
        shared
            .delete_feedback(tenant_key, turns)
            .await
            .map_err(state_backend_unavailable)?
    } else {
        0
    };
    let local_deleted = state
        .feedback
        .delete_matching(|tenant, turn| {
            tenant == tenant_key && turns.iter().any(|candidate| candidate == turn)
        })
        .await?;
    Ok(shared_deleted.max(local_deleted))
}

fn validate_decision_scope(decision: &RouteDecision) -> Result<(), GatewayError> {
    for (field, value) in [
        ("task.id", decision.task_id.as_deref()),
        ("agent.harness", decision.agent_harness.as_deref()),
        ("trace.conversation", decision.conversation_id.as_deref()),
        ("trace.branch", decision.branch_id.as_deref()),
        ("trace.turn", decision.trace_turn.as_deref()),
    ] {
        if let Some(value) = value {
            validate_scope_value(field, value)?;
        }
    }
    Ok(())
}

fn tenant_key(tenant_id: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(tenant_id.as_bytes()))
}

fn feedback_scope_key(tenant_key: &str, turn: &str) -> String {
    format!("{tenant_key}\0{turn}")
}

fn task_scope_key(tenant_key: &str, task_id: &str) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(format!("{tenant_key}\0{task_id}").as_bytes())
    )
}

fn session_scope_key(tenant_key: &str, conversation_id: &str, branch_id: &str) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(format!("{tenant_key}\0session\0{conversation_id}\0{branch_id}").as_bytes())
    )
}

fn scope_component_key(kind: &str, value: &str) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(format!("{kind}\0{value}").as_bytes())
    )
}

fn wire_api_name(api: &WireApi) -> String {
    match api {
        WireApi::OpenAiChat => "open_ai_chat".to_owned(),
        WireApi::OpenAiResponses => "open_ai_responses".to_owned(),
        WireApi::AnthropicMessages => "anthropic_messages".to_owned(),
        WireApi::Custom(name) => format!("custom:{name}"),
        _ => "unknown".to_owned(),
    }
}

fn decision_binding_key(tenant_key: &str, decision: &RouteDecision) -> Option<String> {
    match (
        decision.conversation_id.as_deref(),
        decision.branch_id.as_deref(),
    ) {
        (Some(conversation), Some(branch)) => {
            Some(session_scope_key(tenant_key, conversation, branch))
        }
        _ => decision
            .task_id
            .as_deref()
            .map(|task| task_scope_key(tenant_key, task)),
    }
}

fn management_target_key(tenant_key: &str, kind: &str, target: &str) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(format!("{tenant_key}\0{kind}\0{target}").as_bytes())
    )
}

async fn authorize_management(
    state: &AppState,
    headers: &HeaderMap,
    tenant_key: &str,
    required: ManagementRole,
    action: &str,
    target_key: Option<String>,
) -> Result<(), GatewayError> {
    state
        .management_auth
        .authorize(headers, tenant_key, required, action, target_key)
        .await
        .map(|_| ())
        .map_err(GatewayError::from)
}

async fn apply_task_binding(
    state: &AppState,
    governance: &RequestGovernance,
    decision: &mut RouteDecision,
) -> Result<(), GatewayError> {
    apply_task_binding_inner(state, governance, decision, true).await
}

async fn apply_task_binding_inner(
    state: &AppState,
    governance: &RequestGovernance,
    decision: &mut RouteDecision,
    observe: bool,
) -> Result<(), GatewayError> {
    if decision.compatibility_mode
        || decision.call_role == Some(CallRole::Auxiliary)
        || decision.reason == "explicit_model"
    {
        return Ok(());
    }
    let Some(binding_key) = decision_binding_key(&governance.tenant_key, decision) else {
        return Ok(());
    };
    let Some(binding) = state
        .bindings
        .get(&binding_key)
        .await
        .map_err(state_backend_unavailable)?
    else {
        return Ok(());
    };
    let session_scoped = decision.conversation_id.is_some();
    let model = state.catalog.model(&binding.model);
    let identity_matches = model.is_some_and(|model| {
        (binding.provider.is_empty() || binding.provider == model.provider.as_str())
            && (binding.api.is_empty() || binding.api == wire_api_name(&model.api))
    });
    let context_matches = binding.prompt_profile_hash == decision.prompt_profile_hash
        && binding.toolset_hash == decision.toolset_hash;
    if session_scoped && (!identity_matches || !context_matches) {
        if decision.migration_boundary.is_none() {
            return Err(GatewayError::bad_request(
                "unsafe_session_migration",
                "session execution identity changed outside a safe migration boundary",
            ));
        }
        "session_migration".clone_into(&mut decision.reason);
        return Ok(());
    }
    if decision.reason == "preference_pin" {
        if session_scoped {
            "session_migration".clone_into(&mut decision.reason);
        } else {
            "task_migration".clone_into(&mut decision.reason);
        }
        return Ok(());
    }
    match decision.migration_boundary {
        Some(MigrationBoundary::NewTask | MigrationBoundary::AfterCompaction) => {
            if session_scoped {
                "session_migration".clone_into(&mut decision.reason);
            } else {
                "task_migration".clone_into(&mut decision.reason);
            }
            return Ok(());
        }
        Some(MigrationBoundary::ExplicitUserRetry | MigrationBoundary::TerminalProviderFailure) => {
            if let Some(model) = next_eligible_model(state, &binding, decision) {
                *decision = state
                    .route
                    .bind_decision(&state.catalog, decision.clone(), &model)?;
                if session_scoped {
                    "session_migration".clone_into(&mut decision.reason);
                } else {
                    "task_migration".clone_into(&mut decision.reason);
                }
                return Ok(());
            }
        }
        _ => {}
    }
    match state
        .route
        .bind_decision(&state.catalog, decision.clone(), &binding.model)
    {
        Ok(bound) => {
            *decision = bound;
            if session_scoped {
                "session_binding".clone_into(&mut decision.reason);
            }
            if observe {
                state
                    .metrics
                    .bindings_applied
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
        Err(RouteError::NoEligibleTier) if decision.migration_boundary.is_some() => {
            "capability_migration".clone_into(&mut decision.reason);
            Ok(())
        }
        Err(RouteError::NoEligibleTier) => Err(GatewayError::bad_request(
            "unsafe_task_migration",
            "bound model cannot satisfy this call; declare a safe migration boundary",
        )),
        Err(error) => Err(error.into()),
    }
}

fn next_eligible_model(
    state: &AppState,
    binding: &TaskBinding,
    decision: &RouteDecision,
) -> Option<ModelId> {
    let current = state
        .route
        .tiers
        .iter()
        .position(|tier| tier.tier == binding.tier)?;
    state
        .route
        .tiers
        .iter()
        .skip(current.saturating_add(1))
        .map(|tier| tier.model.clone())
        .find(|model| decision.admission.eligible.contains(model))
}

async fn commit_task_binding(
    state: &AppState,
    governance: &RequestGovernance,
    decision: &RouteDecision,
    tier: &str,
    model: &ModelId,
) -> Result<(), GatewayError> {
    if decision.compatibility_mode
        || decision.call_role != Some(CallRole::Primary)
        || decision.reason == "explicit_model"
    {
        return Ok(());
    }
    let Some(task_id) = decision.task_id.as_deref() else {
        return Ok(());
    };
    let Some(binding_key) = decision_binding_key(&governance.tenant_key, decision) else {
        return Ok(());
    };
    let allow_migration = matches!(
        decision.reason.as_str(),
        "task_migration" | "session_migration" | "capability_migration" | "fallback_degraded"
    );
    let model_spec = state
        .catalog
        .model(model)
        .ok_or_else(|| GatewayError::internal("binding model disappeared from catalog"))?;
    let outcome = state
        .bindings
        .put(
            TaskBinding {
                tenant_key: governance.tenant_key.clone(),
                binding_key,
                task_key: task_scope_key(&governance.tenant_key, task_id),
                conversation_key: decision
                    .conversation_id
                    .as_deref()
                    .map(|value| scope_component_key("conversation", value)),
                branch_key: decision
                    .branch_id
                    .as_deref()
                    .map(|value| scope_component_key("branch", value)),
                tier: tier.to_owned(),
                model: model.clone(),
                provider: model_spec.provider.as_str().to_owned(),
                api: wire_api_name(&model_spec.api),
                agent_harness: decision.agent_harness.clone(),
                prompt_profile_hash: decision.prompt_profile_hash.clone(),
                toolset_hash: decision.toolset_hash.clone(),
                bound_at_turn: decision.trace_turn.clone(),
                last_seen_turn: decision.trace_turn.clone(),
                generation: 1,
                tenant_generation: governance.tenant_generation,
                task_generation: governance.task_generation,
            },
            allow_migration,
        )
        .await
        .map_err(state_backend_unavailable)?;
    match outcome {
        BindingWrite::Created => {
            state
                .metrics
                .bindings_created
                .fetch_add(1, Ordering::Relaxed);
        }
        BindingWrite::Migrated => {
            state
                .metrics
                .binding_migrations
                .fetch_add(1, Ordering::Relaxed);
        }
        BindingWrite::Unchanged => {}
        BindingWrite::Conflict => {
            state
                .metrics
                .binding_conflicts
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn messages_hash(request: &Value) -> Result<String, GatewayError> {
    let messages = request
        .get("messages")
        .ok_or_else(|| GatewayError::bad_request("invalid_messages", "messages are required"))?;
    let encoded = serde_json::to_vec(messages).map_err(GatewayError::internal)?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

async fn persist_semantic_vector(
    state: &AppState,
    request: &Value,
    governance: &RequestGovernance,
) -> Result<Option<VectorRef>, GatewayError> {
    let Some(value) = request.pointer("/urouter/semantic_vector") else {
        return Ok(None);
    };
    if !governance.policy.allow_training
        || governance.policy.recording != RecordingMode::MetadataOnly
        || governance.compatibility_mode
    {
        return Ok(None);
    }
    let store = state.vector_store.as_ref().ok_or_else(|| {
        GatewayError::bad_request(
            "vector_store_unavailable",
            "semantic_vector requires a configured vector side-store",
        )
    })?;
    let vector = serde_json::from_value::<Vec<f32>>(value.clone()).map_err(|_| {
        GatewayError::bad_request(
            "invalid_semantic_vector",
            "semantic_vector must be an array of finite numbers",
        )
    })?;
    if vector.is_empty() || vector.len() > 4_096 {
        return Err(GatewayError::bad_request(
            "invalid_semantic_vector",
            "semantic_vector dimensions must be in 1..=4096",
        ));
    }
    if let Ok(reference) = store.append(&vector).await {
        state.metrics.vector_writes.fetch_add(1, Ordering::Relaxed);
        Ok(Some(reference))
    } else {
        state
            .metrics
            .vector_write_errors
            .fetch_add(1, Ordering::Relaxed);
        Err(GatewayError::internal("semantic vector persistence failed"))
    }
}

async fn execute_routed_upstream(
    state: &AppState,
    decision: &RouteDecision,
    request: &Value,
    request_id: &str,
    tenant_key: &str,
) -> Result<UpstreamExecution, RoutedFailure> {
    let started = Instant::now();
    let mut attempts = Vec::new();
    let mut runtime_filter_trace = Vec::new();
    let mut last_error = None;
    let mut last_model = state
        .catalog
        .model(&decision.model)
        .expect("route decision model exists")
        .clone();
    let mut last_tier = decision.tier.clone();
    let mut last_depth = 0_u8;
    let identity = RequestExecutionIdentity {
        request_id,
        tenant_key,
    };
    let mut pending = VecDeque::from([decision.tier.clone()]);
    let mut visited = BTreeSet::new();

    while let Some(tier_name) = pending.pop_front() {
        if !visited.insert(tier_name.clone()) {
            continue;
        }
        let depth = u8::try_from(visited.len().saturating_sub(1)).unwrap_or(u8::MAX);
        if depth > state.max_fallback_depth {
            break;
        }
        let tier = execution_tier(state, decision, &tier_name)
            .map_err(|error| routed_setup_failure(state, decision, error))?;
        if depth > 0 {
            state.metrics.fallbacks.fetch_add(1, Ordering::Relaxed);
        }
        last_depth = depth;
        last_tier.clone_from(&tier.tier);
        match execute_tier(
            state,
            &tier,
            decision,
            request,
            &identity,
            &mut attempts,
            &mut runtime_filter_trace,
        )
        .await
        {
            Ok(success) => {
                return Ok(UpstreamExecution {
                    response: success.response,
                    elapsed_ms: started.elapsed().as_millis(),
                    attempts,
                    lease: success.lease,
                    tier: tier.tier.clone(),
                    model: success.model,
                    fallback_depth: depth,
                });
            }
            Err(exhausted) => {
                last_model = exhausted.model;
                if exhausted
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "upstream_bad_request")
                {
                    return Err(RoutedFailure {
                        error: exhausted.error.expect("bad request error exists"),
                        attempts,
                        tier: tier.tier.clone(),
                        model: last_model,
                        fallback_depth: depth,
                        runtime_filter_trace,
                    });
                }
                let cause = fallback_cause(exhausted.error.as_ref());
                if decision.reason != "explicit_model" {
                    for fallback in typed_fallbacks(state, &tier.tier, cause) {
                        if !visited.contains(&fallback) {
                            pending.push_back(fallback);
                        }
                    }
                }
                last_error = exhausted.error;
            }
        }
    }

    Err(RoutedFailure {
        error: last_error.unwrap_or_else(|| GatewayError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "capacity_exhausted",
            message: "all configured deployments and fallback tiers are unavailable".to_owned(),
            request_id: None,
            decision_id: None,
        }),
        attempts,
        tier: last_tier,
        model: last_model,
        fallback_depth: last_depth,
        runtime_filter_trace,
    })
}

fn fallback_cause(error: Option<&GatewayError>) -> FallbackCause {
    match error.map(|error| error.code) {
        Some("upstream_transport") => FallbackCause::Transport,
        Some("upstream_timeout") => FallbackCause::Timeout,
        Some("upstream_rate_limited") => FallbackCause::RateLimited,
        Some("upstream_server_error") => FallbackCause::ServerError,
        Some("upstream_provider_unavailable") => FallbackCause::ProviderUnavailable,
        Some("upstream_unauthorized") => FallbackCause::Unauthorized,
        Some("upstream_not_found") => FallbackCause::NotFound,
        Some("upstream_bad_request") => FallbackCause::BadRequest,
        Some("tenant_quota_exhausted") => FallbackCause::Quota,
        Some("context_window_exhausted") => FallbackCause::ContextWindow,
        Some("content_policy_rejected") => FallbackCause::ContentPolicy,
        _ => FallbackCause::Capacity,
    }
}

fn typed_fallbacks(state: &AppState, tier_name: &str, cause: FallbackCause) -> Vec<String> {
    state
        .route
        .tiers
        .iter()
        .find(|tier| tier.tier == tier_name)
        .map_or_else(Vec::new, |tier| tier.fallbacks_for(cause).to_vec())
}

#[allow(clippy::too_many_lines)]
async fn execute_tier(
    state: &AppState,
    tier: &ExecutionTier,
    decision: &RouteDecision,
    request: &Value,
    identity: &RequestExecutionIdentity<'_>,
    attempts: &mut Vec<AttemptRecord>,
    runtime_filter_trace: &mut Vec<DeploymentEvaluation>,
) -> Result<TierSuccess, TierExhausted> {
    let candidates = filter_deployments(
        state,
        tier,
        decision,
        identity.tenant_key,
        runtime_filter_trace,
    );
    let mut last_model = fallback_model(state, &candidates, decision);
    let mut last_error = None;
    let mut excluded = BTreeSet::new();
    let mut retries_used = 0_u8;

    loop {
        let Some(lease) = next_lease(
            state,
            &candidates,
            &mut excluded,
            &last_model,
            runtime_filter_trace,
        )
        .await?
        else {
            break;
        };
        let deployment = lease.deployment.clone();
        let prepared = prepare_deployment_request(state, &deployment, request)
            .await
            .map_err(|error| *error)?;
        let model = prepared.model;
        let url = prepared.url;
        let headers = prepared.headers;
        let upstream_request = prepared.request;
        last_model.clone_from(&model);

        match send_deployment_request(state, &url, &headers, &upstream_request).await {
            Ok((response, latency_ms)) => {
                let attempt = observe_attempt(state, &deployment.id, latency_ms, attempts.len());
                let selection_trace = record_cache_affinity_success(
                    state,
                    decision,
                    &deployment,
                    lease.selection_trace.clone(),
                );
                attempts.push(AttemptRecord::new(
                    identity.request_id,
                    attempt,
                    &tier.tier,
                    &deployment,
                    &model,
                    selection_trace,
                    lease.capacity_snapshot.clone(),
                    AttemptOutcome::success(response.status().as_u16(), latency_ms),
                ));
                return Ok(TierSuccess {
                    response,
                    lease,
                    model,
                });
            }
            Err(failure) => {
                let attempt =
                    observe_attempt(state, &deployment.id, failure.latency_ms, attempts.len());
                let selection_trace = lease.selection_trace.clone();
                let capacity_snapshot = lease.capacity_snapshot.clone();
                if let Err(error) = lease.complete(Err(failure.kind)).await {
                    return Err(TierExhausted {
                        error: Some(error),
                        model,
                    });
                }
                let directive = state.retry_policy.directive(
                    failure.kind,
                    retries_used,
                    candidates.len(),
                    failure.retry_after_ms,
                );
                let retry = directive != RetryDirective::Stop;
                attempts.push(AttemptRecord::new(
                    identity.request_id,
                    attempt,
                    &tier.tier,
                    &deployment,
                    &model,
                    selection_trace,
                    capacity_snapshot,
                    AttemptOutcome::failure(&failure, retry),
                ));
                last_error = Some(upstream_error(
                    failure.kind,
                    failure.status,
                    failure.detail,
                    attempts,
                ));
                if !apply_retry_directive(
                    state,
                    directive,
                    &mut retries_used,
                    &mut excluded,
                    &deployment.id,
                )
                .await
                {
                    break;
                }
            }
        }
    }
    Err(tier_exhausted(last_error, last_model))
}

fn record_cache_affinity_success(
    state: &AppState,
    decision: &RouteDecision,
    deployment: &RouteDeployment,
    mut trace: Vec<DeploymentEvaluation>,
) -> Vec<DeploymentEvaluation> {
    let hit = state
        .cache_affinity
        .preferred(decision.prompt_profile_hash.as_deref())
        .as_deref()
        == Some(deployment.id.as_str());
    state
        .cache_affinity
        .remember(decision.prompt_profile_hash.as_deref(), &deployment.id);
    if !hit {
        return trace;
    }
    state
        .metrics
        .cache_affinity_hits
        .fetch_add(1, Ordering::Relaxed);
    if let Some(selected) = trace
        .iter_mut()
        .find(|item| item.deployment == deployment.id)
    {
        selected.reasons.push("cache_affinity_hit".to_owned());
    } else {
        trace.push(DeploymentEvaluation {
            deployment: deployment.id.clone(),
            disposition: DeploymentDisposition::Selected,
            reasons: vec!["cache_affinity_hit".to_owned()],
        });
    }
    trace
}

const fn tier_exhausted(error: Option<GatewayError>, model: ModelSpec) -> TierExhausted {
    TierExhausted { error, model }
}

fn observe_attempt(state: &AppState, deployment: &str, latency_ms: u128, attempts: usize) -> u8 {
    state
        .metrics
        .upstream_attempts
        .fetch_add(1, Ordering::Relaxed);
    state
        .capacity
        .observe_latency(deployment, u64::try_from(latency_ms).unwrap_or(u64::MAX));
    attempt_number(attempts)
}

async fn prepare_deployment_request(
    state: &AppState,
    deployment: &RouteDeployment,
    request: &Value,
) -> Result<PreparedDeployment, Box<TierExhausted>> {
    let model = state.catalog.model(&deployment.model).unwrap().clone();
    let (endpoint, url) = endpoint_for_deployment(state, deployment, &model).map_err(|error| {
        Box::new(TierExhausted {
            error: Some(error),
            model: model.clone(),
        })
    })?;
    let headers = resolve_headers(&state.credentials, &endpoint)
        .await
        .map_err(|error| {
            Box::new(TierExhausted {
                error: Some(error),
                model: model.clone(),
            })
        })?;
    let request = provider_request(request, &model).map_err(|error| {
        Box::new(TierExhausted {
            error: Some(error),
            model: model.clone(),
        })
    })?;
    Ok(PreparedDeployment {
        model,
        url,
        headers,
        request,
    })
}

async fn apply_retry_directive(
    state: &AppState,
    directive: RetryDirective,
    retries_used: &mut u8,
    excluded: &mut BTreeSet<String>,
    deployment_id: &str,
) -> bool {
    if directive == RetryDirective::Stop {
        return false;
    }
    state.metrics.retries.fetch_add(1, Ordering::Relaxed);
    *retries_used = retries_used.saturating_add(1);
    match directive {
        RetryDirective::Stop => unreachable!("stop returned before retry execution"),
        RetryDirective::ReselectDeployment => {
            excluded.insert(deployment_id.to_owned());
        }
        RetryDirective::RetrySameDeployment { backoff_ms } => {
            excluded.clear();
            sleep(Duration::from_millis(backoff_ms)).await;
        }
    }
    true
}

fn filter_deployments(
    state: &AppState,
    tier: &ExecutionTier,
    decision: &RouteDecision,
    tenant_key_value: &str,
    runtime_filter_trace: &mut Vec<DeploymentEvaluation>,
) -> Vec<RouteDeployment> {
    let mut candidates = tier
        .deployments
        .iter()
        .filter_map(|deployment| {
            let reasons = deployment_filter_reasons(state, deployment, decision, tenant_key_value);
            if reasons.is_empty() {
                Some(deployment.clone())
            } else {
                let mut counters = state
                    .metrics
                    .filter_rejections
                    .lock()
                    .expect("filter metric lock poisoned");
                for reason in &reasons {
                    *counters.entry(reason.clone()).or_default() += 1;
                }
                drop(counters);
                runtime_filter_trace.push(DeploymentEvaluation {
                    deployment: deployment.id.clone(),
                    disposition: DeploymentDisposition::Excluded,
                    reasons,
                });
                None
            }
        })
        .collect::<Vec<_>>();
    if let Some(preferred) = state
        .cache_affinity
        .preferred(decision.prompt_profile_hash.as_deref())
        && candidates
            .iter()
            .any(|deployment| deployment.id == preferred)
    {
        for deployment in &mut candidates {
            deployment.order = if deployment.id == preferred {
                0
            } else {
                deployment.order.saturating_add(1)
            };
        }
    }
    candidates
}

fn deployment_filter_reasons(
    state: &AppState,
    deployment: &RouteDeployment,
    decision: &RouteDecision,
    tenant_key_value: &str,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if !deployment.enabled {
        reasons.push("deployment_disabled".to_owned());
    }
    if !deployment.credential_available {
        reasons.push("credential_unavailable".to_owned());
    }
    let bound_request = matches!(decision.reason.as_str(), "task_binding" | "session_binding");
    let within_binding_grace = bound_request
        && deployment
            .binding_grace_until_unix
            .is_some_and(|deadline| unix_seconds() <= deadline);
    if !deployment.accept_new_requests && !within_binding_grace {
        reasons.push("deployment_retired".to_owned());
    }
    if !decision.admission.eligible.contains(&deployment.model) {
        reasons.extend(
            decision
                .admission
                .excluded
                .iter()
                .find(|excluded| excluded.model == deployment.model)
                .map_or_else(
                    || vec!["model_ineligible".to_owned()],
                    |excluded| excluded.reasons.clone(),
                ),
        );
    }
    if decision
        .policy
        .region
        .as_ref()
        .is_some_and(|required| deployment.region.as_ref() != Some(required))
    {
        reasons.push("region_mismatch".to_owned());
    }
    if decision.policy.residency.as_ref().is_some_and(|required| {
        !deployment
            .residency
            .iter()
            .any(|available| available == required)
    }) {
        reasons.push("residency_mismatch".to_owned());
    }
    if !deployment.tenant_allowlist.is_empty()
        && !deployment
            .tenant_allowlist
            .iter()
            .any(|allowed| allowed == tenant_key_value || tenant_key(allowed) == tenant_key_value)
    {
        reasons.push("tenant_not_allowed".to_owned());
    }
    match state.catalog.model(&deployment.model) {
        Some(model) => match endpoint_for_deployment(state, deployment, model) {
            Ok((endpoint, _))
                if deployment.credential_available
                    && !credential_source_configured(&endpoint.auth) =>
            {
                reasons.push("credential_unavailable".to_owned());
            }
            Err(_) => reasons.push("endpoint_unavailable".to_owned()),
            Ok(_) => {}
        },
        None => reasons.push("model_unavailable".to_owned()),
    }
    reasons
}

fn credential_source_configured(plan: &AuthPlan) -> bool {
    match plan {
        AuthPlan::ApiKeyEnv { env, .. } | AuthPlan::AmbientEnv { env, .. } => {
            std::env::var_os(env).is_some()
        }
        AuthPlan::OAuthBearerFile { path, .. } => FsPath::new(path).is_file(),
        AuthPlan::OAuthClientCredentials {
            client_id_env,
            client_secret_env,
            ..
        } => {
            std::env::var_os(client_id_env).is_some()
                && std::env::var_os(client_secret_env).is_some()
        }
        AuthPlan::None => true,
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn fallback_model(
    state: &AppState,
    candidates: &[RouteDeployment],
    decision: &RouteDecision,
) -> ModelSpec {
    candidates
        .first()
        .and_then(|deployment| state.catalog.model(&deployment.model))
        .cloned()
        .unwrap_or_else(|| state.catalog.model(&decision.model).unwrap().clone())
}

async fn next_lease(
    state: &AppState,
    candidates: &[RouteDeployment],
    excluded: &mut BTreeSet<String>,
    model: &ModelSpec,
    runtime_filter_trace: &mut Vec<DeploymentEvaluation>,
) -> Result<Option<ExecutionLease>, TierExhausted> {
    acquire_execution_lease(state, candidates, excluded, runtime_filter_trace)
        .await
        .map_err(|error| TierExhausted {
            error: Some(error),
            model: model.clone(),
        })
}

async fn acquire_execution_lease(
    state: &AppState,
    candidates: &[RouteDeployment],
    excluded: &mut BTreeSet<String>,
    runtime_filter_trace: &mut Vec<DeploymentEvaluation>,
) -> Result<Option<ExecutionLease>, GatewayError> {
    let mut selection_trace = Vec::new();
    loop {
        let snapshot = combined_capacity_snapshot(state, candidates, excluded).await?;
        let local = match state.capacity.select_from_snapshot(candidates, snapshot) {
            Ok(lease) => lease,
            Err(CapacityError::Exhausted(evaluations)) => {
                selection_trace.extend(evaluations);
                runtime_filter_trace.extend(selection_trace);
                return Ok(None);
            }
            Err(error) => return Err(GatewayError::internal(error)),
        };
        selection_trace.extend(local.evaluations.clone());
        let deployment_id = local.deployment.id.clone();
        let deployment = local.deployment.clone();
        let capacity_snapshot = local.snapshot.clone();
        if let Some(permit) = state
            .shared_circuits
            .acquire(&local.deployment)
            .await
            .map_err(state_backend_unavailable)?
        {
            runtime_filter_trace.extend(selection_trace.clone());
            return Ok(Some(ExecutionLease {
                local: Some(local),
                deployment,
                shared: Arc::clone(&state.shared_circuits),
                permit: Some(permit),
                tier_size: candidates.len(),
                selection_trace,
                capacity_snapshot,
            }));
        }
        selection_trace.push(DeploymentEvaluation {
            deployment: deployment_id.clone(),
            disposition: DeploymentDisposition::Excluded,
            reasons: vec!["shared_circuit_unavailable".to_owned()],
        });
        excluded.insert(deployment_id);
    }
}

async fn combined_capacity_snapshot(
    state: &AppState,
    candidates: &[RouteDeployment],
    excluded: &BTreeSet<String>,
) -> Result<CapacitySnapshot, GatewayError> {
    let mut snapshot = state.capacity.capacity_snapshot(candidates, excluded);
    for deployment in candidates {
        let shared = state
            .shared_circuits
            .snapshot(deployment)
            .await
            .map_err(state_backend_unavailable)?;
        let Some(candidate) = snapshot
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == deployment.id)
        else {
            return Err(GatewayError::internal(
                "capacity snapshot omitted a configured deployment",
            ));
        };
        for circuit in shared {
            match circuit.state {
                urouter_gateway::capacity::CircuitState::Open => {
                    candidate.circuit = urouter_contracts::LocalCircuitAvailability::Open;
                    candidate.unavailable_reasons.push(format!(
                        "shared_{}_circuit_unavailable",
                        circuit.scope.as_str()
                    ));
                }
                urouter_gateway::capacity::CircuitState::HalfOpen
                    if candidate.circuit == urouter_contracts::LocalCircuitAvailability::Closed =>
                {
                    candidate.circuit = urouter_contracts::LocalCircuitAvailability::HalfOpen;
                }
                _ => {}
            }
        }
    }
    Ok(snapshot)
}

async fn send_deployment_request(
    state: &AppState,
    url: &str,
    headers: &BTreeMap<String, String>,
    request: &Value,
) -> Result<(reqwest::Response, u128), AttemptFailure> {
    let started = Instant::now();
    let mut builder = state
        .client
        .post(url)
        .timeout(state.request_timeout)
        .json(request);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    match builder.send().await {
        Ok(response) if response.status().is_success() => {
            Ok((response, started.elapsed().as_millis()))
        }
        Ok(response) => {
            let status = response.status();
            let retry_after_ms = parse_retry_after_ms(response.headers());
            let kind = classify_status(status);
            let detail = response
                .text()
                .await
                .unwrap_or_else(|_| "upstream response unavailable".to_owned());
            Err(AttemptFailure {
                kind,
                status: Some(status),
                detail,
                retry_after_ms,
                latency_ms: started.elapsed().as_millis(),
            })
        }
        Err(error) => Err(AttemptFailure {
            kind: if error.is_timeout() {
                UpstreamErrorKind::Timeout
            } else {
                UpstreamErrorKind::Transport
            },
            status: error.status(),
            detail: error.to_string(),
            retry_after_ms: None,
            latency_ms: started.elapsed().as_millis(),
        }),
    }
}

fn execution_tiers(
    state: &AppState,
    decision: &RouteDecision,
) -> Result<Vec<ExecutionTier>, GatewayError> {
    if decision.reason == "explicit_model" {
        return Ok(vec![ExecutionTier {
            tier: decision.tier.clone(),
            deployments: vec![RouteDeployment {
                id: format!("explicit:{}", decision.model),
                model: decision.model.clone(),
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
            }],
        }]);
    }
    let tier_specs = state
        .route
        .tiers
        .iter()
        .map(|tier| FallbackTierSpec {
            tier: tier.tier.clone(),
            fallbacks: tier
                .fallbacks
                .iter()
                .chain(tier.fallbacks_by_error.values().flatten())
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        })
        .collect::<Vec<_>>();
    let plan = plan_fallback_tiers(&tier_specs, &decision.tier, state.max_fallback_depth)
        .map_err(|error| GatewayError::internal(format!("invalid routing plan: {error:?}")))?;
    let mut result = plan
        .tiers
        .iter()
        .map(|tier_name| {
            let tier = state
                .route
                .tiers
                .iter()
                .find(|tier| tier.tier == *tier_name)
                .expect("routing plan only contains configured tiers");
            ExecutionTier {
                tier: tier.tier.clone(),
                deployments: tier.effective_deployments(),
            }
        })
        .collect::<Vec<_>>();
    if decision.conversation_id.is_some()
        && decision.migration_boundary != Some(MigrationBoundary::TerminalProviderFailure)
    {
        for tier in &mut result {
            tier.deployments
                .retain(|deployment| deployment.model == decision.model);
        }
        result.retain(|tier| !tier.deployments.is_empty());
        if result.is_empty() {
            return Err(GatewayError::internal(
                "session-bound model has no configured deployment",
            ));
        }
    }
    Ok(result)
}

fn execution_tier(
    state: &AppState,
    decision: &RouteDecision,
    tier_name: &str,
) -> Result<ExecutionTier, GatewayError> {
    if decision.reason == "explicit_model" {
        return execution_tiers(state, decision).and_then(|tiers| {
            tiers
                .into_iter()
                .next()
                .ok_or_else(|| GatewayError::internal("explicit model has no execution tier"))
        });
    }
    let configured = state
        .route
        .tiers
        .iter()
        .find(|tier| tier.tier == tier_name)
        .ok_or_else(|| GatewayError::internal(format!("unknown fallback tier {tier_name}")))?;
    let mut deployments = configured.effective_deployments();
    if decision.conversation_id.is_some()
        && decision.migration_boundary != Some(MigrationBoundary::TerminalProviderFailure)
    {
        deployments.retain(|deployment| deployment.model == decision.model);
    }
    if deployments.is_empty() {
        return Err(GatewayError::internal(
            "session-bound model has no configured deployment",
        ));
    }
    Ok(ExecutionTier {
        tier: configured.tier.clone(),
        deployments,
    })
}

fn endpoint_for_deployment(
    state: &AppState,
    deployment: &RouteDeployment,
    model: &ModelSpec,
) -> Result<(EndpointPlan, String), GatewayError> {
    let provider = state
        .catalog
        .provider(&model.provider)
        .ok_or_else(|| GatewayError::internal("selected provider disappeared"))?;
    let mut endpoint = EndpointPlan::for_model(provider, model).map_err(GatewayError::internal)?;
    if let Some(base_url) = &deployment.base_url {
        let override_url = url::Url::parse(base_url).map_err(GatewayError::internal)?;
        if endpoint.auth == AuthPlan::None
            && !override_url.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            })
        {
            return Err(GatewayError::bad_request(
                "unsafe_deployment_override",
                "unauthenticated deployment URL override must be loopback",
            ));
        }
        endpoint.url = override_url;
    }
    let path = match model.api {
        WireApi::OpenAiChat => "chat/completions",
        WireApi::OpenAiResponses => "responses",
        WireApi::AnthropicMessages => "messages",
        _ => {
            return Err(GatewayError::bad_request(
                "unsupported_wire_api",
                "custom provider APIs require an installed transport adapter",
            ));
        }
    };
    let url = format!("{}/{path}", endpoint.url.as_str().trim_end_matches('/'));
    Ok((endpoint, url))
}

fn provider_request(request: &Value, model: &ModelSpec) -> Result<Value, GatewayError> {
    match model.api {
        WireApi::OpenAiChat => {
            let mut request = request.clone();
            rewrite_request(&mut request, model);
            Ok(request)
        }
        WireApi::OpenAiResponses => {
            reject_non_chat_stream(request, "open_ai_responses")?;
            let normalized = from_openai_chat(request)
                .map_err(|error| GatewayError::bad_request("protocol_conversion_failed", error))?;
            let (mut request, _) = to_openai_responses(&normalized, LossPolicy::Reject)
                .map_err(|error| GatewayError::bad_request("protocol_semantic_loss", error))?;
            request["model"] = model.upstream_id.clone().into();
            Ok(request)
        }
        WireApi::AnthropicMessages => {
            reject_non_chat_stream(request, "anthropic_messages")?;
            let normalized = from_openai_chat(request)
                .map_err(|error| GatewayError::bad_request("protocol_conversion_failed", error))?;
            let (mut request, _) = to_anthropic_messages(&normalized, LossPolicy::Reject)
                .map_err(|error| GatewayError::bad_request("protocol_semantic_loss", error))?;
            request["model"] = model.upstream_id.clone().into();
            Ok(request)
        }
        _ => Err(GatewayError::bad_request(
            "unsupported_wire_api",
            "custom provider APIs require an installed transport adapter",
        )),
    }
}

fn reject_non_chat_stream(request: &Value, api: &str) -> Result<(), GatewayError> {
    if request.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(GatewayError::bad_request(
            "unsupported_provider_streaming",
            format!("{api} transport does not support streaming handoff"),
        ));
    }
    Ok(())
}

fn provider_response_to_chat(body: Value, api: &WireApi) -> Result<Value, GatewayError> {
    match api {
        WireApi::OpenAiChat => Ok(body),
        WireApi::OpenAiResponses => Ok(responses_provider_to_chat(&body)),
        WireApi::AnthropicMessages => Ok(anthropic_provider_to_chat(&body)),
        _ => Err(GatewayError::internal(
            "custom provider response has no transport adapter",
        )),
    }
}

fn responses_provider_to_chat(body: &Value) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if part.get("type").and_then(Value::as_str) == Some("output_text")
                                && let Some(value) = part.get("text").and_then(Value::as_str)
                            {
                                text.push_str(value);
                            }
                        }
                    }
                }
                Some("reasoning") => {
                    if let Some(parts) = item.get("summary").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(value) = part.get("text").and_then(Value::as_str) {
                                reasoning.push_str(value);
                            }
                        }
                    }
                }
                Some("function_call") => tool_calls.push(json!({
                    "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": item.get("arguments").cloned().unwrap_or_else(|| "{}".into())
                    }
                })),
                _ => {}
            }
        }
    }
    let finish_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = json!({"role": "assistant", "content": text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = reasoning.into();
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = tool_calls.into();
    }
    json!({
        "id": body.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion",
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {
            "prompt_tokens": body.pointer("/usage/input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": body.pointer("/usage/output_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": body.pointer("/usage/total_tokens").cloned().unwrap_or(json!(0))
        }
    })
}

fn anthropic_provider_to_chat(body: &Value) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    if let Some(content) = body.get("content").and_then(Value::as_array) {
        for part in content {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(part.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("tool_use") => tool_calls.push(json!({
                    "id": part.get("id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": part.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": part.get("input").cloned().unwrap_or_else(|| json!({})).to_string()
                    }
                })),
                _ => {}
            }
        }
    }
    let finish_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        message["tool_calls"] = tool_calls.into();
    }
    let input = body
        .pointer("/usage/input_tokens")
        .cloned()
        .unwrap_or(json!(0));
    let output = body
        .pointer("/usage/output_tokens")
        .cloned()
        .unwrap_or(json!(0));
    let total = input
        .as_u64()
        .unwrap_or(0)
        .saturating_add(output.as_u64().unwrap_or(0));
    json!({
        "id": body.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion",
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {"prompt_tokens": input, "completion_tokens": output, "total_tokens": total}
    })
}

fn routed_setup_failure(
    state: &AppState,
    decision: &RouteDecision,
    error: GatewayError,
) -> RoutedFailure {
    RoutedFailure {
        error,
        attempts: Vec::new(),
        tier: decision.tier.clone(),
        model: state
            .catalog
            .model(&decision.model)
            .expect("route decision model exists")
            .clone(),
        fallback_depth: 0,
        runtime_filter_trace: Vec::new(),
    }
}

fn attempt_number(existing: usize) -> u8 {
    u8::try_from(existing.saturating_add(1)).unwrap_or(u8::MAX)
}

fn classify_status(status: reqwest::StatusCode) -> UpstreamErrorKind {
    match status.as_u16() {
        401 | 403 => UpstreamErrorKind::Unauthorized,
        404 => UpstreamErrorKind::NotFound,
        429 => UpstreamErrorKind::RateLimited,
        502..=504 => UpstreamErrorKind::ProviderUnavailable,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::BadRequest,
    }
}

fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(|seconds| seconds.saturating_mul(1_000))
}

fn upstream_error(
    kind: UpstreamErrorKind,
    status: Option<reqwest::StatusCode>,
    detail: impl std::fmt::Display,
    attempts: &[AttemptRecord],
) -> GatewayError {
    let gateway_status = if kind == UpstreamErrorKind::Timeout {
        StatusCode::GATEWAY_TIMEOUT
    } else {
        StatusCode::BAD_GATEWAY
    };
    let code = match kind {
        UpstreamErrorKind::Transport => "upstream_transport",
        UpstreamErrorKind::Timeout => "upstream_timeout",
        UpstreamErrorKind::RateLimited => "upstream_rate_limited",
        UpstreamErrorKind::ServerError => "upstream_server_error",
        UpstreamErrorKind::ProviderUnavailable => "upstream_provider_unavailable",
        UpstreamErrorKind::Unauthorized => "upstream_unauthorized",
        UpstreamErrorKind::NotFound => "upstream_not_found",
        UpstreamErrorKind::BadRequest => "upstream_bad_request",
    };
    let detail = detail.to_string();
    let detail = detail.get(..detail.len().min(2_048)).unwrap_or(&detail);
    GatewayError {
        status: gateway_status,
        code,
        message: format!(
            "upstream status {} after {} attempt(s): {detail}",
            status.map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
            attempts.len()
        ),
        request_id: None,
        decision_id: None,
    }
}

fn rewrite_request(request: &mut Value, model: &ModelSpec) {
    let Some(object) = request.as_object_mut() else {
        return;
    };
    object.remove("urouter");
    object.insert("model".to_owned(), Value::String(model.upstream_id.clone()));
    if model.compat.supports_developer_role == Some(false)
        && let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut)
    {
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("developer") {
                message["role"] = Value::String("system".to_owned());
            }
        }
    }
    normalize_max_tokens(object, model);
}

fn normalize_max_tokens(object: &mut serde_json::Map<String, Value>, model: &ModelSpec) {
    let value = object
        .remove("max_tokens")
        .or_else(|| object.remove("max_completion_tokens"))
        .or_else(|| object.remove("max_output_tokens"));
    let Some(value) = value else {
        return;
    };
    let field = match model.compat.max_tokens_field {
        Some(MaxTokensField::MaxCompletionTokens) => "max_completion_tokens",
        Some(MaxTokensField::MaxOutputTokens) => "max_output_tokens",
        _ => "max_tokens",
    };
    object.insert(field.to_owned(), value);
}

async fn resolve_headers(
    credentials: &CredentialManager,
    endpoint: &EndpointPlan,
) -> Result<BTreeMap<String, String>, GatewayError> {
    let mut headers = endpoint.public_headers.clone();
    if let Some((header, value)) = credentials
        .resolve(&endpoint.auth)
        .await
        .map_err(|_| GatewayError::internal("credential resolution failed"))?
    {
        headers.insert(header, value);
    }
    Ok(headers)
}

async fn non_stream_response(
    state: AppState,
    execution: UpstreamExecution,
    context: ResponseContext,
) -> Result<Response, GatewayError> {
    let ResponseContext {
        decision_id,
        decision,
        governance,
        record,
        headers,
        quota,
        quota_input_tokens,
        budget,
    } = context;
    let UpstreamExecution {
        response,
        elapsed_ms,
        attempts,
        lease,
        model,
        fallback_depth,
        tier: _,
    } = execution;
    let deployment = lease.deployment.id.clone();
    let mut body: Value = match response.json().await {
        Ok(body) => body,
        Err(error) => {
            lease.complete(Err(UpstreamErrorKind::ServerError)).await?;
            return Err(GatewayError::internal(error));
        }
    };
    body = match provider_response_to_chat(body, &model.api) {
        Ok(body) => body,
        Err(error) => {
            lease.complete(Err(UpstreamErrorKind::ServerError)).await?;
            return Err(error);
        }
    };
    lease.complete(Ok(())).await?;
    let usage = parse_usage(&body);
    if usage.is_none() {
        state
            .metrics
            .usage_unavailable
            .fetch_add(1, Ordering::Relaxed);
    }
    settle_quota_usage(&quota, usage, quota_input_tokens, attempts.len()).await;
    let cost = calculate_cost(&model, usage)?;
    observe_execution_metrics(
        &state.metrics,
        elapsed_ms,
        elapsed_ms,
        fallback_depth,
        cost.as_ref(),
        Some(&decision_id),
    );
    budget.settle(&state, cost.as_ref(), &attempts).await;
    commit_task_binding(&state, &governance, &decision, &decision.tier, &model.id).await?;
    let disclosure = disclosure(&decision_id, &decision, &model, cost.clone());
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "urouter".to_owned(),
            serde_json::to_value(&disclosure).map_err(GatewayError::internal)?,
        );
    }
    let record = build_record(
        &state.catalog,
        decision_id,
        decision,
        &model,
        &governance,
        &record,
        ExecutionRecord::success(
            false,
            elapsed_ms,
            usage,
            cost,
            attempts,
            deployment,
            fallback_depth,
        ),
    )?;
    store_record(&state, record).await?;
    let _ = quota.release().await;
    let mut response = Json(body).into_response();
    *response.headers_mut() = headers;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

const STREAM_TAIL_BYTES: usize = 64;

#[allow(clippy::too_many_lines)]
fn stream_response(
    state: AppState,
    execution: UpstreamExecution,
    context: ResponseContext,
) -> Response {
    let ResponseContext {
        decision_id,
        decision,
        governance,
        record,
        headers,
        quota,
        quota_input_tokens,
        budget,
    } = context;
    let UpstreamExecution {
        response,
        elapsed_ms,
        attempts,
        lease,
        model,
        fallback_depth,
        tier: _,
    } = execution;
    let deployment = lease.deployment.id.clone();
    let lease = Arc::new(StdMutex::new(Some(lease)));
    let captured = Arc::new(StdMutex::new(StreamCapture::default()));
    let stream_started = Instant::now();
    let upstream_stream = capture_upstream_stream(
        response,
        Arc::clone(&lease),
        Arc::clone(&captured),
        state.metrics.ttft_ms.clone(),
        elapsed_ms,
        stream_started,
    );
    let final_stream = stream::once(async move {
        complete_stream_lease(&lease).await?;
        let (complete, tail, failed) = captured_stream_parts(&captured);
        if failed {
            observe_stream_execution_metrics(
                &state.metrics,
                elapsed_ms.saturating_add(stream_started.elapsed().as_millis()),
                fallback_depth,
                None,
                &decision_id,
            );
            state
                .metrics
                .stream_failures
                .fetch_add(1, Ordering::Relaxed);
            let _ = quota.release().await;
            return Ok::<Bytes, BoxError>(Bytes::new());
        }
        let usage = parse_stream_usage(&complete);
        if usage.is_none() {
            state
                .metrics
                .usage_unavailable
                .fetch_add(1, Ordering::Relaxed);
        }
        settle_quota_usage(&quota, usage, quota_input_tokens, attempts.len()).await;
        let cost = calculate_cost(&model, usage).ok().flatten();
        observe_stream_execution_metrics(
            &state.metrics,
            elapsed_ms.saturating_add(stream_started.elapsed().as_millis()),
            fallback_depth,
            cost.as_ref(),
            &decision_id,
        );
        budget.settle(&state, cost.as_ref(), &attempts).await;
        commit_task_binding(&state, &governance, &decision, &decision.tier, &model.id)
            .await
            .map_err(|error| Box::new(std::io::Error::other(error.message)) as BoxError)?;
        let disclosure = disclosure(&decision_id, &decision, &model, cost.clone());
        let record = build_record(
            &state.catalog,
            decision_id,
            decision,
            &model,
            &governance,
            &record,
            ExecutionRecord::success(
                true,
                elapsed_ms,
                usage,
                cost,
                attempts,
                deployment,
                fallback_depth,
            ),
        );
        if let Ok(record) = record {
            store_record(&state, record)
                .await
                .map_err(|error| Box::new(std::io::Error::other(error.message)) as BoxError)?;
        }
        state
            .metrics
            .streams_completed
            .fetch_add(1, Ordering::Relaxed);
        let _ = quota.release().await;
        let serialized = serde_json::to_string(&disclosure).unwrap_or_else(|_| "{}".to_owned());
        let mut suffix = remove_sse_done(tail);
        suffix.extend_from_slice(
            format!("\nevent: urouter.decision\ndata: {serialized}\n\ndata: [DONE]\n\n").as_bytes(),
        );
        Ok::<Bytes, BoxError>(Bytes::from(suffix))
    });
    let body = Body::from_stream(upstream_stream.chain(final_stream));
    stream_body_response(body, headers)
}

async fn complete_stream_lease(
    lease: &Arc<StdMutex<Option<ExecutionLease>>>,
) -> Result<(), BoxError> {
    let completed = lease.lock().expect("capacity lease lock poisoned").take();
    if let Some(lease) = completed {
        lease
            .complete(Ok(()))
            .await
            .map_err(|error| Box::new(std::io::Error::other(error.message)) as BoxError)?;
    }
    Ok(())
}

fn observe_stream_execution_metrics(
    metrics: &GatewayMetrics,
    request_duration_ms: u128,
    fallback_depth: u8,
    cost: Option<&CostBreakdown>,
    trace_id: &str,
) {
    observe_execution_metrics(
        metrics,
        request_duration_ms,
        request_duration_ms,
        fallback_depth,
        cost,
        Some(trace_id),
    );
}

fn captured_stream_parts(captured: &Arc<StdMutex<StreamCapture>>) -> (Vec<u8>, Vec<u8>, bool) {
    let capture = captured.lock().expect("stream capture lock poisoned");
    (
        capture.complete.clone(),
        capture.tail.clone(),
        capture.failed,
    )
}

fn capture_upstream_stream(
    response: reqwest::Response,
    stream_lease: Arc<StdMutex<Option<ExecutionLease>>>,
    capture: Arc<StdMutex<StreamCapture>>,
    ttft: Histogram,
    upstream_elapsed_ms: u128,
    stream_started: Instant,
) -> impl futures_util::Stream<Item = Result<Bytes, BoxError>> {
    response.bytes_stream().map(move |item| {
        if item.is_err() {
            capture.lock().expect("stream capture lock poisoned").failed = true;
            if let Some(lease) = stream_lease
                .lock()
                .expect("capacity lease lock poisoned")
                .take()
            {
                tokio::spawn(async move {
                    let _ = lease.complete(Err(UpstreamErrorKind::Transport)).await;
                });
            }
        }
        item.map(|bytes| {
            let mut capture = capture.lock().expect("stream capture lock poisoned");
            if !bytes.is_empty() && !capture.first_chunk_observed {
                capture.first_chunk_observed = true;
                ttft.observe(duration_metric_value(
                    upstream_elapsed_ms.saturating_add(stream_started.elapsed().as_millis()),
                ));
            }
            capture.complete.extend_from_slice(bytes.as_ref());
            capture.tail.extend_from_slice(bytes.as_ref());
            let emit = capture.tail.len().saturating_sub(STREAM_TAIL_BYTES);
            Bytes::from(capture.tail.drain(..emit).collect::<Vec<_>>())
        })
        .map_err(|error| Box::new(error) as BoxError)
    })
}

fn stream_body_response(body: Body, headers: HeaderMap) -> Response {
    let mut response = Response::new(body);
    *response.headers_mut() = headers;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
}

fn remove_sse_done(mut bytes: Vec<u8>) -> Vec<u8> {
    const DONE: &[u8] = b"data: [DONE]";
    if let Some(position) = bytes.windows(DONE.len()).rposition(|window| window == DONE) {
        bytes.drain(position..position + DONE.len());
    }
    bytes
}

fn parse_usage(body: &Value) -> Option<Usage> {
    usage_from_value(body.get("usage")?)
}

fn parse_stream_usage(bytes: &[u8]) -> Option<Usage> {
    let body = String::from_utf8_lossy(bytes);
    body.lines().rev().find_map(|line| {
        let data = line.strip_prefix("data: ")?;
        let value: Value = serde_json::from_str(data).ok()?;
        usage_from_value(value.get("usage")?)
    })
}

fn usage_from_value(value: &Value) -> Option<Usage> {
    let prompt_tokens = value.get("prompt_tokens")?.as_u64()?;
    let cache_read = value
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Usage {
        input: prompt_tokens.checked_sub(cache_read)?,
        output: value.get("completion_tokens")?.as_u64()?,
        cache_read,
        ..Usage::default()
    })
}

fn calculate_cost(
    model: &ModelSpec,
    usage: Option<Usage>,
) -> Result<Option<CostBreakdown>, GatewayError> {
    usage
        .map(|usage| calculate_actual_cost(&model.cost, usage))
        .transpose()
        .map_err(GatewayError::internal)
}

fn build_record(
    catalog: &CatalogSnapshot,
    decision_id: String,
    decision: RouteDecision,
    model: &ModelSpec,
    governance: &RequestGovernance,
    record: &RecordSeed,
    execution: ExecutionRecord,
) -> Result<DecisionRecord, GatewayError> {
    let evidence = CatalogEvidence::from_catalog(catalog, &model.id, PriceSource::Catalog)
        .ok_or_else(|| GatewayError::internal("catalog evidence model disappeared"))?;
    let hashed_task_key = decision
        .task_id
        .as_deref()
        .map(|task_id| task_scope_key(&governance.tenant_key, task_id));
    let hashed_session_key = match (
        decision.conversation_id.as_deref(),
        decision.branch_id.as_deref(),
    ) {
        (Some(conversation), Some(branch)) => Some(session_scope_key(
            &governance.tenant_key,
            conversation,
            branch,
        )),
        _ => None,
    };
    let now = unix_seconds();
    let expires_at_unix_s = now
        .saturating_add(u64::from(governance.policy.retention_days).saturating_mul(24 * 60 * 60));
    let retained = governance.policy.recording != RecordingMode::None;
    let semantic_task = semantic_task_name(&decision).to_owned();
    let policy_filters = full_policy_filters(record);
    let routing_trace = RoutingTrace::current_admission_summary(
        decision.model.to_string(),
        decision.alternatives.iter().map(ToString::to_string),
        decision
            .admission
            .excluded
            .iter()
            .map(|excluded| (excluded.model.to_string(), excluded.reasons.clone())),
        decision.reason.clone(),
    )
    .with_decisions(decision.cascade_trace.clone())
    .with_runtime_filters(execution.runtime_filter_trace.clone())
    .with_policy_filters(policy_filters);
    let eligible_candidates = decision
        .admission
        .eligible
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut context = DecisionRecordContext::deterministic(
        &record.request_id,
        RevisionSet {
            catalog: evidence.content_hash.to_string(),
            route: record.route_revision.clone(),
            feature_schema: FEATURE_SCHEMA_VERSION,
            policy: "current_gateway:v1".to_owned(),
        },
        record.features.clone(),
        routing_trace,
        eligible_candidates,
    );
    context.propensity_millionths = record
        .exploration
        .as_ref()
        .map(|evidence| evidence.propensity_millionths);
    Ok(DecisionRecord {
        context: Some(context),
        decision_id,
        trace_turn: decision.trace_turn,
        trace_id: decision.trace_id,
        parent_turn: decision.parent_turn,
        task_key: hashed_task_key,
        session_key: hashed_session_key,
        agent_harness: decision.agent_harness,
        call_role: decision.call_role,
        migration_boundary: decision.migration_boundary,
        compatibility_mode: governance.compatibility_mode,
        tenant_key: governance.tenant_key.clone(),
        recording: governance.policy.recording,
        training_eligible: retained
            && governance.policy.allow_training
            && !governance.compatibility_mode,
        remote_judge_eligible: retained
            && governance.policy.allow_remote_judge
            && !governance.compatibility_mode,
        expires_at_unix_s,
        messages_hash: record.messages_hash.clone(),
        created_at_unix_s: now,
        redaction_profile: "metadata-v1".to_owned(),
        semantic_task,
        vector_ref: record.vector_ref.clone(),
        exploration: record.exploration.clone(),
        route_id: decision.route_id,
        tier: decision.tier,
        reason: decision.reason,
        alternatives: decision.alternatives,
        evidence,
        requirement: decision.requirement,
        admission: decision.admission,
        execution,
        artifact: record.artifact.clone(),
        override_record: None,
        outcome_signals: Vec::new(),
        tenant_generation: governance.tenant_generation,
        task_generation: governance.task_generation,
    })
}

fn full_trace_filter(filter: &str, reason: &str) -> RuleEvaluation {
    RuleEvaluation {
        rule: filter.to_owned(),
        outcome: RuleOutcome::Selected,
        reason: reason.to_owned(),
    }
}

fn full_policy_filters(record: &RecordSeed) -> Vec<RuleEvaluation> {
    let mut filters = vec![
        full_trace_filter("tenant_policy", "admitted"),
        full_trace_filter("quota", "admitted"),
        full_trace_filter("budget", "admitted"),
    ];
    if let Some(artifact) = &record.artifact {
        filters.push(full_trace_filter(
            "artifact",
            match artifact.source {
                DecisionSource::Rule => "rule_fallback",
                DecisionSource::ActiveArtifact => "active",
                DecisionSource::CandidateCanary => "candidate_canary",
            },
        ));
    }
    if record.exploration.is_some() {
        filters.push(full_trace_filter("exploration", "authorized"));
    }
    filters
}

fn disclosure(
    decision_id: &str,
    decision: &RouteDecision,
    model: &ModelSpec,
    cost: Option<CostBreakdown>,
) -> DecisionDisclosure {
    DecisionDisclosure {
        decision_id: decision_id.to_owned(),
        turn: decision.trace_turn.clone(),
        tier: decision.tier.clone(),
        model: model.id.clone(),
        provider: model.provider.clone(),
        source: "model",
        reason: decision.reason.clone(),
        degraded: false,
        alternatives: decision.alternatives.clone(),
        cost,
        compatibility_mode: decision.compatibility_mode,
    }
}

fn decision_headers(
    request_id: &str,
    decision_id: &str,
    decision: &RouteDecision,
) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-urouter-request-id", request_id),
        ("x-urouter-decision-id", decision_id),
        ("x-urouter-tier", decision.tier.as_str()),
        ("x-urouter-model", decision.model.as_str()),
        ("x-urouter-reason", decision.reason.as_str()),
        ("x-urouter-source", "model"),
        ("x-urouter-degraded", "false"),
        (
            "x-urouter-compatibility-mode",
            if decision.compatibility_mode {
                "true"
            } else {
                "false"
            },
        ),
    ] {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).map_err(GatewayError::internal)?,
            HeaderValue::from_str(value).map_err(GatewayError::internal)?,
        );
    }
    let alternatives = decision
        .alternatives
        .iter()
        .map(ModelId::as_str)
        .collect::<Vec<_>>()
        .join(",");
    headers.insert(
        HeaderName::from_static("x-urouter-alternatives"),
        HeaderValue::from_str(&alternatives).map_err(GatewayError::internal)?,
    );
    Ok(headers)
}

impl RecordStore {
    async fn open(
        path: Option<PathBuf>,
        capacity: usize,
        queue_capacity: usize,
        max_bytes: u64,
    ) -> Result<Self, BoxError> {
        let mut retained = VecDeque::with_capacity(capacity);
        let mut expired_replayed = false;
        if let Some(path) = &path
            && let Ok(contents) = tokio::fs::read_to_string(path).await
        {
            for line in contents.lines() {
                if let Ok(mut record) = serde_json::from_str::<DecisionRecord>(line)
                    .map(DecisionRecord::normalize_after_load)
                {
                    normalize_replayed_record(&mut record);
                    if record_expired(&record) {
                        expired_replayed = true;
                        continue;
                    }
                    if retained.len() == capacity {
                        retained.pop_front();
                    }
                    retained.push_back(record);
                }
            }
        }
        let dropped = Arc::new(AtomicU64::new(0));
        let write_errors = Arc::new(AtomicU64::new(0));
        let writer = path.map(|path| {
            let (sender, receiver) = mpsc::channel(queue_capacity);
            tokio::spawn(record_writer(
                path,
                max_bytes,
                receiver,
                Arc::clone(&write_errors),
            ));
            sender
        });
        let store = Self {
            records: Arc::new(RwLock::new(retained)),
            capacity,
            writer,
            dropped,
            write_errors,
        };
        if expired_replayed {
            store
                .rewrite_current()
                .await
                .map_err(|error| -> BoxError { Box::new(std::io::Error::other(error.message)) })?;
        }
        Ok(store)
    }

    async fn append(&self, record: DecisionRecord) {
        {
            let mut records = self.records.write().await;
            if records.len() == self.capacity {
                records.pop_front();
            }
            records.push_back(record.clone());
        }
        if let Some(writer) = &self.writer
            && writer
                .try_send(RecordCommand::Append(Box::new(record)))
                .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn delete_matching(
        &self,
        predicate: impl Fn(&DecisionRecord) -> bool,
    ) -> Result<usize, GatewayError> {
        let (deleted, snapshot) = {
            let mut records = self.records.write().await;
            let before = records.len();
            records.retain(|record| !predicate(record));
            (
                before.saturating_sub(records.len()),
                records.iter().cloned().collect::<Vec<_>>(),
            )
        };
        if deleted == 0 {
            return Ok(0);
        }
        if let Some(writer) = &self.writer {
            let (completed, receiver) = oneshot::channel();
            writer
                .send(RecordCommand::Rewrite {
                    records: snapshot,
                    completed,
                })
                .await
                .map_err(GatewayError::internal)?;
            if !receiver.await.map_err(GatewayError::internal)? {
                return Err(GatewayError::internal("record rewrite failed"));
            }
        }
        Ok(deleted)
    }

    async fn prune_expired(&self) -> Result<usize, GatewayError> {
        self.delete_matching(record_expired).await
    }

    async fn rewrite_current(&self) -> Result<(), GatewayError> {
        let snapshot = self.records.read().await.iter().cloned().collect();
        if let Some(writer) = &self.writer {
            let (completed, receiver) = oneshot::channel();
            writer
                .send(RecordCommand::Rewrite {
                    records: snapshot,
                    completed,
                })
                .await
                .map_err(GatewayError::internal)?;
            if !receiver.await.map_err(GatewayError::internal)? {
                return Err(GatewayError::internal("record rewrite failed"));
            }
        }
        Ok(())
    }
}

async fn tombstone_record_vectors(
    store: Option<&VectorSideStore>,
    records: &[DecisionRecord],
) -> Result<(), GatewayError> {
    let Some(store) = store else {
        return Ok(());
    };
    for reference in records
        .iter()
        .filter_map(|record| record.vector_ref.as_ref())
    {
        store
            .tombstone(reference)
            .await
            .map_err(|_| GatewayError::internal("semantic vector deletion failed"))?;
    }
    Ok(())
}

fn spawn_retention_sweeper(records: RecordStore, vectors: Option<Arc<VectorSideStore>>) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(60)).await;
            let expired = records
                .records
                .read()
                .await
                .iter()
                .filter(|record| record_expired(record))
                .cloned()
                .collect::<Vec<_>>();
            if records.prune_expired().await.is_ok() {
                let _ = tombstone_record_vectors(vectors.as_deref(), &expired).await;
            }
        }
    });
}

async fn record_writer(
    path: PathBuf,
    max_bytes: u64,
    mut receiver: mpsc::Receiver<RecordCommand>,
    write_errors: Arc<AtomicU64>,
) {
    while let Some(command) = receiver.recv().await {
        let RecordCommand::Append(record) = command else {
            let RecordCommand::Rewrite { records, completed } = command else {
                unreachable!();
            };
            let success = rewrite_records(&path, &records).await.is_ok();
            if !success {
                write_errors.fetch_add(1, Ordering::Relaxed);
            }
            let _ = completed.send(success);
            continue;
        };
        let Ok(mut line) = serde_json::to_vec(&record) else {
            write_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        line.push(b'\n');
        rotate_if_needed(&path, max_bytes, line.len(), &write_errors).await;
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?;
            file.write_all(&line).await
        }
        .await;
        if result.is_err() {
            write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn rewrite_records(path: &PathBuf, records: &[DecisionRecord]) -> std::io::Result<()> {
    let temporary = PathBuf::from(format!("{}.rewrite.tmp", path.display()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    for record in records {
        let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        line.push(b'\n');
        file.write_all(&line).await?;
    }
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary, path).await?;
    remove_rotated_copy(path).await
}

fn normalize_replayed_record(record: &mut DecisionRecord) {
    if record.tenant_key.is_empty() {
        record.tenant_key = tenant_key("local");
    }
    if record.expires_at_unix_s == 0 {
        record.expires_at_unix_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_add(7 * 24 * 60 * 60);
    }
}

fn record_expired(record: &DecisionRecord) -> bool {
    record.expires_at_unix_s
        <= SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
}

async fn feedback_writer(
    path: PathBuf,
    max_bytes: u64,
    mut receiver: mpsc::Receiver<FeedbackCommand>,
    write_errors: Arc<AtomicU64>,
) {
    while let Some(command) = receiver.recv().await {
        let FeedbackCommand::Append(event) = command else {
            let FeedbackCommand::Rewrite { events, completed } = command else {
                unreachable!();
            };
            let success = rewrite_feedback(&path, &events).await.is_ok();
            if !success {
                write_errors.fetch_add(1, Ordering::Relaxed);
            }
            let _ = completed.send(success);
            continue;
        };
        let Ok(mut line) = serde_json::to_vec(&event) else {
            write_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        line.push(b'\n');
        rotate_if_needed(&path, max_bytes, line.len(), &write_errors).await;
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?;
            file.write_all(&line).await
        }
        .await;
        if result.is_err() {
            write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn rewrite_feedback(path: &PathBuf, events: &[FeedbackEvent]) -> std::io::Result<()> {
    let temporary = PathBuf::from(format!("{}.rewrite.tmp", path.display()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    for event in events {
        let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        file.write_all(&line).await?;
    }
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary, path).await?;
    remove_rotated_copy(path).await
}

async fn remove_rotated_copy(path: &FsPath) -> std::io::Result<()> {
    let rotated = PathBuf::from(format!("{}.1", path.display()));
    match tokio::fs::remove_file(rotated).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn rotate_if_needed(
    path: &PathBuf,
    max_bytes: u64,
    incoming_bytes: usize,
    write_errors: &AtomicU64,
) {
    if max_bytes > 0
        && tokio::fs::metadata(path)
            .await
            .is_ok_and(|metadata| metadata.len().saturating_add(incoming_bytes as u64) > max_bytes)
    {
        let rotated = PathBuf::from(format!("{}.1", path.display()));
        let _ = tokio::fs::remove_file(&rotated).await;
        if tokio::fs::rename(path, rotated).await.is_err() {
            write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn store_record(state: &AppState, mut record: DecisionRecord) -> Result<(), GatewayError> {
    if record.recording == RecordingMode::None {
        return Ok(());
    }
    if record.reason == "preference_pin" && record.parent_turn.is_some() {
        let records = records_for_tenant(state, &record.tenant_key)
            .await?
            .into_iter()
            .collect::<VecDeque<_>>();
        record.override_record = Some(evaluate_override(&state.route, &record, &records));
        if record
            .override_record
            .as_ref()
            .is_some_and(|override_record| override_record.paired)
        {
            state.metrics.paired.fetch_add(1, Ordering::Relaxed);
        } else {
            state
                .metrics
                .paired_rejected
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(turn) = &record.trace_turn
        && let Some(signals) = feedback_for_turn(state, &record.tenant_key, turn).await?
    {
        record.outcome_signals = signals;
    }
    state
        .record_repository
        .put(record)
        .await
        .map_err(record_repository_error)
}

fn evaluate_override(
    route: &RouteConfig,
    record: &DecisionRecord,
    records: &VecDeque<DecisionRecord>,
) -> OverrideRecord {
    let parent = record.parent_turn.as_ref().and_then(|parent_turn| {
        records.iter().rev().find(|candidate| {
            candidate.tenant_key == record.tenant_key
                && candidate.trace_turn.as_ref() == Some(parent_turn)
        })
    });
    let mut result = OverrideRecord {
        parent_decision_id: parent.map(|parent| parent.decision_id.clone()),
        parent_tier: parent.map(|parent| parent.tier.clone()),
        chosen_tier: record.tier.clone(),
        kind: parent.and_then(|parent| override_kind(route, &parent.tier, &record.tier)),
        parent_completed: parent.is_some_and(|parent| parent.execution.ok),
        paired: false,
        rejected_reason: None,
    };
    result.rejected_reason = if parent.is_none() {
        Some("parent_not_found".to_owned())
    } else if record.trace_id.is_none()
        || record.trace_id.as_ref() != parent.and_then(|value| value.trace_id.as_ref())
    {
        Some("trace_mismatch".to_owned())
    } else if parent.is_some_and(|value| value.messages_hash != record.messages_hash) {
        Some("messages_mismatch".to_owned())
    } else if parent.is_some_and(|value| value.tier == record.tier) {
        Some("same_tier".to_owned())
    } else if !result.parent_completed {
        Some("parent_incomplete".to_owned())
    } else {
        result.paired = true;
        None
    };
    result
}

fn override_kind(route: &RouteConfig, parent: &str, chosen: &str) -> Option<String> {
    let parent_index = route.tiers.iter().position(|tier| tier.tier == parent)?;
    let chosen_index = route.tiers.iter().position(|tier| tier.tier == chosen)?;
    Some(if chosen_index > parent_index {
        "escalate".to_owned()
    } else {
        "downgrade".to_owned()
    })
}

static ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn next_request_id() -> String {
    next_id("req")
}

fn next_decision_id() -> String {
    next_id("dec")
}

fn next_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = u128::from(ID_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    format!("{prefix}_{:x}", nanos.saturating_add(sequence))
}

fn attempt_id(request_id: &str, attempt: u8) -> String {
    format!("{request_id}:attempt:{attempt}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Request};
    use serde_json::json;
    use tower::ServiceExt;

    fn model() -> ModelSpec {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        catalog
            .model(&ModelId::new("local-vllm/qwen3.5-4b").unwrap())
            .unwrap()
            .clone()
    }

    fn route() -> RouteConfig {
        serde_json::from_str(include_str!("../../../gateway/route.json")).unwrap()
    }

    fn assert_nonempty_cascade_trace(response: &Value) {
        assert!(
            response["routing_trace"]["decisions"]
                .as_array()
                .is_some_and(|events| !events.is_empty())
        );
    }

    #[test]
    fn histogram_uses_cumulative_buckets() {
        let histogram = Histogram::new(&[10, 100]);
        histogram.observe(5);
        histogram.observe(50);
        let mut output = String::new();
        histogram.render(&mut output, "test_duration", "test histogram");
        assert!(output.contains("test_duration_bucket{le=\"10\"} 1"));
        assert!(output.contains("test_duration_bucket{le=\"100\"} 2"));
        assert!(output.contains("test_duration_bucket{le=\"+Inf\"} 2"));
        assert!(output.contains("test_duration_sum 55"));
    }

    fn successful_execution() -> ExecutionRecord {
        ExecutionRecord {
            ok: true,
            stream: false,
            upstream_status: 200,
            upstream_latency_ms: 1,
            usage: None,
            cost: None,
            usage_unavailable: true,
            attempts: Vec::new(),
            error_kind: None,
            deployment: String::new(),
            fallback_depth: 0,
            runtime_filter_trace: Vec::new(),
        }
    }

    fn test_governance() -> RequestGovernance {
        RequestGovernance {
            tenant_key: tenant_key("test-tenant"),
            policy: DataPolicyContract::default(),
            compatibility_mode: false,
            tenant_generation: 0,
            task_generation: 0,
        }
    }

    #[test]
    fn shared_state_errors_are_stable_and_redacted() {
        let error = state_backend_unavailable("redis://secret@host:6379 broken pipe");
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code, "state_backend_unavailable");
        assert!(!error.message.contains("secret"));
        assert!(!error.message.contains("redis"));
    }

    #[test]
    fn request_identity_is_disclosed_on_success_and_error() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let decision = route()
            .decide(&catalog, &json!({"model": "urouter/auto", "messages": []}))
            .unwrap();
        let headers = decision_headers("req_test", "dec_test", &decision).unwrap();
        assert_eq!(headers["x-urouter-request-id"], "req_test");
        assert_eq!(headers["x-urouter-decision-id"], "dec_test");

        let response = GatewayError::bad_request("test_error", "invalid")
            .with_request_id("req_error")
            .into_response();
        assert_eq!(response.headers()["x-urouter-request-id"], "req_error");
    }

    #[tokio::test]
    async fn idempotency_key_is_tenant_scoped_and_rejects_request_drift() {
        let state = test_state_with_route(route()).await;
        let headers = HeaderMap::from_iter([(
            HeaderName::from_static("idempotency-key"),
            HeaderValue::from_static("operation-123"),
        )]);
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let tenant_a = tenant_key("tenant-a");
        let key_hash = idempotency_key_hash(&tenant_a, "operation-123");
        assert!(!key_hash.contains("operation-123"));
        assert_ne!(
            key_hash,
            idempotency_key_hash(&tenant_key("tenant-b"), "operation-123")
        );
        let first = resolve_request_id(
            &state,
            &headers,
            &tenant_a,
            &request,
            "req_first".to_owned(),
        )
        .await
        .unwrap();
        let reused = resolve_request_id(
            &state,
            &headers,
            &tenant_a,
            &request,
            "req_second".to_owned(),
        )
        .await
        .unwrap();
        assert_eq!(first, "req_first");
        assert_eq!(reused, first);

        let conflict = resolve_request_id(
            &state,
            &headers,
            &tenant_a,
            &json!({"model": "urouter/auto", "messages": []}),
            "req_conflict".to_owned(),
        )
        .await
        .unwrap_err();
        assert_eq!(conflict.status, StatusCode::CONFLICT);
        assert_eq!(conflict.code, "idempotency_conflict");

        let other_tenant = resolve_request_id(
            &state,
            &headers,
            &tenant_key("tenant-b"),
            &request,
            "req_other_tenant".to_owned(),
        )
        .await
        .unwrap();
        assert_eq!(other_tenant, "req_other_tenant");
    }

    #[test]
    fn configuration_dry_run_report_pins_revisions_without_credentials() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let route = route();
        route.validate(&catalog).unwrap();
        let mut args = Args::parse_from(["urouter-gateway", "--dry-run"]);
        args.catalog = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../catalog/catalog.json");
        let report = dry_run_report(
            &args,
            include_bytes!("../../../catalog/catalog.json"),
            &catalog,
            &route,
        );
        assert_eq!(report.schema_version, 1);
        assert!(
            report.valid,
            "{}",
            serde_json::to_string_pretty(&report).unwrap()
        );
        assert_eq!(report.route.as_ref().unwrap()["id"], "urouter/auto");
        assert!(
            report.route.as_ref().unwrap()["revision"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert_eq!(report.checks.len(), 16);
        assert_eq!(report.checks[12].status, DryRunStatus::Pass);
    }

    #[test]
    fn dry_run_rejects_exploration_without_a_persistent_record_authority() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let route = route();
        let mut args = Args::parse_from(["urouter-gateway", "--dry-run"]);
        args.exploration_epsilon_millionths = 10_000;
        args.exploration_max_budget_nano_usd = 1;
        let report = dry_run_report(
            &args,
            include_bytes!("../../../catalog/catalog.json"),
            &catalog,
            &route,
        );
        assert!(!report.valid);
        assert_eq!(report.checks[6].status, DryRunStatus::Fail);
    }

    #[test]
    fn dry_run_rejects_invalid_budget_and_implicit_redis_failure_policy() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let route = route();
        let mut args = Args::parse_from(["urouter-gateway", "--dry-run"]);
        args.tenant_budget_soft_nano_usd = 100;
        args.tenant_budget_nano_usd = 100;
        args.redis_url = Some("redis://127.0.0.1:6379".to_owned());
        let report = dry_run_report(
            &args,
            include_bytes!("../../../catalog/catalog.json"),
            &catalog,
            &route,
        );
        assert!(!report.valid);
        assert_eq!(report.checks[8].status, DryRunStatus::Fail);
        assert_eq!(report.checks[9].status, DryRunStatus::Fail);
    }

    #[test]
    fn cost_override_check_requires_a_nonempty_reason() {
        let valid = br#"{"targets":[{"cost_override":{"reason":"contract","cost":{}}}]}"#;
        let invalid = br#"{"targets":[{"cost_override":{"reason":" ","cost":{}}}]}"#;
        assert_eq!(cost_override_reason_check(valid).0, DryRunStatus::Pass);
        assert_eq!(cost_override_reason_check(invalid).0, DryRunStatus::Fail);
    }

    #[test]
    fn cost_class_validation_detects_regressions() {
        assert_eq!(
            monotonic_cost_class_status(&CASCADE_COST_CLASSES),
            DryRunStatus::Pass
        );
        assert_eq!(
            monotonic_cost_class_status(&[ValidationCostClass::Remote, ValidationCostClass::Free,]),
            DryRunStatus::Fail
        );
    }

    #[test]
    fn configuration_dry_run_returns_structured_failure() {
        let args = Args::parse_from([
            "urouter-gateway",
            "--dry-run",
            "--catalog",
            "missing-catalog.json",
        ]);
        let report = configuration_dry_run(&args);
        assert!(!report.valid);
        assert_eq!(report.checks.len(), 16);
        assert_eq!(report.errors[0].code, "catalog_read_failed");
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == DryRunStatus::Blocked)
        );
    }

    #[test]
    fn openapi_contract_is_valid_and_covers_registered_paths() {
        let contract: Value =
            serde_json::from_str(include_str!("../../../gateway/openapi.json")).unwrap();
        assert_eq!(contract["openapi"], "3.1.0");
        let paths = contract["paths"].as_object().unwrap();
        for path in [
            "/health",
            "/health/live",
            "/health/ready",
            "/openapi.json",
            "/v1/models",
            "/v1/catalog",
            "/v1/catalog/refresh",
            "/v1/catalog/rollback",
            "/v1/explain",
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
            "/v1/artifacts",
            "/v1/artifacts/promote",
            "/v1/artifacts/rollback",
            "/v1/artifacts/kill",
            "/v1/artifacts/rollout",
            "/v1/artifacts/observe",
            "/v1/adapters/{harness}/chat/completions",
            "/v1/decisions",
            "/v1/stats",
            "/v1/decisions/{id}",
            "/v1/tasks/{id}/records",
            "/v1/tenant/records",
            "/v1/feedback",
            "/v1/feedback/{turn}",
            "/metrics",
            "/v1/tiers",
            "/v1/tasks/{id}/binding",
            "/v1/sessions/{conversation}/{branch}/binding",
        ] {
            assert!(paths.contains_key(path), "OpenAPI is missing {path}");
        }
    }

    #[test]
    fn protocol_response_translation_preserves_text_tools_usage_and_disclosure() {
        let chat = json!({
            "id": "chat-1",
            "model": "model-a",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "working",
                    "tool_calls": [{
                        "id": "call-1",
                        "function": {"name": "lookup", "arguments": "{\"city\":\"Wuhan\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12},
            "urouter": {"tier": "capable"}
        });
        let responses = chat_to_responses(&chat);
        assert_eq!(responses["object"], "response");
        assert_eq!(responses["output"][1]["type"], "function_call");
        assert_eq!(responses["usage"]["total_tokens"], 12);
        assert_eq!(responses["urouter"]["tier"], "capable");
        let anthropic = chat_to_anthropic(&chat);
        assert_eq!(anthropic["type"], "message");
        assert_eq!(anthropic["content"][1]["type"], "tool_use");
        assert_eq!(anthropic["stop_reason"], "tool_use");
        assert_eq!(anthropic["usage"]["output_tokens"], 7);
    }

    #[test]
    fn protocol_stream_event_matrix_preserves_text_reasoning_tools_and_disclosure() {
        let chunk = br#"data: {"id":"chat-1","choices":[{"delta":{"content":"hello","reasoning_content":"think","tool_calls":[{"index":0,"id":"call-1","function":{"name":"weather","arguments":"{\"city\":"}}]}}]}"#;
        for protocol in [ProtocolResponse::Responses, ProtocolResponse::Anthropic] {
            let mut state = ProtocolTranslationState::default();
            let output =
                String::from_utf8(translate_chat_sse_event(chunk, protocol, &mut state)).unwrap();
            match protocol {
                ProtocolResponse::Responses => {
                    assert!(output.contains("response.output_text.delta"));
                    assert!(output.contains("response.reasoning_text.delta"));
                    assert!(output.contains("response.function_call_arguments.delta"));
                }
                ProtocolResponse::Anthropic => {
                    assert!(output.contains("text_delta"));
                    assert!(output.contains("thinking_delta"));
                    assert!(output.contains("input_json_delta"));
                }
            }
            let disclosure = String::from_utf8(translate_chat_sse_event(
                b"event: urouter.decision\ndata: {\"tier\":\"capable\"}",
                protocol,
                &mut state,
            ))
            .unwrap();
            assert!(disclosure.contains("urouter"));
            let done = String::from_utf8(translate_chat_sse_event(
                b"data: [DONE]",
                protocol,
                &mut state,
            ))
            .unwrap();
            assert!(done.contains(match protocol {
                ProtocolResponse::Responses => "response.completed",
                ProtocolResponse::Anthropic => "message_stop",
            }));
        }
    }

    #[tokio::test]
    async fn protocol_stream_translation_handles_fragmented_sse_chunks() {
        let chunks = stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(
                b"data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"content\":\"hel",
            )),
            Ok(Bytes::from_static(b"lo\"}}]}\n\ndata: [DONE]\n\n")),
        ]);
        let response = Response::new(Body::from_stream(chunks));
        let translated = translate_chat_stream_response(response, ProtocolResponse::Responses);
        let bytes = to_bytes(translated.into_body(), 1_048_576).await.unwrap();
        let output = std::str::from_utf8(&bytes).unwrap();
        assert!(output.contains("response.output_text.delta"));
        assert!(output.contains("hello"));
        assert!(output.contains("response.completed"));
    }

    #[test]
    fn provider_transports_convert_non_stream_requests_and_responses_without_guessing() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let mut model = catalog.models().next().unwrap().clone();
        model.upstream_id = "provider-model".to_owned();
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {}}}]
        });

        model.api = WireApi::OpenAiResponses;
        let responses_request = provider_request(&request, &model).unwrap();
        assert_eq!(responses_request["model"], "provider-model");
        assert!(responses_request["input"].is_array());
        let responses = responses_provider_to_chat(&json!({
            "id": "resp-1",
            "model": "provider-model",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "done"}]},
                {"type": "function_call", "call_id": "call-1", "name": "lookup", "arguments": "{}"}
            ],
            "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7}
        }));
        assert_eq!(responses["choices"][0]["message"]["content"], "done");
        assert_eq!(responses["choices"][0]["finish_reason"], "tool_calls");

        model.api = WireApi::AnthropicMessages;
        let anthropic_request = provider_request(&request, &model).unwrap();
        assert_eq!(anthropic_request["model"], "provider-model");
        assert!(anthropic_request["messages"].is_array());
        let anthropic = anthropic_provider_to_chat(&json!({
            "id": "msg-1",
            "model": "provider-model",
            "content": [{"type": "text", "text": "done"}],
            "usage": {"input_tokens": 3, "output_tokens": 4}
        }));
        assert_eq!(anthropic["choices"][0]["message"]["content"], "done");
        assert_eq!(anthropic["usage"]["total_tokens"], 7);

        let mut stream = request;
        stream["stream"] = true.into();
        assert_eq!(
            provider_request(&stream, &model).unwrap_err().code,
            "unsupported_provider_streaming"
        );
    }

    #[tokio::test]
    async fn artifact_policy_overrides_auto_but_falls_back_when_control_revision_changes() {
        use urouter_artifact::{ArtifactSupportDomain, ExportGates, FeatureWeights, LinearPolicy};

        let mut state = test_state_with_route(route()).await;
        let catalog_revision = state.catalog.hashes().content.to_string();
        let route_revision = state.route.revision();
        let artifact = RouterArtifact::build(
            FEATURE_SCHEMA_VERSION,
            catalog_revision.clone(),
            route_revision.clone(),
            "sha256:dataset",
            42,
            100,
            LinearPolicy {
                baseline_tier: "efficient".to_owned(),
                promoted_tier: "capable".to_owned(),
                threshold_millis: 1_000,
                bias_millis: 2_000,
                weights: FeatureWeights {
                    input_kib_millis: 0,
                    message_millis: 0,
                    tool_millis: 0,
                    image_millis: 0,
                    structured_millis: 0,
                    reasoning_millis: 0,
                },
            },
            ArtifactSupportDomain {
                semantic_tasks: BTreeSet::from(["greeting".to_owned()]),
                maximum_input_text_bytes: 10_000,
                tools_supported: false,
            },
            ExportGates {
                reproducible: true,
                privacy_passed: true,
                support_domain_defined: true,
                counterfactual_passed: true,
                quality_lower_bound_millionths: 1,
                maximum_error_rate_millionths: 0,
                maximum_cost_regression_millionths: 0,
            },
        )
        .unwrap();
        let controller = ArtifactController::new(
            Some(artifact),
            RolloutPolicy {
                shadow: false,
                canary_basis_points: 0,
                minimum_samples: 100,
                operation_limit: 6,
            },
        );
        state.artifact = Some(ArtifactRuntime {
            controller,
            catalog_revision,
            route_revision,
        });
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let mut decision = state.route.decide(&state.catalog, &request).unwrap();
        let applied = apply_artifact_policy(&state, &request, "tenant-a", "task-a", &mut decision)
            .unwrap()
            .unwrap();
        assert_eq!(applied.source, DecisionSource::ActiveArtifact);
        assert_eq!(decision.tier, "capable");
        assert_eq!(decision.reason, "artifact_active");

        state.artifact.as_mut().unwrap().route_revision = "stale".to_owned();
        let mut fallback = state.route.decide(&state.catalog, &request).unwrap();
        let evidence = apply_artifact_policy(&state, &request, "tenant-a", "task-a", &mut fallback)
            .unwrap()
            .unwrap();
        assert_eq!(evidence.source, DecisionSource::Rule);
        assert_eq!(
            evidence.fallback_reason.as_deref(),
            Some("control_revision_changed")
        );
        assert_eq!(fallback.tier, "efficient");
    }

    #[tokio::test]
    async fn controlled_exploration_requires_explicit_consent_and_records_propensity() {
        let mut state = test_state_with_route(route()).await;
        state.exploration = ExplorationPolicy {
            epsilon_millionths: 1_000_000,
            maximum_budget_nano_usd: 10_000,
        };
        let mut request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}],
            "urouter": {
                "contract_version": 2,
                "task": {"id": "task-explore"},
                "agent": {
                    "harness": "test",
                    "prompt_profile_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                    "toolset_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                },
                "call": {"role": "primary"},
                "trace": {
                    "turn": "turn-1",
                    "conversation": "conversation-1",
                    "branch": "main"
                },
                "data_policy": {
                    "recording": "metadata_only",
                    "allow_training": true,
                    "allow_exploration": true,
                    "exploration_budget_nano_usd": 5000
                }
            }
        });
        let mut decision = state.route.decide(&state.catalog, &request).unwrap();
        let evidence = apply_controlled_exploration(
            &state,
            &request,
            "tenant-a",
            "task-explore",
            false,
            &mut decision,
        )
        .unwrap()
        .unwrap();
        assert_eq!(evidence.epsilon_millionths, 1_000_000);
        assert!(evidence.propensity_millionths > 0);
        assert!(evidence.eligible_set.len() >= 2);
        assert!(evidence.selected_by_exploration);

        request["urouter"]["data_policy"]["allow_exploration"] = json!(false);
        let mut denied = state.route.decide(&state.catalog, &request).unwrap();
        assert!(
            apply_controlled_exploration(
                &state,
                &request,
                "tenant-a",
                "task-explore",
                false,
                &mut denied,
            )
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn cache_affinity_prefers_the_last_successful_deployment_and_is_bounded() {
        let mut configured = route();
        let model = configured.tiers[0].model.clone();
        configured.tiers[0].deployments = vec![
            RouteDeployment {
                id: "cold".to_owned(),
                model: model.clone(),
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
            },
            RouteDeployment {
                id: "warm".to_owned(),
                model,
                base_url: None,
                weight: 1,
                order: 10,
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
            },
        ];
        let mut state = test_state_with_route(configured).await;
        state.cache_affinity = CacheAffinityStore::new(1);
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let mut decision = state.route.decide(&state.catalog, &request).unwrap();
        decision.prompt_profile_hash = Some("sha256:profile-a".to_owned());
        state
            .cache_affinity
            .remember(decision.prompt_profile_hash.as_deref(), "warm");
        let tier = execution_tier(&state, &decision, "efficient").unwrap();
        let candidates = filter_deployments(&state, &tier, &decision, "tenant-a", &mut Vec::new());
        assert_eq!(
            candidates
                .iter()
                .find(|item| item.id == "warm")
                .unwrap()
                .order,
            0
        );
        assert!(
            candidates
                .iter()
                .find(|item| item.id == "cold")
                .unwrap()
                .order
                > 0
        );

        state
            .cache_affinity
            .remember(Some("sha256:profile-b"), "cold");
        assert!(
            state
                .cache_affinity
                .preferred(Some("sha256:profile-a"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn baseline_http_contract_routes_through_axum() {
        let app = app_router(test_state_with_route(route()).await);

        let health_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health_response.status(), StatusCode::OK);

        let contract_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(contract_response.status(), StatusCode::OK);
        assert_eq!(
            contract_response.headers()[header::CONTENT_TYPE],
            "application/json"
        );

        let models_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(models_response.status(), StatusCode::OK);
        let models_body = to_bytes(models_response.into_body(), 1_048_576)
            .await
            .unwrap();
        let models_json: Value = serde_json::from_slice(&models_body).unwrap();
        assert_eq!(models_json["object"], "list");
        assert!(models_json["data"].as_array().unwrap().iter().any(|model| {
            model["id"] == "urouter/auto" && model["urouter"]["contract_version"] == 2
        }));

        let explain_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/explain")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "model": "urouter/auto",
                            "messages": [{"role": "user", "content": "hello"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(explain_response.status(), StatusCode::OK);
        let explain_body = to_bytes(explain_response.into_body(), 1_048_576)
            .await
            .unwrap();
        let explain_json: Value = serde_json::from_slice(&explain_body).unwrap();
        assert_eq!(explain_json["schema_version"], 1);
        assert_eq!(explain_json["feature_frame"]["schema_version"], 1);
        assert_eq!(explain_json["routing_trace"]["completeness"], "summary");
        assert_nonempty_cascade_trace(&explain_json);
        assert_eq!(explain_json["route_id"], "urouter/auto");
        assert!(explain_json["tier"].is_string());
        assert!(explain_json["model"].is_string());

        let error_response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/explain")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "model": "not-a-route",
                            "messages": [{"role": "user", "content": "hello"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(error_response.status(), StatusCode::BAD_REQUEST);
        let error_body = to_bytes(error_response.into_body(), 1_048_576)
            .await
            .unwrap();
        let error_json: Value = serde_json::from_slice(&error_body).unwrap();
        assert_eq!(error_json["error"]["type"], "urouter_error");
        assert_eq!(error_json["error"]["code"], "route_rejected");
    }

    #[tokio::test]
    async fn readiness_rejects_traffic_while_draining() {
        let state = test_state_with_route(route()).await;
        let accepting = Arc::clone(&state.accepting);
        let app = app_router(state);

        let ready = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::OK);

        accepting.store(false, Ordering::SeqCst);
        let draining = app
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(draining.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn metrics_v2_exposes_histograms_without_high_cardinality_labels() {
        let state = test_state_with_route(route()).await;
        observe_execution_metrics(&state.metrics, 25, 20, 1, None, Some("decision-test"));
        state.metrics.ttft_ms.observe(10);
        let response = metrics(State(state), HeaderMap::new()).await.unwrap();
        let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        for metric in [
            "urouter_llm_calls_total",
            "urouter_upstream_attempts_total",
            "urouter_usage_unavailable_total",
            "urouter_cache_affinity_hit_total",
            "urouter_vector_write_total",
            "urouter_vector_write_errors_total",
            "urouter_request_duration_milliseconds_bucket",
            "urouter_upstream_duration_milliseconds_bucket",
            "urouter_time_to_first_token_milliseconds_bucket",
            "urouter_request_cost_nano_usd_bucket",
            "urouter_fallback_depth_bucket",
        ] {
            assert!(body.contains(metric), "missing {metric}");
        }
        for forbidden in ["tenant=", "task=", "request_id="] {
            assert!(
                !body.contains(forbidden),
                "forbidden metric label {forbidden}"
            );
        }
        assert!(body.contains("# {trace_id=\"decision-test\"} 25"));
        assert!(body.ends_with("# EOF\n"));
    }

    #[tokio::test]
    async fn graceful_shutdown_drains_an_in_flight_request() {
        let started = Arc::new(tokio::sync::Notify::new());
        let handler_started = Arc::clone(&started);
        let app = Router::new().route(
            "/slow",
            get(move || {
                let handler_started = Arc::clone(&handler_started);
                async move {
                    handler_started.notify_one();
                    sleep(Duration::from_millis(50)).await;
                    "complete"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepting = Arc::new(AtomicBool::new(true));
        let server_accepting = Arc::clone(&accepting);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            serve_with_shutdown(listener, app, server_accepting, 1, async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
        });

        let response = tokio::spawn(async move {
            reqwest::get(format!("http://{address}/slow"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        started.notified().await;
        shutdown_tx.send(()).unwrap();
        for _ in 0..20 {
            if !accepting.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!accepting.load(Ordering::SeqCst));
        assert_eq!(response.await.unwrap(), "complete");
        server.await.unwrap();
    }

    #[test]
    fn zero_shutdown_grace_is_rejected() {
        let mut args = Args::try_parse_from(["urouter-gateway"]).unwrap();
        args.shutdown_grace_seconds = 0;
        let error = validate_args(&args).unwrap_err();
        assert!(error.to_string().contains("shutdown grace period"));
    }

    #[test]
    fn redis_authoritative_state_rejects_local_persistence_files() {
        let args = Args::try_parse_from([
            "urouter-gateway",
            "--redis-url",
            "redis://127.0.0.1:6379/",
            "--records",
            "/tmp/records.jsonl",
        ])
        .unwrap();
        let error = validate_args(&args).unwrap_err();
        assert!(error.to_string().contains("Redis authoritative state"));
    }

    fn record_for(request: &Value, decision_id: &str) -> DecisionRecord {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let route = route();
        let decision = route.decide(&catalog, request).unwrap();
        let model = catalog.model(&decision.model).unwrap();
        let record = RecordSeed {
            request_id: "req-test".to_owned(),
            messages_hash: messages_hash(request).unwrap(),
            features: FeatureFrame::from_openai_chat(request),
            route_revision: route.revision(),
            artifact: None,
            exploration: None,
            vector_ref: None,
        };
        build_record(
            &catalog,
            decision_id.to_owned(),
            decision,
            model,
            &test_governance(),
            &record,
            successful_execution(),
        )
        .unwrap()
    }

    #[test]
    fn value_stats_separate_downgrade_and_cache_savings() {
        let mut document: Value =
            serde_json::from_str(include_str!("../../../catalog/catalog.json")).unwrap();
        for model in document["models"].as_array_mut().unwrap() {
            match model["id"].as_str() {
                Some("local-vllm/qwen3.5-4b") => {
                    model["cost"]["base"] = json!({"input": "2", "output": "4", "cache_read": "0.2", "cache_write": "2.5"});
                }
                Some("local-vllm-qwen38/qwen3.8-27b") => {
                    model["cost"]["base"] = json!({"input": "10", "output": "20", "cache_read": "1", "cache_write": "12.5"});
                }
                _ => {}
            }
        }
        let catalog = CatalogSnapshot::from_document(
            serde_json::from_value(document).expect("valid fixture catalog"),
        )
        .unwrap();
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let mut record = record_for(&request, "value-stats");
        record.evidence = CatalogEvidence::from_catalog(
            &catalog,
            &ModelId::new("local-vllm/qwen3.5-4b").unwrap(),
            PriceSource::Catalog,
        )
        .unwrap();
        record.execution.usage = Some(Usage {
            input: 100,
            output: 100,
            cache_read: 900,
            ..Usage::default()
        });
        record.outcome_signals = vec![FeedbackSignal {
            kind: "quality".to_owned(),
            strength: 0.8,
        }];
        let capable = catalog
            .model(&ModelId::new("local-vllm-qwen38/qwen3.8-27b").unwrap())
            .unwrap();

        let stats = calculate_value_stats(&[record], &catalog, Some(capable));

        assert_eq!(stats.actual_cost_nano_usd, 780_000);
        assert_eq!(stats.cache_savings_nano_usd, 1_620_000);
        assert_eq!(stats.downgrade_savings_nano_usd, 9_600_000);
        assert_eq!(stats.total_savings_nano_usd, 11_220_000);
        assert_eq!(stats.quality_mean, Some(0.8));
        assert!((stats.quality_loss_vs_perfect.unwrap() - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn decision_record_v2_pins_features_candidates_and_revisions() {
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let record = record_for(&request, "decision-v2");
        let context = record.context.as_ref().unwrap();
        assert_eq!(context.schema_version, 2);
        assert_eq!(context.request_id, "req-test");
        assert_eq!(context.revisions.feature_schema, FEATURE_SCHEMA_VERSION);
        assert!(context.revisions.catalog.starts_with("sha256:"));
        assert!(context.revisions.route.starts_with("sha256:"));
        assert!(!context.eligible_candidates.is_empty());
        assert!(context.training_complete());
        assert_eq!(
            context.trace.completeness,
            urouter_contracts::TraceCompleteness::Full
        );
        assert_eq!(context.trace.policy_filters.len(), 3);
    }

    #[tokio::test]
    async fn authorized_semantic_vector_is_side_stored_and_only_referenced() {
        let mut state = test_state_with_route(route()).await;
        let root = std::env::temp_dir().join(format!(
            "urouter-gateway-vectors-{}-{}",
            std::process::id(),
            unix_seconds()
        ));
        let store = VectorSideStore::open(&root, 1_024).await.unwrap();
        state.vector_store = Some(Arc::clone(&store));
        let mut governance = test_governance();
        governance.policy.allow_training = true;
        governance.policy.recording = RecordingMode::MetadataOnly;
        governance.compatibility_mode = false;
        let request = json!({
            "urouter": {"semantic_vector": [0.25, -0.5, 1.0]}
        });
        let reference = persist_semantic_vector(&state, &request, &governance)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reference.dimensions, 3);
        assert_eq!(store.read(&reference).await.unwrap(), vec![0.25, -0.5, 1.0]);
        assert!(!serde_json::to_string(&reference).unwrap().contains("0.25"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn semantic_vector_requires_configured_store_when_training_is_authorized() {
        let state = test_state_with_route(route()).await;
        let mut governance = test_governance();
        governance.policy.allow_training = true;
        governance.policy.recording = RecordingMode::MetadataOnly;
        governance.compatibility_mode = false;
        let error = persist_semantic_vector(
            &state,
            &json!({"urouter": {"semantic_vector": [1.0]}}),
            &governance,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "vector_store_unavailable");
    }

    #[test]
    fn legacy_record_remains_readable_but_is_not_training_eligible() {
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "legacy"}],
            "urouter": {"data_policy": {"allow_training": true}}
        });
        let mut value = serde_json::to_value(record_for(&request, "decision-v1")).unwrap();
        value.as_object_mut().unwrap().remove("context");
        value["training_eligible"] = Value::Bool(true);
        let record = serde_json::from_value::<DecisionRecord>(value)
            .unwrap()
            .normalize_after_load();
        assert!(record.context.is_none());
        assert!(!record.training_eligible);
    }

    async fn test_state_with_route(route: RouteConfig) -> AppState {
        let catalog = Arc::new(
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap(),
        );
        route.validate(&catalog).unwrap();
        let capacity = CapacityManager::new(CooldownPolicy::default());
        for tier in &route.tiers {
            capacity.register(&tier.effective_deployments());
        }
        let records = RecordStore::open(None, 10, 1, 0).await.unwrap();
        let route = Arc::new(route);
        let control = ControlPlane::new(
            ControlSnapshot::from_validated(Arc::clone(&catalog), Arc::clone(&route)),
            ControlFailurePolicy::LastGood,
            None,
        );
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        AppState {
            catalog,
            route,
            client: client.clone(),
            credentials: CredentialManager::new(client),
            records: records.clone(),
            record_repository: MemoryDecisionRecordRepository::new(records),
            feedback: FeedbackStore::default(),
            metrics: GatewayMetrics::default(),
            request_timeout: Duration::from_secs(2),
            retry_policy: RetryPolicy {
                max_retries: 1,
                base_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            capacity,
            shared_circuits: Arc::new(LocalCircuitRepository::default()),
            max_fallback_depth: 5,
            bindings: Arc::new(MemoryTaskBindingRepository::new(100)),
            idempotency: MemoryIdempotencyRepository::new(100),
            idempotency_ttl_seconds: 86_400,
            quota: MemoryQuotaRepository::new(0, 0, 0),
            quota_default_max_output_tokens: 4_096,
            budget: MemoryBudgetRepository::new(0, 2_592_000),
            shared_state: None,
            require_tenant_header: false,
            management_auth: ManagementAuth::disabled(),
            accepting: Arc::new(AtomicBool::new(true)),
            control,
            control_source: None,
            artifact: None,
            exploration: ExplorationPolicy {
                epsilon_millionths: 0,
                maximum_budget_nano_usd: 0,
            },
            cache_affinity: CacheAffinityStore::new(100),
            vector_store: None,
        }
    }

    async fn test_budget_accounting(
        state: &AppState,
        tenant_key: &str,
        request_id: &str,
        decision: &RouteDecision,
        request: &Value,
    ) -> BudgetAccounting {
        let (lease, input_tokens, output_tokens) =
            acquire_request_budget(state, tenant_key, request_id, decision, request)
                .await
                .unwrap();
        BudgetAccounting {
            lease,
            input_tokens,
            output_tokens,
        }
    }

    async fn execute_test_request(
        state: &AppState,
        decision: &RouteDecision,
        request: &Value,
        request_id: &str,
    ) -> Result<UpstreamExecution, RoutedFailure> {
        execute_routed_upstream(state, decision, request, request_id, &tenant_key("local")).await
    }

    fn deployment(id: &str, model: &str, base_url: String, order: u16) -> RouteDeployment {
        RouteDeployment {
            id: id.to_owned(),
            model: ModelId::new(model).unwrap(),
            base_url: Some(base_url),
            weight: 1,
            order,
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
        }
    }

    fn set_tier_deployments(
        route: &mut RouteConfig,
        tier_name: &str,
        deployments: Vec<RouteDeployment>,
    ) {
        route
            .tiers
            .iter_mut()
            .find(|tier| tier.tier == tier_name)
            .unwrap()
            .deployments = deployments;
    }

    fn primary_request(task: &str, turn: &str, difficulty: Option<&str>) -> Value {
        let mut request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "continue the task"}],
            "urouter": {
                "contract_version": 1,
                "task": {"id": task},
                "agent": {"harness": "aionui"},
                "call": {"role": "primary"},
                "trace": {"turn": turn},
                "data_policy": {
                    "recording": "metadata_only",
                    "allow_training": false,
                    "allow_remote_judge": false,
                    "retention_days": 7
                }
            }
        });
        if let Some(difficulty) = difficulty {
            request["urouter"]["hint"]["difficulty"] = Value::String(difficulty.to_owned());
        }
        request
    }

    fn session_request(
        task: &str,
        conversation: &str,
        branch: &str,
        turn: &str,
        difficulty: Option<&str>,
    ) -> Value {
        let mut request = primary_request(task, turn, difficulty);
        request["urouter"]["contract_version"] = json!(2);
        request["urouter"]["trace"]["conversation"] = json!(conversation);
        request["urouter"]["trace"]["branch"] = json!(branch);
        request["urouter"]["agent"]["prompt_profile_hash"] =
            json!("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        request["urouter"]["agent"]["toolset_hash"] =
            json!("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        request
    }

    fn adapter_headers(conversation: &str, turn: &str, call_kind: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-urouter-conversation-id", conversation.parse().unwrap());
        headers.insert("x-urouter-turn-id", turn.parse().unwrap());
        headers.insert("x-urouter-call-kind", call_kind.parse().unwrap());
        headers
    }

    async fn commit_decision_binding(
        state: &AppState,
        governance: &RequestGovernance,
        decision: &RouteDecision,
    ) {
        commit_task_binding(state, governance, decision, &decision.tier, &decision.model)
            .await
            .unwrap();
    }

    #[test]
    fn rewrites_model_contract_role_and_token_field() {
        let mut request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "developer", "content": "rules"}],
            "max_output_tokens": 12,
            "urouter": {"contract_version": 1}
        });
        rewrite_request(&mut request, &model());
        assert_eq!(request["model"], "Qwen3.5-4B");
        assert_eq!(request["messages"][0]["role"], "system");
        assert_eq!(request["max_tokens"], 12);
        assert!(request.get("urouter").is_none());
    }

    #[tokio::test]
    async fn primary_task_keeps_exact_model_across_later_calls() {
        let state = test_state_with_route(route()).await;
        let governance = test_governance();
        let first = primary_request("task-stable", "turn-1", Some("hard"));
        let first_decision = state.route.decide(&state.catalog, &first).unwrap();
        assert_eq!(first_decision.tier, "capable");
        commit_decision_binding(&state, &governance, &first_decision).await;

        let later = primary_request("task-stable", "turn-2", None);
        let mut later_decision = state.route.decide(&state.catalog, &later).unwrap();
        assert_eq!(later_decision.tier, "efficient");
        apply_task_binding(&state, &governance, &mut later_decision)
            .await
            .unwrap();
        assert_eq!(later_decision.tier, "capable");
        assert_eq!(later_decision.model, first_decision.model);
        assert_eq!(later_decision.reason, "task_binding");
        assert_eq!(state.metrics.bindings_applied.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn session_binding_is_branch_scoped_and_requires_safe_identity_migration() {
        let state = test_state_with_route(route()).await;
        let governance = test_governance();
        let first = session_request(
            "task-session",
            "conversation-a",
            "main",
            "turn-1",
            Some("hard"),
        );
        let first_decision = state.route.decide(&state.catalog, &first).unwrap();
        commit_task_binding(
            &state,
            &governance,
            &first_decision,
            &first_decision.tier,
            &first_decision.model,
        )
        .await
        .unwrap();

        let later = session_request("task-session", "conversation-a", "main", "turn-2", None);
        let mut later_decision = state.route.decide(&state.catalog, &later).unwrap();
        apply_task_binding(&state, &governance, &mut later_decision)
            .await
            .unwrap();
        assert_eq!(later_decision.model, first_decision.model);
        assert_eq!(later_decision.reason, "session_binding");

        let other_branch = session_request(
            "task-session",
            "conversation-a",
            "experiment",
            "turn-3",
            None,
        );
        let mut other_decision = state.route.decide(&state.catalog, &other_branch).unwrap();
        apply_task_binding(&state, &governance, &mut other_decision)
            .await
            .unwrap();
        assert_eq!(other_decision.reason, "default_efficient");

        let mut changed = session_request("task-session", "conversation-a", "main", "turn-4", None);
        changed["urouter"]["agent"]["toolset_hash"] =
            json!("sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
        let mut changed_decision = state.route.decide(&state.catalog, &changed).unwrap();
        let error = apply_task_binding(&state, &governance, &mut changed_decision)
            .await
            .unwrap_err();
        assert_eq!(error.code, "unsafe_session_migration");

        changed["urouter"]["call"]["migration_boundary"] = json!("after_compaction");
        let mut migrated = state.route.decide(&state.catalog, &changed).unwrap();
        apply_task_binding(&state, &governance, &mut migrated)
            .await
            .unwrap();
        assert_eq!(migrated.reason, "session_migration");
        commit_task_binding(
            &state,
            &governance,
            &migrated,
            &migrated.tier,
            &migrated.model,
        )
        .await
        .unwrap();
        let key = session_scope_key(&governance.tenant_key, "conversation-a", "main");
        let binding = state.bindings.get(&key).await.unwrap().unwrap();
        assert_eq!(binding.generation, 2);
        assert_eq!(binding.toolset_hash, migrated.toolset_hash);
        assert_eq!(
            state
                .bindings
                .remove_task(
                    &governance.tenant_key,
                    &task_scope_key(&governance.tenant_key, "task-session"),
                )
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn aionui_adapter_preserves_primary_session_and_bypasses_title_call() {
        let state = test_state_with_route(route()).await;
        let governance = test_governance();
        let mut first = json!({
            "model": "urouter/auto",
            "messages": [
                {"role": "system", "content": "stable agent rules"},
                {"role": "user", "content": "plan a hard migration"}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}]
        });
        adapt_agent_request(
            "aionui",
            &adapter_headers("conversation-adapter", "turn-1", "plan"),
            &mut first,
        )
        .unwrap();
        first["urouter"]["hint"]["difficulty"] = json!("hard");
        let first_decision = state.route.decide(&state.catalog, &first).unwrap();
        assert_eq!(first_decision.tier, "capable");
        commit_decision_binding(&state, &governance, &first_decision).await;

        let mut continuation = json!({
            "model": "urouter/auto",
            "messages": [
                {"role": "system", "content": "stable agent rules"},
                {"role": "user", "content": "continue"}
            ],
            "tools": [{"function": {"parameters": {"type": "object"}, "name": "read"}, "type": "function"}]
        });
        adapt_agent_request(
            "aionui",
            &adapter_headers("conversation-adapter", "turn-2", "primary"),
            &mut continuation,
        )
        .unwrap();
        let mut continued = state.route.decide(&state.catalog, &continuation).unwrap();
        apply_task_binding(&state, &governance, &mut continued)
            .await
            .unwrap();
        assert_eq!(continued.model, first_decision.model);
        assert_eq!(continued.reason, "session_binding");
        commit_decision_binding(&state, &governance, &continued).await;

        let mut title = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "make a short title"}]
        });
        adapt_agent_request(
            "aionui",
            &adapter_headers("conversation-adapter", "turn-title", "title"),
            &mut title,
        )
        .unwrap();
        let mut title_decision = state.route.decide(&state.catalog, &title).unwrap();
        apply_task_binding(&state, &governance, &mut title_decision)
            .await
            .unwrap();
        assert_eq!(title_decision.tier, "efficient");
        assert_eq!(title_decision.call_role, Some(CallRole::Auxiliary));
        commit_decision_binding(&state, &governance, &title_decision).await;

        let binding = state
            .bindings
            .get(&session_scope_key(
                &governance.tenant_key,
                "conversation-adapter",
                "main",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.model, first_decision.model);
        assert_eq!(binding.last_seen_turn.as_deref(), Some("turn-2"));

        continuation["tools"] = json!([]);
        continuation.as_object_mut().unwrap().remove("urouter");
        let mut changed_headers = adapter_headers("conversation-adapter", "turn-3", "primary");
        changed_headers.insert("x-urouter-task-id", "conversation-adapter".parse().unwrap());
        adapt_agent_request("aionui", &changed_headers, &mut continuation).unwrap();
        let mut changed = state.route.decide(&state.catalog, &continuation).unwrap();
        assert_eq!(
            apply_task_binding(&state, &governance, &mut changed)
                .await
                .unwrap_err()
                .code,
            "unsafe_session_migration"
        );
    }

    #[tokio::test]
    async fn explicit_retry_boundary_upgrades_bound_task_once() {
        let state = test_state_with_route(route()).await;
        let governance = test_governance();
        let first = primary_request("task-retry", "turn-1", None);
        let first_decision = state.route.decide(&state.catalog, &first).unwrap();
        commit_task_binding(
            &state,
            &governance,
            &first_decision,
            &first_decision.tier,
            &first_decision.model,
        )
        .await
        .unwrap();

        let mut retry = primary_request("task-retry", "turn-2", None);
        retry["urouter"]["call"]["migration_boundary"] =
            Value::String("explicit_user_retry".to_owned());
        let mut retry_decision = state.route.decide(&state.catalog, &retry).unwrap();
        apply_task_binding(&state, &governance, &mut retry_decision)
            .await
            .unwrap();
        assert_eq!(retry_decision.tier, "capable");
        assert_eq!(retry_decision.reason, "task_migration");
        commit_task_binding(
            &state,
            &governance,
            &retry_decision,
            &retry_decision.tier,
            &retry_decision.model,
        )
        .await
        .unwrap();
        assert_eq!(state.metrics.binding_migrations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn stale_concurrent_decision_cannot_overwrite_first_binding() {
        let state = test_state_with_route(route()).await;
        let governance = test_governance();
        let hard = primary_request("task-race", "turn-hard", Some("hard"));
        let normal = primary_request("task-race", "turn-normal", None);
        let hard_decision = state.route.decide(&state.catalog, &hard).unwrap();
        let stale_normal = state.route.decide(&state.catalog, &normal).unwrap();

        commit_task_binding(
            &state,
            &governance,
            &hard_decision,
            &hard_decision.tier,
            &hard_decision.model,
        )
        .await
        .unwrap();
        commit_task_binding(
            &state,
            &governance,
            &stale_normal,
            &stale_normal.tier,
            &stale_normal.model,
        )
        .await
        .unwrap();

        let binding = state
            .bindings
            .get(&task_scope_key(&governance.tenant_key, "task-race"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.model, hard_decision.model);
        assert_eq!(binding.bound_at_turn.as_deref(), Some("turn-hard"));
        assert_eq!(state.metrics.binding_conflicts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auxiliary_and_compatibility_calls_do_not_change_binding() {
        let state = test_state_with_route(route()).await;
        let governance = test_governance();
        let primary = primary_request("task-bypass", "turn-1", Some("hard"));
        let primary_decision = state.route.decide(&state.catalog, &primary).unwrap();
        commit_task_binding(
            &state,
            &governance,
            &primary_decision,
            &primary_decision.tier,
            &primary_decision.model,
        )
        .await
        .unwrap();
        let key = task_scope_key(&governance.tenant_key, "task-bypass");
        let original = state.bindings.get(&key).await.unwrap().unwrap();

        let auxiliary = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "title"}],
            "urouter": {
                "task": {"id": "task-bypass"},
                "agent": {"harness": "aionui"},
                "call": {"role": "auxiliary"},
                "trace": {"turn": "turn-title"}
            }
        });
        let mut auxiliary_decision = state.route.decide(&state.catalog, &auxiliary).unwrap();
        apply_task_binding(&state, &governance, &mut auxiliary_decision)
            .await
            .unwrap();
        assert_eq!(auxiliary_decision.tier, "efficient");
        commit_task_binding(
            &state,
            &governance,
            &auxiliary_decision,
            &auxiliary_decision.tier,
            &auxiliary_decision.model,
        )
        .await
        .unwrap();
        assert_eq!(
            state.bindings.get(&key).await.unwrap().unwrap().model,
            original.model
        );

        let legacy = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "legacy"}]
        });
        let legacy_decision = state.route.decide(&state.catalog, &legacy).unwrap();
        assert!(legacy_decision.compatibility_mode);
        commit_task_binding(
            &state,
            &governance,
            &legacy_decision,
            &legacy_decision.tier,
            &legacy_decision.model,
        )
        .await
        .unwrap();
        assert_eq!(
            state.bindings.get(&key).await.unwrap().unwrap().model,
            original.model
        );
    }

    #[tokio::test]
    async fn binding_store_is_bounded_and_does_not_retain_raw_task_ids() {
        let store = MemoryTaskBindingRepository::new(1);
        let tenant = tenant_key("test-tenant");
        for task in ["sensitive-task-one", "sensitive-task-two"] {
            store
                .put(
                    TaskBinding {
                        tenant_key: tenant.clone(),
                        binding_key: task_scope_key(&tenant, task),
                        task_key: task_scope_key(&tenant, task),
                        conversation_key: None,
                        branch_key: None,
                        tier: "efficient".to_owned(),
                        model: ModelId::new("local-vllm/qwen3.5-4b").unwrap(),
                        provider: "local-vllm".to_owned(),
                        api: "open_ai_chat".to_owned(),
                        agent_harness: Some("aionui".to_owned()),
                        prompt_profile_hash: None,
                        toolset_hash: None,
                        bound_at_turn: Some("turn".to_owned()),
                        last_seen_turn: Some("turn".to_owned()),
                        generation: 1,
                        tenant_generation: 0,
                        task_generation: 0,
                    },
                    false,
                )
                .await
                .unwrap();
        }
        assert!(
            store
                .get(&task_scope_key(&tenant, "sensitive-task-one"))
                .await
                .unwrap()
                .is_none()
        );
        let retained = store
            .get(&task_scope_key(&tenant, "sensitive-task-two"))
            .await
            .unwrap()
            .unwrap();
        assert!(!retained.task_key.contains("sensitive-task-two"));
        assert!(store.remove(&retained.task_key).await.unwrap());
    }

    #[test]
    fn parses_terminal_stream_usage() {
        let usage = parse_stream_usage(
            b"data: {\"choices\":[]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\ndata: [DONE]\n",
        )
        .unwrap();
        assert_eq!(usage.input, 12);
        assert_eq!(usage.output, 3);
    }

    #[test]
    fn removes_done_marker_from_stream_tail() {
        let tail = remove_sse_done(b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n".to_vec());
        assert_eq!(tail, b"data: {\"choices\":[]}\n\n\n\n");
    }

    #[test]
    fn override_pair_requires_same_trace_and_messages() {
        let parent_request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "solve"}],
            "urouter": {"trace": {"id": "trace-1", "turn": "turn-1"}}
        });
        let parent = record_for(&parent_request, "decision-1");
        let child_request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "solve"}],
            "urouter": {
                "trace": {"id": "trace-1", "turn": "turn-2", "parent_turn": "turn-1"},
                "preference": {"pin_tier": "capable"}
            }
        });
        let child = record_for(&child_request, "decision-2");
        let records = VecDeque::from([parent]);
        let paired = evaluate_override(&route(), &child, &records);
        assert!(paired.paired);
        assert_eq!(paired.kind.as_deref(), Some("escalate"));

        let changed_request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "solve a different problem"}],
            "urouter": {
                "trace": {"id": "trace-1", "turn": "turn-3", "parent_turn": "turn-1"},
                "preference": {"pin_tier": "capable"}
            }
        });
        let changed = record_for(&changed_request, "decision-3");
        let rejected = evaluate_override(&route(), &changed, &records);
        assert!(!rejected.paired);
        assert_eq!(
            rejected.rejected_reason.as_deref(),
            Some("messages_mismatch")
        );
    }

    #[tokio::test]
    async fn feedback_is_idempotent_per_turn_and_kind() {
        let store = FeedbackStore::default();
        let tenant = tenant_key("test-tenant");
        store
            .upsert(
                &tenant,
                "turn-1",
                vec![FeedbackSignal {
                    kind: "accepted".to_owned(),
                    strength: 0.5,
                }],
            )
            .await;
        let signals = store
            .upsert(
                &tenant,
                "turn-1",
                vec![FeedbackSignal {
                    kind: "accepted".to_owned(),
                    strength: 1.0,
                }],
            )
            .await;
        assert_eq!(signals.len(), 1);
        assert!((signals[0].strength - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn feedback_events_recover_after_restart() {
        let path = std::env::temp_dir().join(format!(
            "urouter-feedback-test-{}.jsonl",
            next_decision_id()
        ));
        let store = FeedbackStore::open(Some(path.clone()), 4, 1_000_000)
            .await
            .unwrap();
        let tenant = tenant_key("test-tenant");
        store
            .upsert(
                &tenant,
                "turn-recover",
                vec![FeedbackSignal {
                    kind: "task_succeeded".to_owned(),
                    strength: 0.9,
                }],
            )
            .await;
        for _ in 0..50 {
            if tokio::fs::metadata(&path)
                .await
                .is_ok_and(|metadata| metadata.len() > 0)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        drop(store);
        let recovered = FeedbackStore::open(Some(path.clone()), 4, 1_000_000)
            .await
            .unwrap();
        let values = recovered.values.read().await;
        assert!(
            values[&feedback_scope_key(&tenant, "turn-recover")].contains_key("task_succeeded")
        );
        drop(values);
        let records = RecordStore::open(None, 2, 1, 0).await.unwrap();
        records
            .append(record_for(
                &json!({
                    "model": "urouter/auto",
                    "messages": [{"role": "user", "content": "recover"}],
                    "urouter": {"trace": {"turn": "turn-recover"}}
                }),
                "decision-recover",
            ))
            .await;
        reconcile_feedback(&records, &recovered).await;
        assert_eq!(
            records.records.read().await[0].outcome_signals[0].kind,
            "task_succeeded"
        );
        drop(recovered);
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn tenant_scope_isolates_feedback_and_task_bindings() {
        let state = test_state_with_route(route()).await;
        let tenant_a = RequestGovernance {
            tenant_key: tenant_key("tenant-a"),
            ..test_governance()
        };
        let tenant_b = RequestGovernance {
            tenant_key: tenant_key("tenant-b"),
            ..test_governance()
        };
        let request = primary_request("shared-task", "shared-turn", Some("hard"));
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        commit_task_binding(
            &state,
            &tenant_a,
            &decision,
            &decision.tier,
            &decision.model,
        )
        .await
        .unwrap();
        assert!(
            state
                .bindings
                .get(&task_scope_key(&tenant_a.tenant_key, "shared-task"))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            state
                .bindings
                .get(&task_scope_key(&tenant_b.tenant_key, "shared-task"))
                .await
                .unwrap()
                .is_none()
        );

        state
            .feedback
            .upsert(
                &tenant_a.tenant_key,
                "shared-turn",
                vec![FeedbackSignal {
                    kind: "accepted".to_owned(),
                    strength: 1.0,
                }],
            )
            .await;
        let values = state.feedback.values.read().await;
        assert!(values.contains_key(&feedback_scope_key(&tenant_a.tenant_key, "shared-turn")));
        assert!(!values.contains_key(&feedback_scope_key(&tenant_b.tenant_key, "shared-turn")));
    }

    #[tokio::test]
    #[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
    #[allow(clippy::too_many_lines)]
    async fn redis_records_and_feedback_are_cross_instance_consistent() {
        let url = std::env::var("UROUTER_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16380/".to_owned());
        let prefix = format!("urouter-shared-state-test-{}", next_decision_id());
        let mut state_a = test_state_with_route(route()).await;
        state_a.shared_state = Some(
            RedisSharedState::connect(&url, prefix.clone())
                .await
                .unwrap(),
        );
        state_a.idempotency =
            RedisIdempotencyRepository::new(state_a.shared_state.as_ref().unwrap().clone());
        state_a.record_repository = RedisDecisionRecordRepository::new(
            state_a.shared_state.as_ref().unwrap().clone(),
            state_a.records.clone(),
            state_a.records.capacity,
        );
        let mut state_b = test_state_with_route(route()).await;
        state_b.shared_state = Some(RedisSharedState::connect(&url, prefix).await.unwrap());
        state_b.idempotency =
            RedisIdempotencyRepository::new(state_b.shared_state.as_ref().unwrap().clone());
        state_b.record_repository = RedisDecisionRecordRepository::new(
            state_b.shared_state.as_ref().unwrap().clone(),
            state_b.records.clone(),
            state_b.records.capacity,
        );
        let tenant = tenant_key("shared-state-tenant");
        let idempotency_request = json!({"model": "urouter/auto", "messages": []});
        let headers = HeaderMap::from_iter([(
            HeaderName::from_static("idempotency-key"),
            HeaderValue::from_static("shared-operation"),
        )]);
        let first_request_id = resolve_request_id(
            &state_a,
            &headers,
            &tenant,
            &idempotency_request,
            "req_shared_first".to_owned(),
        )
        .await
        .unwrap();
        let second_request_id = resolve_request_id(
            &state_b,
            &headers,
            &tenant,
            &idempotency_request,
            "req_shared_second".to_owned(),
        )
        .await
        .unwrap();
        assert_eq!(first_request_id, second_request_id);
        let mut record = record_for(
            &primary_request("shared-state-task", "shared-state-turn", None),
            "shared-state-decision",
        );
        record.tenant_key.clone_from(&tenant);
        store_record(&state_a, record).await.unwrap();

        let records = records_for_tenant(&state_b, &tenant).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision_id, "shared-state-decision");
        ingest_feedback(
            &state_b,
            &tenant,
            "shared-state-turn",
            vec![FeedbackSignal {
                kind: "accepted".to_owned(),
                strength: 1.0,
            }],
            7,
        )
        .await
        .unwrap();
        let recovered = record_for_id(&state_a, &tenant, "shared-state-decision")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.outcome_signals[0].kind, "accepted");
        assert!(
            feedback_for_turn(&state_a, &tenant, "shared-state-turn")
                .await
                .unwrap()
                .is_some()
        );
        let (left, right) = tokio::join!(
            ingest_feedback(
                &state_a,
                &tenant,
                "shared-state-turn",
                vec![FeedbackSignal {
                    kind: "disputed".to_owned(),
                    strength: 0.8,
                }],
                7,
            ),
            ingest_feedback(
                &state_b,
                &tenant,
                "shared-state-turn",
                vec![FeedbackSignal {
                    kind: "task_succeeded".to_owned(),
                    strength: 0.9,
                }],
                7,
            )
        );
        left.unwrap();
        right.unwrap();
        let merged = feedback_for_turn(&state_a, &tenant, "shared-state-turn")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.len(), 3);
        let hydrated = record_for_id(&state_b, &tenant, "shared-state-decision")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hydrated.outcome_signals.len(), 3);

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-urouter-tenant-id",
            HeaderValue::from_static("shared-state-tenant"),
        );
        assert_eq!(
            delete_decision(
                State(state_b.clone()),
                Path("shared-state-decision".to_owned()),
                headers,
            )
            .await
            .unwrap(),
            StatusCode::NO_CONTENT
        );
        assert!(
            record_for_id(&state_a, &tenant, "shared-state-decision")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            feedback_for_turn(&state_a, &tenant, "shared-state-turn")
                .await
                .unwrap()
                .is_none()
        );

        let mut expiring = record_for(
            &primary_request("expiring-task", "expiring-turn", None),
            "expiring-decision",
        );
        expiring.tenant_key.clone_from(&tenant);
        expiring.expires_at_unix_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_add(1);
        let shared = state_a.shared_state.as_ref().unwrap();
        shared.put_record(&expiring, 10).await.unwrap();
        shared
            .upsert_feedback(
                &tenant,
                "expiring-turn",
                &[FeedbackSignal {
                    kind: "accepted".to_owned(),
                    strength: 1.0,
                }],
                1,
            )
            .await
            .unwrap();
        sleep(Duration::from_millis(1_100)).await;
        assert!(
            shared
                .record(&tenant, "expiring-decision")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            shared
                .feedback(&tenant, "expiring-turn")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn tenant_header_can_be_mandatory() {
        let mut state = test_state_with_route(route()).await;
        state.require_tenant_header = true;
        let error = resolve_tenant(&state, &HeaderMap::new()).unwrap_err();
        assert_eq!(error.code, "tenant_required");

        let mut headers = HeaderMap::new();
        headers.insert("x-urouter-tenant-id", HeaderValue::from_static("tenant-a"));
        let (resolved, compatibility) = resolve_tenant(&state, &headers).unwrap();
        assert_eq!(resolved, tenant_key("tenant-a"));
        assert!(!compatibility);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn management_endpoints_enforce_role_and_tenant_scope() {
        let suffix = next_decision_id();
        let keyring_path = std::env::temp_dir().join(format!("urouter-keyring-{suffix}.json"));
        let audit_path = std::env::temp_dir().join(format!("urouter-audit-{suffix}.jsonl"));
        let tenant_a = tenant_key("tenant-a");
        let key = |id: &str, token: &str, role: &str, tenants: Vec<String>| {
            json!({
                "id": id,
                "token_sha256": format!("sha256:{:x}", Sha256::digest(token.as_bytes())),
                "role": role,
                "tenant_keys": tenants
            })
        };
        tokio::fs::write(
            &keyring_path,
            serde_json::to_vec(&json!({
                "version": 1,
                "keys": [
                    key("reader", "reader-token", "reader", vec![tenant_a.clone()]),
                    key("operator", "operator-token", "operator", vec![tenant_a.clone()]),
                    key("admin", "admin-token", "admin", vec!["*".to_owned()])
                ]
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let mut state = test_state_with_route(route()).await;
        state.management_auth = ManagementAuth::open(
            Some(keyring_path.clone()),
            Some(audit_path.clone()),
            16,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let mut record = record_for(&primary_request("task-a", "turn-a", None), "decision-a");
        record.tenant_key.clone_from(&tenant_a);
        state.records.append(record).await;
        let request_headers = |tenant: &'static str, token: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-urouter-tenant-id", HeaderValue::from_static(tenant));
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
            headers
        };

        assert!(
            decisions(
                State(state.clone()),
                request_headers("tenant-a", "reader-token"),
                Query(DecisionQuery::default()),
            )
            .await
            .is_ok()
        );
        assert_eq!(
            delete_decision(
                State(state.clone()),
                Path("decision-a".to_owned()),
                request_headers("tenant-a", "reader-token"),
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            metrics(
                State(state.clone()),
                request_headers("tenant-a", "operator-token"),
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            decisions(
                State(state.clone()),
                request_headers("tenant-b", "reader-token"),
                Query(DecisionQuery::default()),
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            decisions(
                State(state.clone()),
                request_headers("tenant-a", "wrong-token"),
                Query(DecisionQuery::default()),
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::UNAUTHORIZED
        );
        assert!(
            metrics(
                State(state.clone()),
                request_headers("tenant-a", "admin-token"),
            )
            .await
            .is_ok()
        );
        assert_eq!(
            delete_decision(
                State(state.clone()),
                Path("decision-a".to_owned()),
                request_headers("tenant-a", "operator-token"),
            )
            .await
            .unwrap(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            resolve_tenant(&state, &HeaderMap::new()).unwrap_err().code,
            "tenant_required"
        );
        let audit = tokio::fs::read_to_string(&audit_path).await.unwrap();
        assert_eq!(audit.lines().count(), 7);
        assert!(!audit.contains("reader-token"));
        let _ = tokio::fs::remove_file(keyring_path).await;
        let _ = tokio::fs::remove_file(audit_path).await;
    }

    #[tokio::test]
    async fn catalog_management_reports_status_and_stable_conflicts() {
        let state = test_state_with_route(route()).await;
        let status = catalog_status(State(state.clone()), HeaderMap::new())
            .await
            .unwrap();
        assert!(status.0.contains_key(header::ETAG));
        assert_eq!(status.1.0["hot_reload"], false);
        assert_eq!(status.1.0["control"]["ready"], true);

        let refresh = refresh_catalog(State(state.clone()), HeaderMap::new())
            .await
            .unwrap_err();
        assert_eq!(refresh.status, StatusCode::CONFLICT);
        assert_eq!(refresh.code, "control_reload_not_configured");

        let rollback = rollback_catalog(State(state), HeaderMap::new())
            .await
            .unwrap_err();
        assert_eq!(rollback.status, StatusCode::CONFLICT);
        assert_eq!(rollback.code, "control_rollback_unavailable");
    }

    #[tokio::test]
    async fn recording_none_retains_no_decision_or_piggyback_feedback() {
        let state = test_state_with_route(route()).await;
        let mut governance = test_governance();
        governance.policy.recording = RecordingMode::None;
        governance.policy.allow_training = true;
        governance.policy.allow_remote_judge = true;
        let request = primary_request("private-task", "private-turn", None);
        let catalog = &state.catalog;
        let decision = state.route.decide(catalog, &request).unwrap();
        let model = catalog.model(&decision.model).unwrap();
        let record_seed = RecordSeed {
            request_id: "req-private".to_owned(),
            messages_hash: messages_hash(&request).unwrap(),
            features: FeatureFrame::from_openai_chat(&request),
            route_revision: state.route.revision(),
            artifact: None,
            exploration: None,
            vector_ref: None,
        };
        let record = build_record(
            catalog,
            "decision-private".to_owned(),
            decision,
            model,
            &governance,
            &record_seed,
            successful_execution(),
        )
        .unwrap();
        assert!(!record.training_eligible);
        assert!(!record.remote_judge_eligible);
        store_record(&state, record).await.unwrap();
        ingest_piggyback(
            &state,
            &governance,
            &[SignalContract {
                turn: Some("private-turn".to_owned()),
                kind: Some("accepted".to_owned()),
                strength: None,
            }],
        )
        .await
        .unwrap();
        assert!(state.records.records.read().await.is_empty());
        assert!(state.feedback.values.read().await.is_empty());
    }

    #[tokio::test]
    async fn expired_records_are_pruned() {
        let store = RecordStore::open(None, 4, 1, 0).await.unwrap();
        let mut expired = record_for(
            &primary_request("old-task", "old-turn", None),
            "decision-old",
        );
        expired.expires_at_unix_s = 1;
        store.append(expired).await;
        assert_eq!(store.prune_expired().await.unwrap(), 1);
        assert!(store.records.read().await.is_empty());
    }

    #[tokio::test]
    async fn persistent_delete_survives_restart_and_removes_rotated_copy() {
        let path = std::env::temp_dir().join(format!(
            "urouter-record-delete-test-{}.jsonl",
            next_decision_id()
        ));
        let rotated = PathBuf::from(format!("{}.1", path.display()));
        let store = RecordStore::open(Some(path.clone()), 8, 8, 1_000_000)
            .await
            .unwrap();
        let mut tenant_a = record_for(&primary_request("task-a", "turn-a", None), "decision-a");
        tenant_a.tenant_key = tenant_key("tenant-a");
        let mut tenant_b = record_for(&primary_request("task-b", "turn-b", None), "decision-b");
        tenant_b.tenant_key = tenant_key("tenant-b");
        store.append(tenant_a).await;
        store.append(tenant_b).await;
        for _ in 0..50 {
            if tokio::fs::read_to_string(&path)
                .await
                .is_ok_and(|contents| contents.contains("decision-b"))
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        tokio::fs::copy(&path, &rotated).await.unwrap();
        assert_eq!(
            store
                .delete_matching(|record| record.tenant_key == tenant_key("tenant-a"))
                .await
                .unwrap(),
            1
        );
        assert!(!tokio::fs::try_exists(&rotated).await.unwrap());
        drop(store);

        let recovered = RecordStore::open(Some(path.clone()), 8, 8, 1_000_000)
            .await
            .unwrap();
        let records = recovered.records.read().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision_id, "decision-b");
        drop(records);
        drop(recovered);
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn record_store_retains_configured_capacity() {
        let store = RecordStore::open(None, 2, 1, 0).await.unwrap();
        for index in 0..3 {
            let request = json!({
                "model": "urouter/auto",
                "messages": [{"role": "user", "content": format!("request {index}")}]
            });
            store
                .append(record_for(&request, &format!("decision-{index}")))
                .await;
        }
        let records = store.records.read().await;
        assert_eq!(records.len(), 2);
        assert_eq!(records.front().unwrap().decision_id, "decision-1");
    }

    async fn assert_record_repository_contract(repository: Arc<dyn DecisionRecordRepository>) {
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "record contract"}]
        });
        let mut record = record_for(&request, "decision-contract");
        let tenant = record.tenant_key.clone();
        record.task_key = Some("task-contract".to_owned());
        repository.put(record).await.unwrap();
        let other_tenant = tenant_key("record-contract-other");
        let mut other = record_for(&request, "decision-other");
        other.tenant_key.clone_from(&other_tenant);
        repository.put(other).await.unwrap();

        assert_eq!(repository.list(&tenant).await.unwrap().len(), 1);
        assert_eq!(repository.list(&other_tenant).await.unwrap().len(), 1);
        assert!(
            repository
                .get(&tenant, "decision-contract")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            repository
                .get(&other_tenant, "decision-contract")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            repository
                .delete(
                    &tenant,
                    RecordDelete::Task {
                        decision_ids: vec!["decision-contract".to_owned()],
                        task_key: "task-contract".to_owned(),
                        before_generation: 1,
                    },
                )
                .await
                .unwrap(),
            1
        );
        assert!(repository.list(&tenant).await.unwrap().is_empty());
        assert_eq!(repository.list(&other_tenant).await.unwrap().len(), 1);
        assert_eq!(
            repository
                .delete(
                    &other_tenant,
                    RecordDelete::Decisions(vec!["decision-other".to_owned()]),
                )
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn memory_record_repository_passes_shared_contract() {
        let store = RecordStore::open(None, 4, 1, 0).await.unwrap();
        assert_record_repository_contract(MemoryDecisionRecordRepository::new(store)).await;
    }

    #[tokio::test]
    async fn memory_record_repository_honors_generation_boundary() {
        let store = RecordStore::open(None, 4, 1, 0).await.unwrap();
        let repository = MemoryDecisionRecordRepository::new(store);
        let request = json!({"model": "urouter/auto", "messages": []});
        let mut record = record_for(&request, "decision-generation");
        let tenant = record.tenant_key.clone();
        record.task_key = Some("task-generation".to_owned());
        repository.put(record).await.unwrap();
        assert_eq!(
            repository
                .delete(
                    &tenant,
                    RecordDelete::Task {
                        decision_ids: vec!["decision-generation".to_owned()],
                        task_key: "task-generation".to_owned(),
                        before_generation: 0,
                    },
                )
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn tenant_quota_rejects_at_limit_and_recovers_after_release() {
        let mut state = test_state_with_route(route()).await;
        state.quota = MemoryQuotaRepository::new(1, 0, 0);
        let lease = acquire_tenant_quota(&state, "quota-tenant", 0)
            .await
            .unwrap();
        let Err(error) = acquire_tenant_quota(&state, "quota-tenant", 0).await else {
            panic!("quota reservation should be rejected at the configured limit");
        };
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.code, "tenant_concurrency_exhausted");
        assert_eq!(state.metrics.quota_rejections.load(Ordering::Relaxed), 1);
        lease.release().await.unwrap();
        acquire_tenant_quota(&state, "quota-tenant", 0)
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn chat_completions_returns_stable_error_when_tenant_quota_is_exhausted() {
        let mut state = test_state_with_route(route()).await;
        state.quota = MemoryQuotaRepository::new(1, 0, 0);
        let held_lease = acquire_tenant_quota(&state, &tenant_key("local"), 0)
            .await
            .unwrap();
        let app = app_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "model": "urouter/auto",
                            "messages": [{"role": "user", "content": "hello"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["type"], "urouter_error");
        assert_eq!(error["error"]["code"], "tenant_concurrency_exhausted");
        held_lease.release().await.unwrap();
    }

    #[tokio::test]
    async fn chat_completions_returns_stable_error_when_budget_is_exhausted() {
        let paid_route: RouteConfig = serde_json::from_value(json!({
            "id": "urouter/auto",
            "tiers": [{
                "tier": "efficient",
                "model": "anthropic/claude-sonnet-4-6"
            }]
        }))
        .unwrap();
        let mut state = test_state_with_route(paid_route).await;
        state.budget = MemoryBudgetRepository::new(1, 2_592_000);
        let app = app_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "model": "urouter/auto",
                            "messages": [{"role": "user", "content": "hello"}],
                            "max_tokens": 1
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["type"], "urouter_error");
        assert_eq!(error["error"]["code"], "tenant_budget_exhausted");
    }

    #[tokio::test]
    async fn chat_completions_returns_stable_error_when_tenant_rpm_is_exhausted() {
        let mut state = test_state_with_route(route()).await;
        state.quota = MemoryQuotaRepository::new(0, 1, 0);
        acquire_tenant_quota(&state, &tenant_key("local"), 0)
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
        let app = app_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "model": "urouter/auto",
                            "messages": [{"role": "user", "content": "hello"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["type"], "urouter_error");
        assert_eq!(error["error"]["code"], "tenant_rate_limit_exhausted");
    }

    #[test]
    fn quota_estimate_reserves_retry_input_and_requested_output() {
        let request = json!({
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 12
        });
        let serialized = u64::try_from(serde_json::to_vec(&request).unwrap().len()).unwrap();
        let expected_input = serialized.div_ceil(4);
        assert_eq!(
            estimate_quota_tokens(&request, 4_096, 1),
            (expected_input, expected_input * 2 + 12)
        );
    }

    #[tokio::test]
    async fn chat_completions_returns_stable_error_when_tenant_tpm_is_exhausted() {
        let mut state = test_state_with_route(route()).await;
        state.quota = MemoryQuotaRepository::new(0, 0, 10);
        acquire_tenant_quota(&state, &tenant_key("local"), 10)
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
        let app = app_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "model": "urouter/auto",
                            "messages": [{"role": "user", "content": "hello"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["code"], "tenant_token_limit_exhausted");
    }

    #[tokio::test]
    async fn non_stream_usage_settles_tpm_reservation() {
        async fn completion() -> Json<Value> {
            Json(json!({
                "id": "chatcmpl-quota",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))
        }

        let upstream = Router::new().route("/chat/completions", post(completion));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let mut route = route();
        set_tier_deployments(
            &mut route,
            "efficient",
            vec![deployment(
                "quota-upstream",
                "local-vllm/qwen3.5-4b",
                format!("http://{address}"),
                0,
            )],
        );
        let quota = MemoryQuotaRepository::new(0, 0, 100);
        let mut state = test_state_with_route(route).await;
        state.retry_policy.max_retries = 0;
        state.quota_default_max_output_tokens = 5;
        state.quota = quota.clone();
        let app = app_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "model": "urouter/auto",
                            "messages": [{"role": "user", "content": "hello"}],
                            "max_tokens": 5
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let reservation = quota.reserve(&tenant_key("local"), 98).await.unwrap();
        let quota::QuotaReservation::Granted(permit) = reservation else {
            panic!("actual usage should replace the larger TPM reservation");
        };
        quota.release(permit).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn stream_terminal_usage_settles_tpm_reservation() {
        async fn completion() -> Response {
            Response::new(Body::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                 data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
                 data: [DONE]\n\n",
            ))
        }

        let upstream = Router::new().route("/chat/completions", post(completion));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let mut route = route();
        set_tier_deployments(
            &mut route,
            "efficient",
            vec![deployment(
                "quota-stream-upstream",
                "local-vllm/qwen3.5-4b",
                format!("http://{address}"),
                0,
            )],
        );
        let quota = MemoryQuotaRepository::new(0, 0, 100);
        let mut state = test_state_with_route(route).await;
        state.retry_policy.max_retries = 0;
        state.quota_default_max_output_tokens = 5;
        state.quota = quota.clone();
        let app = app_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "model": "urouter/auto",
                            "messages": [{"role": "user", "content": "hello"}],
                            "max_tokens": 5,
                            "stream": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 1_048_576).await.unwrap();

        let reservation = quota.reserve(&tenant_key("local"), 98).await.unwrap();
        let quota::QuotaReservation::Granted(permit) = reservation else {
            panic!("terminal stream usage should replace the larger TPM reservation");
        };
        quota.release(permit).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
    async fn redis_record_repository_passes_shared_contract() {
        let url = std::env::var("UROUTER_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16380/".to_owned());
        let prefix = format!("urouter-record-contract-{}", next_decision_id());
        let shared = RedisSharedState::connect(&url, prefix).await.unwrap();
        let local = RecordStore::open(None, 4, 1, 0).await.unwrap();
        assert_record_repository_contract(RedisDecisionRecordRepository::new(shared, local, 4))
            .await;
    }

    #[tokio::test]
    async fn retries_on_a_different_deployment_and_opens_failed_primary() {
        async fn primary() -> Response {
            (StatusCode::SERVICE_UNAVAILABLE, "temporary").into_response()
        }
        async fn backup() -> Response {
            Json(json!({"ok": true})).into_response()
        }

        let app = Router::new()
            .route("/primary/chat/completions", post(primary))
            .route("/backup/chat/completions", post(backup));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut route = route();
        set_tier_deployments(
            &mut route,
            "efficient",
            vec![
                deployment(
                    "efficient-primary",
                    "local-vllm/qwen3.5-4b",
                    format!("http://{address}/primary"),
                    0,
                ),
                deployment(
                    "efficient-backup",
                    "local-vllm/qwen3.5-4b",
                    format!("http://{address}/backup"),
                    1,
                ),
            ],
        );
        let mut state = test_state_with_route(route).await;
        state.quota = MemoryQuotaRepository::new(1, 0, 0);
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let execution = execute_test_request(&state, &decision, &request, "req-retry")
            .await
            .unwrap();
        assert_eq!(execution.response.status(), StatusCode::OK);
        assert_eq!(execution.attempts.len(), 2);
        assert_eq!(execution.attempts[0].deployment, "efficient-primary");
        assert_eq!(execution.attempts[1].deployment, "efficient-backup");
        assert!(
            execution.attempts[0]
                .selection_trace
                .iter()
                .any(|candidate| {
                    candidate.deployment == "efficient-backup"
                        && candidate.reasons == ["lower_priority_order"]
                })
        );
        assert!(
            execution.attempts[1]
                .selection_trace
                .iter()
                .any(|candidate| {
                    candidate.deployment == "efficient-primary"
                        && candidate.disposition == DeploymentDisposition::Excluded
                        && candidate
                            .reasons
                            .iter()
                            .any(|reason| reason == "retry_excluded")
                })
        );
        assert_eq!(
            execution.attempts[0].attempt_id.as_deref(),
            Some("req-retry:attempt:1")
        );
        assert_eq!(
            execution.attempts[1].attempt_id.as_deref(),
            Some("req-retry:attempt:2")
        );
        assert!(execution.attempts[0].retry);
        assert_eq!(state.metrics.retries.load(Ordering::Relaxed), 1);
        let health = state.capacity.snapshot();
        assert_eq!(
            health
                .iter()
                .find(|item| item.deployment == "efficient-primary")
                .unwrap()
                .state,
            urouter_gateway::capacity::CircuitState::Open
        );
        server.abort();
    }

    #[tokio::test]
    async fn exhausted_tier_without_attempt_retains_top_level_filter_trace() {
        let mut route = route();
        route.tiers[0].fallbacks.clear();
        route.tiers[0].deployments = vec![
            deployment(
                "cooling-a",
                "local-vllm/qwen3.5-4b",
                "http://127.0.0.1:1/a".to_owned(),
                0,
            ),
            deployment(
                "cooling-b",
                "local-vllm/qwen3.5-4b",
                "http://127.0.0.1:1/b".to_owned(),
                0,
            ),
        ];
        let state = test_state_with_route(route).await;
        let deployments = state.route.tiers[0].effective_deployments();
        state
            .capacity
            .select(&deployments, &BTreeSet::new())
            .unwrap()
            .complete(Err(UpstreamErrorKind::ServerError));
        state
            .capacity
            .select(&deployments, &BTreeSet::new())
            .unwrap()
            .complete(Err(UpstreamErrorKind::ServerError));

        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let result = execute_test_request(&state, &decision, &request, "req-exhausted").await;
        let Err(failure) = result else {
            panic!("cooling deployments must be exhausted before an upstream attempt");
        };
        assert!(failure.attempts.is_empty());
        assert_eq!(failure.runtime_filter_trace.len(), 2);
        assert!(failure.runtime_filter_trace.iter().all(|candidate| {
            candidate.disposition == DeploymentDisposition::Excluded
                && candidate
                    .reasons
                    .iter()
                    .any(|reason| reason == "local_circuit_unavailable")
        }));
    }

    #[tokio::test]
    async fn deployment_policy_filters_emit_complete_machine_reasons() {
        let mut route = route();
        route.tiers.truncate(1);
        route.tiers[0].fallbacks.clear();
        let mut restricted = deployment(
            "restricted",
            "local-vllm/qwen3.5-4b",
            "http://127.0.0.1:1".to_owned(),
            0,
        );
        restricted.enabled = false;
        restricted.credential_available = false;
        restricted.region = Some("cn-east".to_owned());
        restricted.residency = vec!["cn".to_owned()];
        restricted.tenant_allowlist = vec!["tenant-a".to_owned()];
        route.tiers[0].deployments = vec![restricted];
        let state = test_state_with_route(route).await;
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}],
            "urouter": {"policy": {"region": "us-west", "residency": "eu"}}
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let result = execute_routed_upstream(
            &state,
            &decision,
            &request,
            "req-policy",
            &tenant_key("tenant-b"),
        )
        .await;
        let Err(failure) = result else {
            panic!("restricted deployment must be filtered before execution");
        };
        assert!(failure.attempts.is_empty());
        assert_eq!(failure.runtime_filter_trace.len(), 1);
        assert_eq!(
            failure.runtime_filter_trace[0].reasons,
            [
                "deployment_disabled",
                "credential_unavailable",
                "region_mismatch",
                "residency_mismatch",
                "tenant_not_allowed"
            ]
        );
        let counters = state.metrics.filter_rejections.lock().unwrap();
        assert_eq!(counters.get("region_mismatch"), Some(&1));
        assert_eq!(counters.get("tenant_not_allowed"), Some(&1));
    }

    #[tokio::test]
    async fn retired_deployment_only_serves_bound_requests_during_grace() {
        let state = test_state_with_route(route()).await;
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "summarize this document"}]
        });
        let mut decision = state.route.decide(&state.catalog, &request).unwrap();
        let mut retired = deployment(
            "retired",
            "local-vllm/qwen3.5-4b",
            "http://127.0.0.1:1".to_owned(),
            0,
        );
        retired.accept_new_requests = false;
        retired.binding_grace_until_unix = Some(unix_seconds() + 60);

        assert!(
            deployment_filter_reasons(&state, &retired, &decision, "local")
                .contains(&"deployment_retired".to_owned())
        );

        decision.reason = "task_binding".to_owned();
        assert!(
            !deployment_filter_reasons(&state, &retired, &decision, "local")
                .contains(&"deployment_retired".to_owned())
        );

        retired.binding_grace_until_unix = Some(unix_seconds().saturating_sub(1));
        assert!(
            deployment_filter_reasons(&state, &retired, &decision, "local")
                .contains(&"deployment_retired".to_owned())
        );
    }

    #[tokio::test]
    async fn falls_back_to_capable_after_efficient_is_exhausted() {
        async fn unavailable() -> Response {
            (StatusCode::SERVICE_UNAVAILABLE, "temporary").into_response()
        }
        async fn capable() -> Response {
            Json(json!({"ok": true})).into_response()
        }

        let app = Router::new()
            .route("/efficient/chat/completions", post(unavailable))
            .route("/capable/chat/completions", post(capable));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut route = route();
        set_tier_deployments(
            &mut route,
            "efficient",
            vec![deployment(
                "efficient-only",
                "local-vllm/qwen3.5-4b",
                format!("http://{address}/efficient"),
                0,
            )],
        );
        set_tier_deployments(
            &mut route,
            "capable",
            vec![deployment(
                "capable-only",
                "local-vllm-qwen38/qwen3.8-27b",
                format!("http://{address}/capable"),
                0,
            )],
        );
        let mut state = test_state_with_route(route).await;
        state.retry_policy.max_retries = 0;
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let execution = execute_test_request(&state, &decision, &request, "req-fallback")
            .await
            .unwrap();
        assert_eq!(execution.tier, "capable");
        assert_eq!(execution.fallback_depth, 1);
        assert_eq!(execution.attempts.len(), 2);
        assert_eq!(execution.attempts[1].deployment, "capable-only");
        assert_eq!(state.metrics.fallbacks.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn bad_request_does_not_fallback() {
        async fn bad_request() -> Response {
            (StatusCode::BAD_REQUEST, "invalid input").into_response()
        }
        let app = Router::new().route("/efficient/chat/completions", post(bad_request));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut route = route();
        set_tier_deployments(
            &mut route,
            "efficient",
            vec![deployment(
                "bad-request",
                "local-vllm/qwen3.5-4b",
                format!("http://{address}/efficient"),
                0,
            )],
        );
        let state = test_state_with_route(route).await;
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let failure = execute_test_request(&state, &decision, &request, "req-bad-request")
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error.code, "upstream_bad_request");
        assert_eq!(failure.attempts.len(), 1);
        assert_eq!(state.metrics.fallbacks.load(Ordering::Relaxed), 0);
        server.abort();
    }

    #[tokio::test]
    async fn timeout_uses_its_typed_fallback_chain() {
        async fn slow() -> Response {
            sleep(Duration::from_millis(100)).await;
            Json(json!({"unexpected": true})).into_response()
        }
        async fn generic() -> Response {
            Json(json!({"path": "generic"})).into_response()
        }
        async fn timeout_backup() -> Response {
            Json(json!({"path": "timeout"})).into_response()
        }

        let slow_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slow_address = slow_listener.local_addr().unwrap();
        let slow_server = tokio::spawn(async move {
            axum::serve(
                slow_listener,
                Router::new().route("/chat/completions", post(slow)),
            )
            .await
            .unwrap();
        });
        let generic_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generic_address = generic_listener.local_addr().unwrap();
        let generic_server = tokio::spawn(async move {
            axum::serve(
                generic_listener,
                Router::new().route("/chat/completions", post(generic)),
            )
            .await
            .unwrap();
        });
        let timeout_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let timeout_address = timeout_listener.local_addr().unwrap();
        let timeout_server = tokio::spawn(async move {
            axum::serve(
                timeout_listener,
                Router::new().route("/chat/completions", post(timeout_backup)),
            )
            .await
            .unwrap();
        });

        let mut route = route();
        route.tiers[0].deployments = vec![deployment(
            "slow",
            "local-vllm/qwen3.5-4b",
            format!("http://{slow_address}"),
            0,
        )];
        route.tiers[1].deployments = vec![deployment(
            "generic",
            "local-vllm-qwen38/qwen3.8-27b",
            format!("http://{generic_address}"),
            0,
        )];
        let mut timeout_tier = route.tiers[1].clone();
        timeout_tier.tier = "timeout-backup".to_owned();
        timeout_tier.fallbacks.clear();
        timeout_tier.deployments = vec![deployment(
            "timeout-backup",
            "local-vllm-qwen38/qwen3.8-27b",
            format!("http://{timeout_address}"),
            0,
        )];
        route.tiers.push(timeout_tier);
        route.tiers[0]
            .fallbacks_by_error
            .insert(FallbackCause::Timeout, vec!["timeout-backup".to_owned()]);
        let mut state = test_state_with_route(route).await;
        state.request_timeout = Duration::from_millis(25);
        state.retry_policy.max_retries = 0;
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let execution = execute_test_request(&state, &decision, &request, "req-timeout")
            .await
            .unwrap();
        assert_eq!(execution.tier, "timeout-backup");
        assert_eq!(execution.attempts.len(), 2);
        assert_eq!(
            execution.attempts[0].error_kind,
            Some(UpstreamErrorKind::Timeout)
        );
        assert_eq!(execution.attempts[1].deployment, "timeout-backup");

        slow_server.abort();
        generic_server.abort();
        timeout_server.abort();
    }

    #[tokio::test]
    async fn fallback_depth_counts_edges_from_selected_tier() {
        let mut state = test_state_with_route(route()).await;
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        state.max_fallback_depth = 0;
        let tiers = execution_tiers(&state, &decision).unwrap();
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].tier, "efficient");

        state.max_fallback_depth = 1;
        let tiers = execution_tiers(&state, &decision).unwrap();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[1].tier, "capable");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn dropping_gateway_stream_cancels_upstream_body() {
        struct DropFlag(Arc<AtomicU64>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(1, Ordering::Relaxed);
            }
        }

        async fn slow_stream(State(dropped): State<Arc<AtomicU64>>) -> Response {
            let stream = stream::unfold((DropFlag(dropped), 0_u64), |(guard, index)| async move {
                sleep(Duration::from_millis(10)).await;
                Some((
                    Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!(
                        "data: {{\"index\":{index}}}\n\n"
                    ))),
                    (guard, index.saturating_add(1)),
                ))
            });
            Response::new(Body::from_stream(stream))
        }

        let dropped = Arc::new(AtomicU64::new(0));
        let app = Router::new()
            .route("/chat/completions", post(slow_stream))
            .with_state(Arc::clone(&dropped));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut route = route();
        set_tier_deployments(
            &mut route,
            "efficient",
            vec![deployment(
                "slow-stream",
                "local-vllm/qwen3.5-4b",
                format!("http://{address}"),
                0,
            )],
        );
        let mut state = test_state_with_route(route).await;
        state.quota = MemoryQuotaRepository::new(1, 0, 0);
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "stream"}],
            "stream": true
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let execution = execute_test_request(&state, &decision, &request, "req-stream")
            .await
            .unwrap();
        let QuotaAdmission::Granted(quota) =
            QuotaLease::acquire(Arc::clone(&state.quota), "stream-tenant", 0)
                .await
                .unwrap()
        else {
            panic!("initial stream quota should be granted");
        };
        let budget =
            test_budget_accounting(&state, "stream-tenant", "req-stream", &decision, &request)
                .await;
        let response = stream_response(
            state.clone(),
            execution,
            ResponseContext {
                decision_id: "decision-cancel".to_owned(),
                decision,
                governance: test_governance(),
                record: RecordSeed {
                    request_id: "req-stream".to_owned(),
                    messages_hash: messages_hash(&request).unwrap(),
                    features: FeatureFrame::from_openai_chat(&request),
                    route_revision: state.route.revision(),
                    artifact: None,
                    exploration: None,
                    vector_ref: None,
                },
                headers: HeaderMap::new(),
                quota,
                quota_input_tokens: 0,
                budget,
            },
        );
        drop(response);
        for _ in 0..50 {
            if dropped.load(Ordering::Relaxed) == 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        let mut replacement = None;
        for _ in 0..50 {
            replacement = match QuotaLease::acquire(Arc::clone(&state.quota), "stream-tenant", 0)
                .await
                .unwrap()
            {
                QuotaAdmission::Granted(lease) => Some(lease),
                QuotaAdmission::Rejected(_) => None,
            };
            if replacement.is_some() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        replacement.unwrap().release().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn partial_stream_failure_never_runs_success_finalization() {
        async fn broken_stream() -> Response {
            let chunks = stream::unfold(0_u8, |step| async move {
                match step {
                    0 => Some((
                        Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: {\"delta\":1}\n\n")),
                        1,
                    )),
                    1 => {
                        sleep(Duration::from_millis(25)).await;
                        Some((Err(std::io::Error::other("stream interrupted")), 2))
                    }
                    _ => None,
                }
            });
            Response::new(Body::from_stream(chunks))
        }

        let app = Router::new().route("/chat/completions", post(broken_stream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut route = route();
        route.tiers[0].fallbacks.clear();
        set_tier_deployments(
            &mut route,
            "efficient",
            vec![deployment(
                "broken-stream",
                "local-vllm/qwen3.5-4b",
                format!("http://{address}"),
                0,
            )],
        );
        let mut state = test_state_with_route(route).await;
        state.retry_policy.max_retries = 0;
        state.quota = MemoryQuotaRepository::new(1, 0, 0);
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "stream"}],
            "stream": true
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let execution = execute_test_request(&state, &decision, &request, "req-broken")
            .await
            .unwrap();
        let QuotaAdmission::Granted(quota) =
            QuotaLease::acquire(Arc::clone(&state.quota), "broken-tenant", 0)
                .await
                .unwrap()
        else {
            panic!("stream quota should be granted");
        };
        let budget =
            test_budget_accounting(&state, "broken-tenant", "req-broken", &decision, &request)
                .await;
        let response = stream_response(
            state.clone(),
            execution,
            ResponseContext {
                decision_id: "decision-broken".to_owned(),
                decision,
                governance: test_governance(),
                record: RecordSeed {
                    request_id: "req-broken".to_owned(),
                    messages_hash: messages_hash(&request).unwrap(),
                    features: FeatureFrame::from_openai_chat(&request),
                    route_revision: state.route.revision(),
                    artifact: None,
                    exploration: None,
                    vector_ref: None,
                },
                headers: HeaderMap::new(),
                quota,
                quota_input_tokens: 0,
                budget,
            },
        );
        let mut body = response.into_body().into_data_stream();
        while body.next().await.is_some() {}
        assert_eq!(state.metrics.stream_failures.load(Ordering::Relaxed), 1);
        assert_eq!(state.metrics.streams_completed.load(Ordering::Relaxed), 0);
        assert!(state.records.records.read().await.is_empty());
        let replacement = QuotaLease::acquire(Arc::clone(&state.quota), "broken-tenant", 0)
            .await
            .unwrap();
        assert!(matches!(replacement, QuotaAdmission::Granted(_)));
        server.abort();
    }
}
