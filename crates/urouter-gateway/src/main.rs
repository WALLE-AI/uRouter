use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    sync::{RwLock, mpsc, oneshot},
    time::sleep,
};
use urouter_ai::{
    auth::AuthPlan,
    catalog::{CatalogSnapshot, ModelSpec},
    compat::MaxTokensField,
    endpoint::EndpointPlan,
    evidence::CatalogEvidence,
    pricing::{CostBreakdown, PriceSource, calculate_actual_cost},
};
use urouter_gateway::{
    CallRole, DataPolicyContract, MigrationBoundary, RecordingMode, RetryPolicy, RouteConfig,
    RouteDecision, RouteDeployment, RouteError, SignalContract, UpstreamErrorKind,
    capacity::{CapacityError, CapacityLease, CapacityManager, CooldownPolicy},
    circuit::{
        CircuitPermit, LocalCircuitRepository, RedisCircuitRepository, SharedCircuitRepository,
    },
};
use urouter_types::{ModelId, Usage, WireApi};

mod adapter;
mod binding;
mod management_auth;
mod shared_state;

use adapter::adapt_agent_request;
use binding::{
    BindingWrite, MemoryTaskBindingRepository, RedisTaskBindingRepository, TaskBinding,
    TaskBindingRepository,
};
use management_auth::{ManagementAuth, ManagementAuthError, ManagementRole};
use shared_state::RedisSharedState;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Parser)]
#[command(about = "uRouter M0 Auto gateway")]
struct Args {
    #[arg(long, default_value = "catalog/catalog.json")]
    catalog: PathBuf,
    #[arg(long, default_value = "gateway/route.json")]
    route: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    #[arg(long)]
    records: Option<PathBuf>,
    #[arg(long)]
    feedback_records: Option<PathBuf>,
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
    #[arg(long, default_value_t = 10_000)]
    task_binding_capacity: usize,
    #[arg(long)]
    redis_url: Option<String>,
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
}

#[derive(Clone)]
struct AppState {
    catalog: Arc<CatalogSnapshot>,
    route: Arc<RouteConfig>,
    client: reqwest::Client,
    records: RecordStore,
    feedback: FeedbackStore,
    metrics: GatewayMetrics,
    request_timeout: Duration,
    retry_policy: RetryPolicy,
    capacity: Arc<CapacityManager>,
    shared_circuits: Arc<dyn SharedCircuitRepository>,
    max_fallback_depth: u8,
    bindings: Arc<dyn TaskBindingRepository>,
    shared_state: Option<RedisSharedState>,
    require_tenant_header: bool,
    management_auth: ManagementAuth,
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
    route_id: String,
    tier: String,
    reason: String,
    alternatives: Vec<ModelId>,
    evidence: CatalogEvidence,
    requirement: urouter_ai::capabilities::CapabilityRequirement,
    admission: urouter_ai::admission::AdmissionResult,
    execution: ExecutionRecord,
    #[serde(default)]
    override_record: Option<OverrideRecord>,
    #[serde(default)]
    outcome_signals: Vec<FeedbackSignal>,
    #[serde(skip)]
    tenant_generation: u64,
    #[serde(skip)]
    task_generation: u64,
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

#[derive(Clone, Default)]
struct GatewayMetrics {
    requests: Arc<AtomicU64>,
    successes: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
    retries: Arc<AtomicU64>,
    feedback_signals: Arc<AtomicU64>,
    paired: Arc<AtomicU64>,
    paired_rejected: Arc<AtomicU64>,
    streams_completed: Arc<AtomicU64>,
    fallbacks: Arc<AtomicU64>,
    bindings_created: Arc<AtomicU64>,
    bindings_applied: Arc<AtomicU64>,
    binding_migrations: Arc<AtomicU64>,
    compatibility_requests: Arc<AtomicU64>,
    binding_conflicts: Arc<AtomicU64>,
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
}

const fn bool_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttemptRecord {
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
}

#[derive(Clone)]
struct ExecutionTier {
    tier: String,
    deployments: Vec<RouteDeployment>,
}

struct TierSuccess {
    response: reqwest::Response,
    lease: ExecutionLease,
    model: ModelSpec,
}

struct ExecutionLease {
    local: Option<CapacityLease>,
    deployment: RouteDeployment,
    shared: Arc<dyn SharedCircuitRepository>,
    permit: Option<CircuitPermit>,
    tier_size: usize,
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
    decision_id: Option<String>,
}

impl GatewayError {
    fn bad_request(code: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: error.to_string(),
            decision_id: None,
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: error.to_string(),
            decision_id: None,
        }
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
        Self::bad_request("route_rejected", error)
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
            decision_id: None,
        }
    }
}

