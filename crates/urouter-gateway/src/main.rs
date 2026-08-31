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
mod dry_run;
mod idempotency;
mod logging;
mod management_auth;
mod persistence;
mod protocol_translation;
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
use dry_run::{
    DryRunStatus, budget_boundary_message, budget_boundary_status, configuration_dry_run,
};
use idempotency::{
    IdempotencyClaim, IdempotencyRepository, MemoryIdempotencyRepository,
    RedisIdempotencyRepository,
};
use logging::LogFormat;
use management_auth::{ManagementAuth, ManagementAuthError, ManagementRole};
use persistence::{
    feedback_writer, normalize_replayed_record, record_expired, record_writer,
    spawn_retention_sweeper, store_record, tombstone_record_vectors,
};
use protocol_translation::{translate_chat_response, translate_chat_stream_response};
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
    /// Explicit HTTP/HTTPS proxy for upstream provider calls. Ambient
    /// `http_proxy`/`https_proxy` are always ignored so that a loopback or
    /// in-cluster deployment is never silently re-routed; an egress proxy must
    /// be opted into here.
    #[arg(long)]
    upstream_proxy: Option<String>,
    /// Hosts that bypass `--upstream-proxy`, as a comma-separated list. Matches
    /// reqwest's `NO_PROXY` syntax.
    #[arg(long)]
    upstream_no_proxy: Option<String>,
    /// Log wire format: `json` for machine ingestion, `text` for local runs.
    #[arg(long, default_value_t = LogFormat::Json)]
    log_format: LogFormat,
    /// Default tracing filter directive. `RUST_LOG` overrides it when set.
    #[arg(long, default_value = "info")]
    log_level: String,
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
        // Every error response passes through here, so this is the one place
        // that guarantees a rejected request is visible in the logs. Server
        // faults are errors; client faults are warnings the operator can filter
        // out. The message is the stable, already-redacted client-facing text.
        // `into_response` runs after the request span has closed, so the
        // identifiers are attached explicitly rather than inherited.
        let request_id = self.request_id.as_deref().unwrap_or("-");
        let decision_id = self.decision_id.as_deref().unwrap_or("-");
        if self.status.is_server_error() {
            tracing::error!(
                status = self.status.as_u16(),
                code = self.code,
                detail = %self.message,
                request_id,
                decision_id,
                "request rejected"
            );
        } else {
            tracing::warn!(
                status = self.status.as_u16(),
                code = self.code,
                detail = %self.message,
                request_id,
                decision_id,
                "request rejected"
            );
        }
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

/// The client-facing 503 is deliberately opaque, so the cause is only ever
/// visible here. Without this event a Redis outage is indistinguishable from any
/// other 503 in the logs.
fn state_backend_unavailable(error: impl std::fmt::Display) -> GatewayError {
    tracing::error!(error = %error, "shared state backend is unavailable");
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
    logging::install(args.log_format, &args.log_level)?;
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
    let client = upstream_client(&args)?;
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
    tracing::info!(
        bind = %args.bind,
        catalog_revision = %state.control.snapshot().revision,
        shared_state = state.shared_state.is_some(),
        upstream_proxy = args.upstream_proxy.is_some(),
        management_rbac = args.management_keyring.is_some(),
        shutdown_grace_seconds = args.shutdown_grace_seconds,
        "urouter-gateway listening"
    );
    serve_with_drain(
        listener,
        app,
        Arc::clone(&state.accepting),
        args.shutdown_grace_seconds,
    )
    .await?;
    Ok(())
}