fn state_backend_unavailable(_error: impl std::fmt::Display) -> GatewayError {
    GatewayError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "state_backend_unavailable",
        message: "the shared state backend is temporarily unavailable".to_owned(),
        decision_id: None,
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let args = Args::parse();
    validate_args(&args)?;
    let catalog = Arc::new(CatalogSnapshot::from_json_str(&fs::read_to_string(
        &args.catalog,
    )?)?);
    let route: RouteConfig = serde_json::from_str(&fs::read_to_string(&args.route)?)?;
    route.validate(&catalog)?;
    let bindings = build_binding_repository(&args).await?;
    let cooldown_policy = configured_cooldown_policy(&args);
    let shared_circuits = build_circuit_repository(&args, cooldown_policy).await?;
    let shared_state = build_shared_state(&args).await?;
    let management_auth = ManagementAuth::open(
        args.management_keyring.clone(),
        args.management_audit.clone(),
        args.management_audit_queue_capacity,
        Duration::from_secs(args.management_keyring_reload_seconds),
    )
    .await?;
    let feedback_path = args.feedback_records.or_else(|| {
        args.records
            .as_ref()
            .map(|path| PathBuf::from(format!("{}.feedback", path.display())))
    });
    let records = RecordStore::open(
        args.records,
        args.record_capacity,
        args.record_queue_capacity,
        args.record_max_bytes,
    )
    .await?;
    let feedback_store = FeedbackStore::open(
        feedback_path,
        args.record_queue_capacity,
        args.record_max_bytes,
    )
    .await?;
    reconcile_feedback(&records, &feedback_store).await;
    let capacity = CapacityManager::new(cooldown_policy);
    for tier in &route.tiers {
        capacity.register(&tier.effective_deployments());
    }
    let state = AppState {
        catalog,
        route: Arc::new(route),
        client: reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(args.connect_timeout_ms))
            .build()?,
        records,
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
        shared_state,
        require_tenant_header: args.require_tenant_header,
        management_auth,
    };
    spawn_retention_sweeper(state.records.clone());
    let app = app_router(state);
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    println!("urouter-gateway listening on http://{}", args.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/explain", post(explain))
        .route("/v1/chat/completions", post(chat_completions))
        .route(
            "/v1/adapters/{harness}/chat/completions",
            post(adapter_chat_completions),
        )
        .route("/v1/decisions", get(decisions))
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
        || args.management_audit_queue_capacity == 0
    {
        return Err(
            "record, queue, task binding, and audit capacities must be greater than zero".into(),
        );
    }
    if args.cooldown_failure_threshold_millis > 1_000 {
        return Err("cooldown threshold must be <= 1000".into());
    }
    if args.task_binding_ttl_seconds == 0 {
        return Err("task binding TTL must be greater than zero".into());
    }
    if args.management_keyring_reload_seconds == 0 {
        return Err("management keyring reload interval must be greater than zero".into());
    }
    if args.redis_url.is_some() && (args.records.is_some() || args.feedback_records.is_some()) {
        return Err(
            "--records/--feedback-records cannot be combined with Redis authoritative state".into(),
        );
    }
    Ok(())
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

async fn tiers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, GatewayError> {
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

async fn metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let tenant_key = resolve_tenant_key(&state, &headers)?;
    authorize_management(
        &state,
        &headers,
        &tenant_key,
        ManagementRole::Admin,
        "metrics.read",
        None,
    )
    .await?;
    let metric = &state.metrics;
    let resident_memory_bytes = process_resident_memory_bytes().await.unwrap_or(0);
    let body = format!(
        concat!(
            "# TYPE urouter_requests_total counter\n",
            "urouter_requests_total {}\n",
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
        metric.successes.load(Ordering::Relaxed),
        metric.errors.load(Ordering::Relaxed),
        metric.retries.load(Ordering::Relaxed),
        metric.feedback_signals.load(Ordering::Relaxed),
        metric.paired.load(Ordering::Relaxed),
        metric.paired_rejected.load(Ordering::Relaxed),
        metric.streams_completed.load(Ordering::Relaxed),
        metric.fallbacks.load(Ordering::Relaxed),
        metric.bindings_created.load(Ordering::Relaxed),
        metric.bindings_applied.load(Ordering::Relaxed),
        metric.binding_migrations.load(Ordering::Relaxed),
        metric.compatibility_requests.load(Ordering::Relaxed),
        metric.binding_conflicts.load(Ordering::Relaxed),
        state.records.dropped.load(Ordering::Relaxed),
        state.records.write_errors.load(Ordering::Relaxed),
        state.feedback.dropped.load(Ordering::Relaxed),
        state.feedback.write_errors.load(Ordering::Relaxed),
        resident_memory_bytes,
    );
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    Ok(response)
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

async fn explain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let (tenant_key, tenant_compatibility) = resolve_tenant(&state, &headers)?;
    let mut decision = state.route.decide(&state.catalog, &request)?;
    decision.compatibility_mode |= tenant_compatibility;
    validate_decision_scope(&decision)?;
    let governance = request_governance(&state, tenant_key, &decision).await?;
    apply_task_binding_inner(&state, &governance, &mut decision, false).await?;
    let retained = governance.policy.recording != RecordingMode::None;
    Ok(Json(json!({
        "route_id": decision.route_id,
        "tier": decision.tier,
        "model": decision.model,
        "reason": decision.reason,
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
    let turn = record_for_id(&state, &tenant_key, &id)
        .await?
        .and_then(|record| record.trace_turn);
    let shared_deleted = if let Some(shared) = &state.shared_state {
        shared
            .delete_records(&tenant_key, std::slice::from_ref(&id))
            .await
            .map_err(state_backend_unavailable)?
    } else {
        0
    };
    let local_deleted = state
        .records
        .delete_matching(|record| record.tenant_key == tenant_key && record.decision_id == id)
        .await?;
    let deleted = shared_deleted.max(local_deleted);
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
    let shared_deleted = if let Some(shared) = &state.shared_state {
        shared
            .delete_records(&tenant_key, &decision_ids)
            .await
            .map_err(state_backend_unavailable)?
    } else {
        0
    };
    let local_deleted = state
        .records
        .delete_matching(|record| {
            record.tenant_key == tenant_key
                && record.task_key.as_ref() == Some(&task_key)
                && record.task_generation < current_task_generation
        })
        .await?;
    let deleted = shared_deleted.max(local_deleted);
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
    let shared_deleted = if let Some(shared) = &state.shared_state {
        shared
            .delete_records(&tenant_key, &decision_ids)
            .await
            .map_err(state_backend_unavailable)?
    } else {
        0
    };
    let local_deleted = state
        .records
        .delete_matching(|record| {
            record.tenant_key == tenant_key && record.tenant_generation < current_tenant_generation
        })
        .await?;
    let deleted = shared_deleted.max(local_deleted);
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
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let (tenant_key, tenant_compatibility) = resolve_tenant(&state, &headers)?;
    let result =
        chat_completions_inner(state.clone(), request, tenant_key, tenant_compatibility).await;
    if result.is_ok() {
        state.metrics.successes.fetch_add(1, Ordering::Relaxed);
    } else {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
    }
    result
}

async fn adapter_chat_completions(
    State(state): State<AppState>,
    Path(harness): Path<String>,
    headers: HeaderMap,
    Json(mut request): Json<Value>,
) -> Result<Response, GatewayError> {
    adapt_agent_request(&harness, &headers, &mut request)
        .map_err(|error| GatewayError::bad_request("invalid_agent_adapter", error))?;
    chat_completions(State(state), headers, Json(request)).await
}

async fn chat_completions_inner(
    state: AppState,
    request: Value,
    tenant_key: String,
    tenant_compatibility: bool,
) -> Result<Response, GatewayError> {
    let mut decision = state.route.decide(&state.catalog, &request)?;
    decision.compatibility_mode |= tenant_compatibility;
    validate_decision_scope(&decision)?;
    let governance = request_governance(&state, tenant_key, &decision).await?;
    observe_compatibility(&state, decision.compatibility_mode);
    apply_task_binding(&state, &governance, &mut decision).await?;
    ingest_piggyback(&state, &governance, &decision.signals).await?;
    let messages_hash = messages_hash(&request)?;
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let started = Instant::now();
    let execution = match execute_routed_upstream(&state, &decision, &request).await {
        Ok(execution) => execution,
        Err(mut failure) => {
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
            decision.tier.clone_from(&failure.tier);
            decision.model = failure.model.id.clone();
            if failure.fallback_depth > 0 {
                "fallback_degraded".clone_into(&mut decision.reason);
            }
            let deployment = failure
                .attempts
                .last()
                .map(|attempt| attempt.deployment.clone())
                .unwrap_or_default();
            let record = build_record(
                &state.catalog,
                decision_id.clone(),
                decision,
                &failure.model,
                messages_hash,
                &governance,
                ExecutionRecord {
                    ok: false,
                    stream,
                    upstream_status,
                    upstream_latency_ms: started.elapsed().as_millis(),
                    usage: None,
                    cost: None,
                    usage_unavailable: true,
                    attempts: failure.attempts,
                    error_kind,
                    deployment,
                    fallback_depth: failure.fallback_depth,
                },
            )?;
            store_record(&state, record).await?;
            failure.error.decision_id = Some(decision_id);
            return Err(failure.error);
        }
    };
    if execution.tier != decision.tier {
        decision.tier.clone_from(&execution.tier);
        decision.model = execution.model.id.clone();
        "fallback_degraded".clone_into(&mut decision.reason);
    }
    let decision_id = next_decision_id();
    let headers = decision_headers(&decision_id, &decision)?;
    if stream {
        Ok(stream_response(
            state,
            execution,
            decision_id,
            decision,
            messages_hash,
            governance,
            headers,
        ))
    } else {
        non_stream_response(
            state,
            execution,
            decision_id,
            decision,
            messages_hash,
            governance,
            headers,
        )
        .await
    }
}

fn observe_compatibility(state: &AppState, compatibility_mode: bool) {
    if compatibility_mode {
        state
            .metrics
            .compatibility_requests
            .fetch_add(1, Ordering::Relaxed);
    }
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
    if let Some(shared) = &state.shared_state {
        let mut records = shared
            .records(tenant_key)
            .await
            .map_err(state_backend_unavailable)?;
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
        return Ok(records);
    }
    state.records.prune_expired().await?;
    Ok(state
        .records
        .records
        .read()
        .await
        .iter()
        .filter(|record| record.tenant_key == tenant_key)
        .cloned()
        .collect())
}

async fn record_for_id(
    state: &AppState,
    tenant_key: &str,
    decision_id: &str,
) -> Result<Option<DecisionRecord>, GatewayError> {
    if let Some(shared) = &state.shared_state {
        let Some(mut record) = shared
            .record(tenant_key, decision_id)
            .await
            .map_err(state_backend_unavailable)?
        else {
            return Ok(None);
        };
        if let Some(turn) = &record.trace_turn
            && let Some(signals) = shared
                .feedback(tenant_key, turn)
                .await
                .map_err(state_backend_unavailable)?
        {
            record.outcome_signals = signals;
        }
        return Ok(Some(record));
    }
    state.records.prune_expired().await?;
    Ok(state
        .records
        .records
        .read()
        .await
        .iter()
        .find(|record| record.decision_id == decision_id && record.tenant_key == tenant_key)
        .cloned())
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

async fn execute_routed_upstream(
    state: &AppState,
    decision: &RouteDecision,
    request: &Value,
) -> Result<UpstreamExecution, RoutedFailure> {
    let tiers = execution_tiers(state, decision)
        .map_err(|error| routed_setup_failure(state, decision, error))?;
    let started = Instant::now();
    let mut attempts = Vec::new();
    let mut last_error = None;
    let mut last_model = state
        .catalog
        .model(&decision.model)
        .expect("route decision model exists")
        .clone();
    let mut last_tier = decision.tier.clone();
    let mut last_depth = 0_u8;

    for (depth, tier) in tiers.iter().enumerate() {
        let depth = u8::try_from(depth).unwrap_or(u8::MAX);
        if depth > 0 {
            state.metrics.fallbacks.fetch_add(1, Ordering::Relaxed);
        }
        last_depth = depth;
        last_tier.clone_from(&tier.tier);
        match execute_tier(state, tier, decision, request, &mut attempts).await {
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
                    });
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
            decision_id: None,
        }),
        attempts,
        tier: last_tier,
        model: last_model,
        fallback_depth: last_depth,
    })
}

async fn execute_tier(
    state: &AppState,
    tier: &ExecutionTier,
    decision: &RouteDecision,
    request: &Value,
    attempts: &mut Vec<AttemptRecord>,
) -> Result<TierSuccess, TierExhausted> {
    let candidates = tier
        .deployments
        .iter()
        .filter(|deployment| decision.admission.eligible.contains(&deployment.model))
        .cloned()
        .collect::<Vec<_>>();
    let fallback_model = candidates
        .first()
        .and_then(|deployment| state.catalog.model(&deployment.model))
        .cloned()
        .unwrap_or_else(|| state.catalog.model(&decision.model).unwrap().clone());
    let mut last_model = fallback_model;
    let mut last_error = None;
    let mut excluded = BTreeSet::new();
    let mut retries_used = 0_u8;

    loop {
        let Some(lease) =
            acquire_tier_lease(state, &candidates, &mut excluded, &last_model).await?
        else {
            break;
        };
        let deployment = lease.deployment.clone();
        let model = state.catalog.model(&deployment.model).unwrap().clone();
        last_model.clone_from(&model);
        let (endpoint, url) =
            endpoint_for_deployment(state, &deployment, &model).map_err(|error| TierExhausted {
                error: Some(error),
                model: model.clone(),
            })?;
        let headers = resolve_headers(&endpoint).map_err(|error| TierExhausted {
            error: Some(error),
            model: model.clone(),
        })?;
        let mut upstream_request = request.clone();
        rewrite_request(&mut upstream_request, &model);

        match send_deployment_request(state, &url, &headers, &upstream_request).await {
            Ok((response, latency_ms)) => {
                attempts.push(AttemptRecord {
                    attempt: attempt_number(attempts.len()),
                    tier: tier.tier.clone(),
                    deployment: deployment.id,
                    model: Some(model.id.clone()),
                    status: Some(response.status().as_u16()),
                    latency_ms,
                    error_kind: None,
                    retry: false,
                });
                return Ok(TierSuccess {
                    response,
                    lease,
                    model,
                });
            }
            Err(failure) => {
                if let Err(error) = lease.complete(Err(failure.kind)).await {
                    return Err(TierExhausted {
                        error: Some(error),
                        model,
                    });
                }
                let retry = state.retry_policy.should_retry(failure.kind, retries_used);
                attempts.push(AttemptRecord {
                    attempt: attempt_number(attempts.len()),
                    tier: tier.tier.clone(),
                    deployment: deployment.id.clone(),
                    model: Some(model.id.clone()),
                    status: failure.status.map(|status| status.as_u16()),
                    latency_ms: failure.latency_ms,
                    error_kind: Some(failure.kind),
                    retry,
                });
                last_error = Some(upstream_error(
                    failure.kind,
                    failure.status,
                    failure.detail,
                    attempts,
                ));
                if failure.kind == UpstreamErrorKind::BadRequest || !retry {
                    break;
                }
                state.metrics.retries.fetch_add(1, Ordering::Relaxed);
                retries_used = retries_used.saturating_add(1);
                excluded.insert(deployment.id);
                if candidates.len() == 1 {
                    excluded.clear();
                    sleep(Duration::from_millis(state.retry_policy.backoff_ms(
                        retries_used.saturating_sub(1),
                        failure.retry_after_ms,
                    )))
                    .await;
                }
            }
        }
    }
    Err(TierExhausted {
        error: last_error,
        model: last_model,
    })
}

async fn acquire_tier_lease(
    state: &AppState,
    candidates: &[RouteDeployment],
    excluded: &mut BTreeSet<String>,
    model: &ModelSpec,
) -> Result<Option<ExecutionLease>, TierExhausted> {
    acquire_execution_lease(state, candidates, excluded)
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
) -> Result<Option<ExecutionLease>, GatewayError> {
    loop {
        let local = match state.capacity.select(candidates, excluded) {
            Ok(lease) => lease,
            Err(CapacityError::Exhausted) => return Ok(None),
            Err(error) => return Err(GatewayError::internal(error)),
        };
        let deployment_id = local.deployment.id.clone();
        let deployment = local.deployment.clone();
        match state
            .shared_circuits
            .acquire(&local.deployment)
            .await
            .map_err(state_backend_unavailable)?
        {
            Some(permit) => {
                return Ok(Some(ExecutionLease {
                    local: Some(local),
                    deployment,
                    shared: Arc::clone(&state.shared_circuits),
                    permit: Some(permit),
                    tier_size: candidates.len(),
                }));
            }
            None => {
                excluded.insert(deployment_id);
            }
        }
    }
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
            }],
        }]);
    }
    let mut result = Vec::new();
    let mut visited = BTreeSet::new();
    append_execution_tier(
        &state.route,
        &decision.tier,
        state.max_fallback_depth,
        &mut visited,
        &mut result,
    )?;
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

fn append_execution_tier(
    route: &RouteConfig,
    tier_name: &str,
    remaining: u8,
    visited: &mut BTreeSet<String>,
    result: &mut Vec<ExecutionTier>,
) -> Result<(), GatewayError> {
    if !visited.insert(tier_name.to_owned()) {
        return Ok(());
    }
    let tier = route
        .tiers
        .iter()
        .find(|tier| tier.tier == tier_name)
        .ok_or_else(|| GatewayError::internal(format!("selected tier {tier_name} disappeared")))?;
    result.push(ExecutionTier {
        tier: tier.tier.clone(),
        deployments: tier.effective_deployments(),
    });
    if remaining == 0 {
        return Ok(());
    }
    for fallback in &tier.fallbacks {
        append_execution_tier(route, fallback, remaining - 1, visited, result)?;
    }
    Ok(())
}

fn endpoint_for_deployment(
    state: &AppState,
    deployment: &RouteDeployment,
    model: &ModelSpec,
) -> Result<(EndpointPlan, String), GatewayError> {
    if model.api != WireApi::OpenAiChat {
        return Err(GatewayError::bad_request(
            "unsupported_wire_api",
            "Gateway currently requires open_ai_chat",
        ));
    }
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
    let url = format!(
        "{}/chat/completions",
        endpoint.url.as_str().trim_end_matches('/')
    );
    Ok((endpoint, url))
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

fn resolve_headers(endpoint: &EndpointPlan) -> Result<BTreeMap<String, String>, GatewayError> {
    let mut headers = endpoint.public_headers.clone();
    if let AuthPlan::ApiKeyEnv {
        env: variable,
        header,
        prefix,
    } = &endpoint.auth
    {
        let secret = env::var(variable).map_err(|_| {
            GatewayError::internal(format!(
                "credential environment variable {variable} is not set"
            ))
        })?;
        headers.insert(header.clone(), format!("{prefix}{secret}"));
    }
    Ok(headers)
}

async fn non_stream_response(
    state: AppState,
    execution: UpstreamExecution,
    decision_id: String,
    decision: RouteDecision,
    messages_hash: String,
    governance: RequestGovernance,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let UpstreamExecution {
        response,
        elapsed_ms,
        attempts,
        lease,
        model,
        fallback_depth,
        ..
    } = execution;
    let deployment = lease.deployment.id.clone();
    let mut body: Value = match response.json().await {
        Ok(body) => body,
        Err(error) => {
            lease.complete(Err(UpstreamErrorKind::ServerError)).await?;
            return Err(GatewayError::internal(error));
        }
    };
    lease.complete(Ok(())).await?;
    let usage = parse_usage(&body);
    let cost = calculate_cost(&model, usage)?;
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
        messages_hash,
        &governance,
        ExecutionRecord {
            ok: true,
            stream: false,
            upstream_status: 200,
            upstream_latency_ms: elapsed_ms,
            usage,
            cost,
            usage_unavailable: usage.is_none(),
            attempts,
            error_kind: None,
            deployment,
            fallback_depth,
        },
    )?;
    store_record(&state, record).await?;
    let mut response = Json(body).into_response();
    *response.headers_mut() = headers;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

fn stream_response(
    state: AppState,
    execution: UpstreamExecution,
    decision_id: String,
    decision: RouteDecision,
    messages_hash: String,
    governance: RequestGovernance,
    headers: HeaderMap,
) -> Response {
    const TAIL_BYTES: usize = 64;
    let UpstreamExecution {
        response,
        elapsed_ms,
        attempts,
        lease,
        model,
        fallback_depth,
        ..
    } = execution;
    let deployment = lease.deployment.id.clone();
    let lease = Arc::new(StdMutex::new(Some(lease)));
    let stream_lease = Arc::clone(&lease);
    let captured = Arc::new(StdMutex::new(StreamCapture::default()));
    let capture = Arc::clone(&captured);
    let upstream_stream = response.bytes_stream().map(move |item| {
        if item.is_err()
            && let Some(lease) = stream_lease
                .lock()
                .expect("capacity lease lock poisoned")
                .take()
        {
            tokio::spawn(async move {
                let _ = lease.complete(Err(UpstreamErrorKind::Transport)).await;
            });
        }
        item.map(|bytes| {
            let mut capture = capture.lock().expect("stream capture lock poisoned");
            capture.complete.extend_from_slice(bytes.as_ref());
            capture.tail.extend_from_slice(bytes.as_ref());
            let emit = capture.tail.len().saturating_sub(TAIL_BYTES);
            Bytes::from(capture.tail.drain(..emit).collect::<Vec<_>>())
        })
        .map_err(|error| Box::new(error) as BoxError)
    });
    let final_stream = stream::once(async move {
        let completed_lease = lease.lock().expect("capacity lease lock poisoned").take();
        if let Some(lease) = completed_lease {
            lease
                .complete(Ok(()))
                .await
                .map_err(|error| Box::new(std::io::Error::other(error.message)) as BoxError)?;
        }
        let (complete, tail) = {
            let capture = captured.lock().expect("stream capture lock poisoned");
            (capture.complete.clone(), capture.tail.clone())
        };
        let usage = parse_stream_usage(&complete);
        let cost = calculate_cost(&model, usage).ok().flatten();
        commit_task_binding(&state, &governance, &decision, &decision.tier, &model.id)
            .await
            .map_err(|error| Box::new(std::io::Error::other(error.message)) as BoxError)?;
        let disclosure = disclosure(&decision_id, &decision, &model, cost.clone());
        let record = build_record(
            &state.catalog,
            decision_id,
            decision,
            &model,
            messages_hash,
            &governance,
            ExecutionRecord {
                ok: true,
                stream: true,
                upstream_status: 200,
                upstream_latency_ms: elapsed_ms,
                usage,
                cost,
                usage_unavailable: usage.is_none(),
                attempts,
                error_kind: None,
                deployment,
                fallback_depth,
            },
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
        let serialized = serde_json::to_string(&disclosure).unwrap_or_else(|_| "{}".to_owned());
        let mut suffix = remove_sse_done(tail);
        suffix.extend_from_slice(
            format!("\nevent: urouter.decision\ndata: {serialized}\n\ndata: [DONE]\n\n").as_bytes(),
        );
        Ok::<Bytes, BoxError>(Bytes::from(suffix))
    });
    let mut response = Response::new(Body::from_stream(upstream_stream.chain(final_stream)));
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
    messages_hash: String,
    governance: &RequestGovernance,
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expires_at_unix_s = now
        .saturating_add(u64::from(governance.policy.retention_days).saturating_mul(24 * 60 * 60));
    let retained = governance.policy.recording != RecordingMode::None;
    Ok(DecisionRecord {
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
        messages_hash,
        route_id: decision.route_id,
        tier: decision.tier,
        reason: decision.reason,
        alternatives: decision.alternatives,
        evidence,
        requirement: decision.requirement,
        admission: decision.admission,
        execution,
        override_record: None,
        outcome_signals: Vec::new(),
        tenant_generation: governance.tenant_generation,
        task_generation: governance.task_generation,
    })
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
    decision_id: &str,
    decision: &RouteDecision,
) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
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
                if let Ok(mut record) = serde_json::from_str::<DecisionRecord>(line) {
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

fn spawn_retention_sweeper(records: RecordStore) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(60)).await;
            let _ = records.prune_expired().await;
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
    if let Some(shared) = &state.shared_state {
        shared
            .put_record(&record, state.records.capacity)
            .await
            .map_err(state_backend_unavailable)?;
    }
    state.records.append(record).await;
    Ok(())
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

fn next_decision_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("dec_{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let decision = route().decide(&catalog, request).unwrap();
        let model = catalog.model(&decision.model).unwrap();
        build_record(
            &catalog,
            decision_id.to_owned(),
            decision,
            model,
            messages_hash(request).unwrap(),
            &test_governance(),
            successful_execution(),
        )
        .unwrap()
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
        AppState {
            catalog,
            route: Arc::new(route),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            records: RecordStore::open(None, 10, 1, 0).await.unwrap(),
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
            shared_state: None,
            require_tenant_header: false,
            management_auth: ManagementAuth::disabled(),
        }
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
        let mut state_b = test_state_with_route(route()).await;
        state_b.shared_state = Some(RedisSharedState::connect(&url, prefix).await.unwrap());
        let tenant = tenant_key("shared-state-tenant");
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
        let record = build_record(
            catalog,
            "decision-private".to_owned(),
            decision,
            model,
            messages_hash(&request).unwrap(),
            &governance,
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
        let state = test_state_with_route(route).await;
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let execution = execute_routed_upstream(&state, &decision, &request)
            .await
            .unwrap();
        assert_eq!(execution.response.status(), StatusCode::OK);
        assert_eq!(execution.attempts.len(), 2);
        assert_eq!(execution.attempts[0].deployment, "efficient-primary");
        assert_eq!(execution.attempts[1].deployment, "efficient-backup");
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
        let execution = execute_routed_upstream(&state, &decision, &request)
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
        let failure = execute_routed_upstream(&state, &decision, &request)
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error.code, "upstream_bad_request");
        assert_eq!(failure.attempts.len(), 1);
        assert_eq!(state.metrics.fallbacks.load(Ordering::Relaxed), 0);
        server.abort();
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
        let state = test_state_with_route(route).await;
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "stream"}],
            "stream": true
        });
        let decision = state.route.decide(&state.catalog, &request).unwrap();
        let execution = execute_routed_upstream(&state, &decision, &request)
            .await
            .unwrap();
        let response = stream_response(
            state,
            execution,
            "decision-cancel".to_owned(),
            decision,
            messages_hash(&request).unwrap(),
            test_governance(),
            HeaderMap::new(),
        );
        drop(response);
        for _ in 0..50 {
            if dropped.load(Ordering::Relaxed) == 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        server.abort();
    }
}