/// Builds the upstream HTTP client.
///
/// Ambient `http_proxy`/`https_proxy` are always ignored: a Gateway routinely
/// addresses loopback and in-cluster deployments, and an inherited proxy would
/// silently re-route or stall those calls. An egress proxy is opted into with
/// `--upstream-proxy`, optionally narrowed by `--upstream-no-proxy`.
fn upstream_client(args: &Args) -> Result<reqwest::Client, BoxError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_millis(args.connect_timeout_ms));
    if let Some(url) = &args.upstream_proxy {
        let mut proxy = reqwest::Proxy::all(url)?;
        if let Some(exceptions) = &args.upstream_no_proxy {
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(exceptions));
        }
        builder = builder.proxy(proxy);
    }
    Ok(builder.build()?)
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
    logging::validate_level(&args.log_level)?;
    if args.upstream_no_proxy.is_some() && args.upstream_proxy.is_none() {
        return Err("upstream no-proxy exceptions require --upstream-proxy".into());
    }
    if let Some(url) = &args.upstream_proxy {
        // Fail at startup rather than on the first upstream call.
        reqwest::Proxy::all(url).map_err(|error| format!("invalid upstream proxy: {error}"))?;
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
                    let revision = candidate.revision.clone();
                    let changed = control.snapshot().revision != revision;
                    control.publish(candidate);
                    if changed {
                        tracing::info!(revision = %revision, "control revision published");
                    }
                }
                Err(error) => {
                    // A rejected manifest keeps the last-good snapshot serving,
                    // so this is the only signal that reloads have stalled.
                    tracing::error!(error = %error, "control manifest reload rejected");
                    control.reject(error);
                }
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

/// The request span is the correlation root: every event emitted while routing,
/// retrying or falling back inherits `request_id` and `tenant_key`, which are the
/// same identifiers the `DecisionRecord` and the trace exemplar carry. Request and
/// response bodies are never fields.
#[tracing::instrument(
    name = "chat",
    skip_all,
    fields(
        request_id = %request_id,
        tenant_key = %tenant_key,
        decision_id = tracing::field::Empty,
        tier = tracing::field::Empty,
        model = tracing::field::Empty,
        stream = tracing::field::Empty,
    )
)]
async fn chat_completions_inner(
    state: AppState,
    request: Value,
    tenant_key: String,
    tenant_compatibility: bool,
    request_id: String,
) -> Result<Response, GatewayError> {
    let RoutedDecision {
        mut decision,
        feature_frame,
        route_revision,
        artifact,
        exploration,
    } = route_request(
        &state,
        &request,
        &tenant_key,
        tenant_compatibility,
        &request_id,
    )?;
    let span = tracing::Span::current();
    trace_decision(&span, &decision, &route_revision);
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
    adopt_served_tier(&mut decision, &execution);
    let decision_id = next_decision_id();
    trace_successful_execution(&span, &decision, &decision_id, &execution, stream, started);
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

/// The pre-execution routing outcome: the rule cascade, then the signed
/// artifact policy, then controlled exploration, all against one Route revision.
struct RoutedDecision {
    decision: RouteDecision,
    feature_frame: FeatureFrame,
    route_revision: String,
    artifact: Option<ArtifactDecision>,
    exploration: Option<ExplorationRecord>,
}

fn route_request(
    state: &AppState,
    request: &Value,
    tenant_key: &str,
    tenant_compatibility: bool,
    request_id: &str,
) -> Result<RoutedDecision, GatewayError> {
    let feature_frame = FeatureFrame::from_openai_chat(request);
    let route_revision = state.route.revision();
    let mut decision = state.route.decide(&state.catalog, request)?;
    let artifact = apply_artifact_policy(
        state,
        request,
        tenant_key,
        &artifact_task_key(&decision, request_id),
        &mut decision,
    )?;
    let exploration = apply_controlled_exploration(
        state,
        request,
        tenant_key,
        &artifact_task_key(&decision, request_id),
        tenant_compatibility,
        &mut decision,
    )?;
    decision.compatibility_mode |= tenant_compatibility;
    validate_decision_scope(&decision)?;
    Ok(RoutedDecision {
        decision,
        feature_frame,
        route_revision,
        artifact,
        exploration,
    })
}

/// Publishes the pre-execution decision onto the request span. Recorded before
/// admission so that a request rejected by quota or budget still carries the
/// tier and model it would have used.
fn trace_decision(span: &tracing::Span, decision: &RouteDecision, route_revision: &str) {
    span.record("tier", decision.tier.as_str());
    span.record("model", decision.model.as_str());
    tracing::debug!(
        reason = %decision.reason,
        compatibility_mode = decision.compatibility_mode,
        route_revision,
        "route decided"
    );
}

/// Rewrites the decision to the tier that actually served the request. The
/// client-facing disclosure and the `DecisionRecord` must both name the served
/// tier, not the originally selected one.
fn adopt_served_tier(decision: &mut RouteDecision, execution: &UpstreamExecution) {
    if execution.tier == decision.tier {
        return;
    }
    tracing::info!(
        from_tier = %decision.tier,
        to_tier = %execution.tier,
        to_model = %execution.model.id,
        "request completed on a fallback tier"
    );
    decision.tier.clone_from(&execution.tier);
    decision.model = execution.model.id.clone();
    "fallback_degraded".clone_into(&mut decision.reason);
}

/// Completes the request span once the tier and model are final, so that a
/// fallback-degraded request is attributed to the tier that actually served it
/// rather than the one it was first routed to.
fn trace_successful_execution(
    span: &tracing::Span,
    decision: &RouteDecision,
    decision_id: &str,
    execution: &UpstreamExecution,
    stream: bool,
    started: Instant,
) {
    span.record("decision_id", decision_id);
    span.record("stream", stream);
    span.record("tier", decision.tier.as_str());
    span.record("model", decision.model.as_str());
    tracing::info!(
        deployment = %execution.lease.deployment.id,
        attempts = execution.attempts.len(),
        elapsed_ms = duration_metric_value(started.elapsed().as_millis()),
        "upstream succeeded"
    );
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
    tracing::warn!(
        decision_id = %decision_id,
        tier = %failure.tier,
        model = %failure.model.id,
        deployment = %deployment,
        attempts = failure.attempts.len(),
        fallback_depth = failure.fallback_depth,
        error_kind = ?error_kind,
        upstream_status,
        code = failure.error.code,
        elapsed_ms = duration_metric_value(elapsed_ms),
        "request failed after exhausting the routing plan"
    );
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
        tracing::debug!(deployment = deployment_id, "retry stopped by directive");
        return false;
    }
    state.metrics.retries.fetch_add(1, Ordering::Relaxed);
    *retries_used = retries_used.saturating_add(1);
    match directive {
        RetryDirective::Stop => unreachable!("stop returned before retry execution"),
        RetryDirective::ReselectDeployment => {
            tracing::info!(
                deployment = deployment_id,
                retries_used = *retries_used,
                "retrying on a different deployment"
            );
            excluded.insert(deployment_id.to_owned());
        }
        RetryDirective::RetrySameDeployment { backoff_ms } => {
            tracing::info!(
                deployment = deployment_id,
                retries_used = *retries_used,
                backoff_ms,
                "retrying the same deployment after backoff"
            );
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
            let latency_ms = started.elapsed().as_millis();
            // The upstream body is deliberately not a log field: it is provider
            // text of unbounded size and unknown sensitivity. It stays on the
            // DecisionRecord, which is governed by the tenant data policy.
            tracing::warn!(
                url,
                status = status.as_u16(),
                kind = ?kind,
                retry_after_ms,
                latency_ms = duration_metric_value(latency_ms),
                "upstream returned an error status"
            );
            let detail = response
                .text()
                .await
                .unwrap_or_else(|_| "upstream response unavailable".to_owned());
            Err(AttemptFailure {
                kind,
                status: Some(status),
                detail,
                retry_after_ms,
                latency_ms,
            })
        }
        Err(error) => {
            let kind = if error.is_timeout() {
                UpstreamErrorKind::Timeout
            } else {
                UpstreamErrorKind::Transport
            };
            let latency_ms = started.elapsed().as_millis();
            tracing::warn!(
                url,
                kind = ?kind,
                latency_ms = duration_metric_value(latency_ms),
                "upstream request failed"
            );
            Err(AttemptFailure {
                kind,
                status: error.status(),
                detail: error.to_string(),
                retry_after_ms: None,
                latency_ms,
            })
        }
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
mod tests;
