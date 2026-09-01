use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use clap::Subcommand;
use reqwest::{Client, RequestBuilder, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use urouter_ai::catalog::{CatalogManifest, CatalogSnapshot, ModelSpec, ProviderSpec};
use urouter_types::WireApi;

const DEFAULT_REGISTRY: &str = "catalog/providers/instances.json";
const DEFAULT_STATE_DIR: &str = "catalog/providers/state";
const DEFAULT_CATALOG: &str = "catalog/catalog.json";
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

type BoxError = Box<dyn std::error::Error>;

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// List configured provider instances.
    List {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
    },
    /// Validate provider instance configuration without reading credentials.
    Check {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    /// Fetch one provider inventory and commit a last-good immutable snapshot.
    Discover {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
        #[arg(long)]
        instance: String,
        #[arg(long, default_value = DEFAULT_STATE_DIR)]
        state_dir: PathBuf,
        #[arg(long, default_value_t = 30)]
        timeout_seconds: u64,
    },
    /// Compare the latest discovered inventory with the published catalog.
    Status {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
        #[arg(long)]
        instance: String,
        #[arg(long, default_value = DEFAULT_STATE_DIR)]
        state_dir: PathBuf,
        #[arg(long, default_value = DEFAULT_CATALOG)]
        catalog: PathBuf,
    },
    /// Build a quarantined candidate from discovery plus reviewed field evidence.
    Candidate {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
        #[arg(long)]
        instance: String,
        #[arg(long, default_value = DEFAULT_STATE_DIR)]
        state_dir: PathBuf,
        #[arg(long)]
        reviews: Option<PathBuf>,
        #[arg(long, default_value = "catalog/providers/candidates")]
        output_dir: PathBuf,
        #[arg(long, default_value = DEFAULT_CATALOG)]
        catalog: PathBuf,
    },
    /// Run bounded capability probes and retain only hashes and pass/fail evidence.
    Probe {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
        #[arg(long)]
        instance: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        budget_nano_usd: u64,
        #[arg(long)]
        estimated_request_nano_usd: u64,
        #[arg(long, default_value_t = 128)]
        max_output_tokens: u64,
        #[arg(long, default_value_t = 30)]
        timeout_seconds: u64,
        #[arg(long, default_value = "catalog/providers/probes")]
        output_dir: PathBuf,
    },
    /// Atomically publish a complete reviewed candidate; control manifest commits last.
    Publish {
        #[arg(long)]
        candidate: PathBuf,
        #[arg(long, default_value = DEFAULT_CATALOG)]
        catalog: PathBuf,
        #[arg(long, default_value = "catalog/manifest.json")]
        catalog_manifest: PathBuf,
        #[arg(long, default_value = "gateway/route.json")]
        route: PathBuf,
        #[arg(long, default_value = "gateway/control-manifest.json")]
        control_manifest: PathBuf,
        #[arg(long)]
        signing_key_env: Option<String>,
    },
    /// Restore the previous atomically published Catalog/control bundle.
    Rollback {
        #[arg(long, default_value = DEFAULT_CATALOG)]
        catalog: PathBuf,
        #[arg(long, default_value = "catalog/manifest.json")]
        catalog_manifest: PathBuf,
        #[arg(long, default_value = "gateway/control-manifest.json")]
        control_manifest: PathBuf,
    },
    /// Bootstrap a control manifest for the current validated Catalog and Route.
    ControlManifest {
        #[arg(long, default_value = DEFAULT_CATALOG)]
        catalog: PathBuf,
        #[arg(long, default_value = "gateway/route.json")]
        route: PathBuf,
        #[arg(long, default_value = "gateway/control-manifest.json")]
        output: PathBuf,
        #[arg(long)]
        signing_key_env: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderRegistry {
    schema_version: u16,
    instances: Vec<ProviderInstance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderInstance {
    id: String,
    vendor: String,
    catalog_provider_id: String,
    runtime: RuntimeConfig,
    discovery: DiscoveryConfig,
    credential_scope: String,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

const fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeConfig {
    base_url: String,
    protocols: Vec<WireApi>,
    #[serde(default)]
    auth_env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DiscoveryConfig {
    OpenAiModels {
        url: String,
        #[serde(default)]
        auth_env: Option<String>,
        #[serde(default)]
        query: BTreeMap<String, String>,
        #[serde(default)]
        min_request_interval_ms: u64,
    },
    BailianCatalog {
        url: String,
        auth_env: String,
        #[serde(default = "default_page_size")]
        page_size: u32,
        #[serde(default = "default_max_pages")]
        max_pages: u32,
        #[serde(default)]
        query: BTreeMap<String, String>,
        #[serde(default)]
        min_request_interval_ms: u64,
    },
    ReviewedStatic {
        path: PathBuf,
    },
}

const fn default_page_size() -> u32 {
    100
}

const fn default_max_pages() -> u32 {
    100
}

impl DiscoveryConfig {
    const fn driver_name(&self) -> &'static str {
        match self {
            Self::OpenAiModels { .. } => "open_ai_models",
            Self::BailianCatalog { .. } => "bailian_catalog",
            Self::ReviewedStatic { .. } => "reviewed_static",
        }
    }

    fn source_template(&self) -> String {
        match self {
            Self::OpenAiModels { url, .. } | Self::BailianCatalog { url, .. } => url.clone(),
            Self::ReviewedStatic { path } => path.display().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProviderModel {
    upstream_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    raw: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventorySnapshot {
    schema_version: u16,
    instance_id: String,
    vendor: String,
    driver: String,
    source: String,
    first_fetched_at_unix: u64,
    complete: bool,
    content_hash: String,
    models: Vec<RawProviderModel>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LatestSnapshot {
    schema_version: u16,
    instance_id: String,
    content_hash: String,
    snapshot: String,
    last_fetched_at_unix: u64,
    models: usize,
}

#[derive(Debug, Serialize)]
struct InstanceSummary<'a> {
    id: &'a str,
    vendor: &'a str,
    catalog_provider_id: &'a str,
    driver: &'static str,
    enabled: bool,
    protocols: &'a [WireApi],
    credential_scope: &'a str,
}

#[derive(Debug, Serialize)]
struct DiscoveryOutput {
    instance_id: String,
    vendor: String,
    driver: String,
    models: usize,
    content_hash: String,
    snapshot: PathBuf,
    changed: bool,
}

#[derive(Debug, Serialize)]
struct InventoryStatus {
    instance_id: String,
    content_hash: String,
    discovered: usize,
    cataloged: usize,
    new_upstream_ids: Vec<String>,
    missing_upstream_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewDocument {
    schema_version: u16,
    instance_id: String,
    #[serde(default)]
    provider: Option<ProviderSpec>,
    #[serde(default)]
    models: Vec<ReviewedProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewedProjection {
    upstream_id: String,
    model: ModelSpec,
    evidence: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateModel {
    upstream_id: String,
    raw_hash: String,
    reviewed: Option<ReviewedProjection>,
    unresolved_fields: Vec<String>,
    quarantine_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateBundle {
    schema_version: u16,
    instance_id: String,
    inventory_hash: String,
    candidate_hash: String,
    complete: bool,
    provider: Option<ProviderSpec>,
    models: Vec<CandidateModel>,
}

#[derive(Debug, Serialize)]
struct ProbeEvidence {
    schema_version: u16,
    instance_id: String,
    upstream_id: String,
    checked_at_unix: u64,
    budget_nano_usd: u64,
    estimated_spend_nano_usd: u64,
    response_content_retained: bool,
    probes: Vec<ProbeResult>,
}

#[derive(Debug, Serialize)]
struct ProbeResult {
    capability: &'static str,
    passed: bool,
    status: u16,
    latency_ms: u128,
    evidence_hash: String,
}

#[derive(Debug, Serialize)]
struct PublishedControlManifest {
    schema_version: u16,
    revision: String,
    catalog_sha256: String,
    route_sha256: String,
    signature: Option<String>,
}

struct LoadedRegistry {
    registry: ProviderRegistry,
    base_dir: PathBuf,
}

struct DiscoveryContext<'a> {
    client: &'a Client,
    instance: &'a ProviderInstance,
    base_dir: &'a Path,
}

#[async_trait]
trait ProviderDriver: Send + Sync {
    fn kind(&self) -> &'static str;

    async fn discover(
        &self,
        context: &DiscoveryContext<'_>,
    ) -> Result<Vec<RawProviderModel>, SyncError>;
}

struct OpenAiModelsDriver;
struct BailianCatalogDriver;
struct ReviewedStaticDriver;

#[derive(Debug, Error)]
enum SyncError {
    #[error("provider registry schema_version must be 1")]
    UnsupportedRegistrySchema,
    #[error("provider registry contains duplicate instance: {0}")]
    DuplicateInstance(String),
    #[error("provider instance not found: {0}")]
    InstanceNotFound(String),
    #[error("provider instance is disabled: {0}")]
    InstanceDisabled(String),
    #[error("invalid provider instance {instance}: {message}")]
    InvalidInstance { instance: String, message: String },
    #[error("credential environment variable {0} is not set")]
    MissingCredential(String),
    #[error("endpoint environment variable {0} is not set")]
    MissingEndpointVariable(String),
    #[error("invalid endpoint template: {0}")]
    InvalidEndpointTemplate(String),
    #[error("provider returned HTTP {status}: {body}")]
    Http { status: StatusCode, body: String },
    #[error("provider response exceeded {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    #[error("provider response is invalid: {0}")]
    InvalidResponse(String),
    #[error("provider inventory contains conflicting duplicate model ID: {0}")]
    ConflictingModel(String),
    #[error("provider pagination exceeded configured maximum of {0} pages")]
    PaginationLimit(u32),
    #[error("latest snapshot does not exist for instance {0}")]
    SnapshotNotFound(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
}

pub fn run_provider(command: ProviderCommand, json: bool) -> Result<(), BoxError> {
    match command {
        ProviderCommand::List { registry } => {
            let loaded = load_registry(&registry)?;
            let summaries = loaded
                .registry
                .instances
                .iter()
                .map(|instance| InstanceSummary {
                    id: &instance.id,
                    vendor: &instance.vendor,
                    catalog_provider_id: &instance.catalog_provider_id,
                    driver: instance.discovery.driver_name(),
                    enabled: instance.enabled,
                    protocols: &instance.runtime.protocols,
                    credential_scope: &instance.credential_scope,
                })
                .collect::<Vec<_>>();
            if json {
                println!("{}", serde_json::to_string(&summaries)?);
            } else {
                for summary in summaries {
                    println!(
                        "{} vendor={} driver={} enabled={} protocols={}",
                        summary.id,
                        summary.vendor,
                        summary.driver,
                        summary.enabled,
                        summary
                            .protocols
                            .iter()
                            .map(wire_api_name)
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                }
            }
        }
        ProviderCommand::Check { registry } => {
            let loaded = load_registry(&registry)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "valid": true,
                        "instances": loaded.registry.instances.len()
                    })
                );
            } else {
                println!(
                    "valid: {} provider instance(s)",
                    loaded.registry.instances.len()
                );
            }
        }
    }
    Ok(())
}

pub async fn run_sync(command: SyncCommand, json: bool) -> Result<(), BoxError> {
    match command {
        SyncCommand::Discover {
            registry,
            instance,
            state_dir,
            timeout_seconds,
        } => {
            run_discover(&registry, &instance, &state_dir, timeout_seconds, json).await?;
        }
        SyncCommand::Status {
            registry,
            instance,
            state_dir,
            catalog,
        } => run_status(&registry, &instance, &state_dir, &catalog, json)?,
        SyncCommand::Candidate {
            registry,
            instance,
            state_dir,
            reviews,
            output_dir,
            catalog,
        } => run_candidate(
            &registry,
            &instance,
            &state_dir,
            reviews.as_deref(),
            &output_dir,
            &catalog,
            json,
        )?,
        SyncCommand::Probe {
            registry,
            instance,
            model,
            budget_nano_usd,
            estimated_request_nano_usd,
            max_output_tokens,
            timeout_seconds,
            output_dir,
        } => {
            run_probe(
                &registry,
                &instance,
                &model,
                budget_nano_usd,
                estimated_request_nano_usd,
                max_output_tokens,
                timeout_seconds,
                &output_dir,
                json,
            )
            .await?;
        }
        SyncCommand::Publish {
            candidate,
            catalog,
            catalog_manifest,
            route,
            control_manifest,
            signing_key_env,
        } => publish_candidate(
            &candidate,
            &catalog,
            &catalog_manifest,
            &route,
            &control_manifest,
            signing_key_env.as_deref(),
            json,
        )?,
        SyncCommand::Rollback {
            catalog,
            catalog_manifest,
            control_manifest,
        } => rollback_bundle(&catalog, &catalog_manifest, &control_manifest, json)?,
        SyncCommand::ControlManifest {
            catalog,
            route,
            output,
            signing_key_env,
        } => write_control_manifest(&catalog, &route, &output, signing_key_env.as_deref(), json)?,
    }
    Ok(())
}

async fn run_discover(
    registry_path: &Path,
    instance_id: &str,
    state_dir: &Path,
    timeout_seconds: u64,
    json: bool,
) -> Result<(), BoxError> {
    if timeout_seconds == 0 {
        return Err("timeout_seconds must be greater than zero".into());
    }
    let loaded = load_registry(registry_path)?;
    let instance = find_instance(&loaded.registry, instance_id)?;
    if !instance.enabled {
        return Err(SyncError::InstanceDisabled(instance.id.clone()).into());
    }
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(timeout_seconds))
        .build()?;
    let context = DiscoveryContext {
        client: &client,
        instance,
        base_dir: &loaded.base_dir,
    };
    let driver = driver_for(&instance.discovery);
    let models = normalize_models(driver.discover(&context).await?)?;
    let fetched_at = unix_timestamp()?;
    let content_hash = hash_models(&models)?;
    let previous_hash = read_latest(state_dir, &instance.id)
        .ok()
        .map(|latest| latest.content_hash);
    let snapshot = InventorySnapshot {
        schema_version: 1,
        instance_id: instance.id.clone(),
        vendor: instance.vendor.clone(),
        driver: driver.kind().to_owned(),
        source: instance.discovery.source_template(),
        first_fetched_at_unix: fetched_at,
        complete: true,
        content_hash: content_hash.clone(),
        models,
    };
    let snapshot_path = persist_snapshot(state_dir, &snapshot, fetched_at)?;
    let output = DiscoveryOutput {
        instance_id: instance.id.clone(),
        vendor: instance.vendor.clone(),
        driver: driver.kind().to_owned(),
        models: snapshot.models.len(),
        content_hash: content_hash.clone(),
        snapshot: snapshot_path,
        changed: previous_hash.as_deref() != Some(&content_hash),
    };
    print_discovery_output(&output, json)?;
    Ok(())
}

fn print_discovery_output(output: &DiscoveryOutput, json: bool) -> Result<(), serde_json::Error> {
    if json {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!(
            "discovered: {} model(s), {}, changed={}",
            output.models, output.content_hash, output.changed
        );
        println!("snapshot: {}", output.snapshot.display());
    }
    Ok(())
}

fn run_status(
    registry_path: &Path,
    instance_id: &str,
    state_dir: &Path,
    catalog_path: &Path,
    json: bool,
) -> Result<(), BoxError> {
    let loaded = load_registry(registry_path)?;
    let instance = find_instance(&loaded.registry, instance_id)?;
    let latest = read_latest(state_dir, &instance.id)?;
    let snapshot_relative = Path::new(&latest.snapshot);
    if snapshot_relative.is_absolute()
        || snapshot_relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || snapshot_relative.parent() != Some(Path::new("snapshots"))
    {
        return Err(SyncError::InvalidResponse(
            "latest snapshot path is outside the instance snapshot directory".to_owned(),
        )
        .into());
    }
    let snapshot_path = state_dir.join(&instance.id).join(snapshot_relative);
    let snapshot: InventorySnapshot = serde_json::from_slice(&fs::read(&snapshot_path)?)?;
    let recomputed_hash = hash_models(&snapshot.models)?;
    if snapshot.instance_id != instance.id
        || snapshot.content_hash != latest.content_hash
        || snapshot.content_hash != recomputed_hash
        || !snapshot.complete
    {
        return Err(SyncError::InvalidResponse(
            "latest snapshot pointer failed integrity validation".to_owned(),
        )
        .into());
    }
    let catalog = CatalogSnapshot::from_json_str(&fs::read_to_string(catalog_path)?)?;
    let discovered = snapshot
        .models
        .iter()
        .map(|model| model.upstream_id.clone())
        .collect::<BTreeSet<_>>();
    let cataloged = catalog
        .models()
        .filter(|model| model.provider.as_str() == instance.catalog_provider_id)
        .map(|model| model.upstream_id.clone())
        .collect::<BTreeSet<_>>();
    let status = InventoryStatus {
        instance_id: instance.id.clone(),
        content_hash: latest.content_hash,
        discovered: discovered.len(),
        cataloged: cataloged.len(),
        new_upstream_ids: discovered.difference(&cataloged).cloned().collect(),
        missing_upstream_ids: cataloged.difference(&discovered).cloned().collect(),
    };
    print_status(&status, json)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_candidate(
    registry_path: &Path,
    instance_id: &str,
    state_dir: &Path,
    reviews_path: Option<&Path>,
    output_dir: &Path,
    catalog_path: &Path,
    json: bool,
) -> Result<(), BoxError> {
    let loaded = load_registry(registry_path)?;
    let instance = find_instance(&loaded.registry, instance_id)?;
    let snapshot = load_latest_inventory(state_dir, instance_id)?;
    let catalog = CatalogSnapshot::from_json_str(&fs::read_to_string(catalog_path)?)?;
    let reviews = if let Some(path) = reviews_path {
        serde_json::from_slice::<ReviewDocument>(&fs::read(path)?)?
    } else {
        ReviewDocument {
            schema_version: 1,
            instance_id: instance_id.to_owned(),
            provider: None,
            models: Vec::new(),
        }
    };
    if reviews.schema_version != 1 || reviews.instance_id != instance_id {
        return Err(SyncError::InvalidResponse(
            "review document schema or instance does not match".to_owned(),
        )
        .into());
    }
    let mut review_by_upstream = BTreeMap::new();
    for review in reviews.models {
        if review_by_upstream
            .insert(review.upstream_id.clone(), review)
            .is_some()
        {
            return Err(SyncError::InvalidResponse(
                "review document contains duplicate upstream IDs".to_owned(),
            )
            .into());
        }
    }
    let provider_available = catalog
        .providers()
        .any(|provider| provider.id.as_str() == instance.catalog_provider_id)
        || reviews
            .provider
            .as_ref()
            .is_some_and(|provider| provider.id.as_str() == instance.catalog_provider_id);
    let mut models = Vec::with_capacity(snapshot.models.len());
    for raw in &snapshot.models {
        let reviewed = review_by_upstream.remove(&raw.upstream_id);
        let (unresolved_fields, quarantine_reasons) =
            candidate_validation(instance, &catalog, &raw.upstream_id, reviewed.as_ref());
        models.push(CandidateModel {
            upstream_id: raw.upstream_id.clone(),
            raw_hash: format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(raw)?)),
            reviewed,
            unresolved_fields,
            quarantine_reasons,
        });
    }
    if !review_by_upstream.is_empty() {
        return Err(SyncError::InvalidResponse(
            "review document references models absent from the inventory".to_owned(),
        )
        .into());
    }
    let complete = provider_available
        && models
            .iter()
            .all(|model| model.unresolved_fields.is_empty() && model.quarantine_reasons.is_empty());
    let candidate_hash =
        hash_candidate(&snapshot.content_hash, reviews.provider.as_ref(), &models)?;
    let bundle = CandidateBundle {
        schema_version: 1,
        instance_id: instance_id.to_owned(),
        inventory_hash: snapshot.content_hash,
        candidate_hash: candidate_hash.clone(),
        complete,
        provider: reviews.provider,
        models,
    };
    persist_candidate(output_dir, &bundle, json)?;
    Ok(())
}

fn persist_candidate(
    output_dir: &Path,
    bundle: &CandidateBundle,
    json_output: bool,
) -> Result<(), BoxError> {
    let instance_id = &bundle.instance_id;
    let instance_dir = output_dir.join(instance_id);
    let hash = bundle
        .candidate_hash
        .strip_prefix("sha256:")
        .unwrap_or(&bundle.candidate_hash);
    let path = instance_dir.join(format!("{hash}.json"));
    if !path.exists() {
        write_new_json(&path, &bundle)?;
    }
    replace_latest_json(
        &instance_dir.join("latest.json"),
        &json!({
            "schema_version": 1,
            "instance_id": instance_id,
            "candidate_hash": bundle.candidate_hash,
            "candidate": path.file_name().unwrap_or_default().to_string_lossy(),
            "complete": bundle.complete
        }),
    )?;
    if json_output {
        println!("{}", serde_json::to_string(&bundle)?);
    } else {
        println!(
            "candidate={} complete={} reviewed={}/{} path={}",
            bundle.candidate_hash,
            bundle.complete,
            bundle
                .models
                .iter()
                .filter(|model| model.reviewed.is_some())
                .count(),
            bundle.models.len(),
            path.display()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_probe(
    registry_path: &Path,
    instance_id: &str,
    model: &str,
    budget_nano_usd: u64,
    estimated_request_nano_usd: u64,
    max_output_tokens: u64,
    timeout_seconds: u64,
    output_dir: &Path,
    json_output: bool,
) -> Result<(), BoxError> {
    const PROBE_COUNT: u64 = 4;
    if estimated_request_nano_usd == 0 || max_output_tokens == 0 || timeout_seconds == 0 {
        return Err(
            "probe estimate, max output tokens, and timeout must be greater than zero".into(),
        );
    }
    let estimated_spend = estimated_request_nano_usd
        .checked_mul(PROBE_COUNT)
        .ok_or("probe cost estimate overflow")?;
    if estimated_spend > budget_nano_usd {
        return Err("probe plan exceeds the explicit cost budget".into());
    }
    let loaded = load_registry(registry_path)?;
    let instance = find_instance(&loaded.registry, instance_id)?;
    let base_url = expand_env_template(&instance.runtime.base_url)?;
    let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let credential = instance
        .runtime
        .auth_env
        .as_ref()
        .map(|variable| {
            env::var(variable).map_err(|_| SyncError::MissingCredential(variable.clone()))
        })
        .transpose()?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(timeout_seconds))
        .build()?;
    let payloads = probe_payloads(model, max_output_tokens);
    let mut probes = Vec::with_capacity(payloads.len());
    for (capability, payload) in payloads {
        probes.push(
            execute_probe(
                &client,
                &endpoint,
                credential.as_deref(),
                capability,
                &payload,
            )
            .await?,
        );
    }
    let evidence = ProbeEvidence {
        schema_version: 1,
        instance_id: instance_id.to_owned(),
        upstream_id: model.to_owned(),
        checked_at_unix: unix_timestamp()?,
        budget_nano_usd,
        estimated_spend_nano_usd: estimated_spend,
        response_content_retained: false,
        probes,
    };
    let encoded = serde_json::to_vec(&evidence)?;
    let evidence_hash = format!("sha256:{:x}", Sha256::digest(&encoded));
    let hash = evidence_hash
        .strip_prefix("sha256:")
        .unwrap_or(&evidence_hash);
    let path = output_dir.join(instance_id).join(format!("{hash}.json"));
    if !path.exists() {
        write_new_json(&path, &evidence)?;
    }
    if json_output {
        println!("{}", serde_json::to_string(&evidence)?);
    } else {
        println!(
            "probe={} passed={}/{} estimated_spend_nano_usd={} path={}",
            evidence_hash,
            evidence.probes.iter().filter(|probe| probe.passed).count(),
            evidence.probes.len(),
            estimated_spend,
            path.display()
        );
    }
    if evidence.probes.iter().any(|probe| !probe.passed) {
        return Err("one or more capability probes failed".into());
    }
    Ok(())
}

fn probe_payloads(model: &str, max_tokens: u64) -> [(&'static str, Value); 4] {
    [
        (
            "text",
            json!({"model": model, "messages": [{"role": "user", "content": "Reply OK"}], "max_tokens": max_tokens, "temperature": 0}),
        ),
        (
            "tool_calling",
            json!({"model": model, "messages": [{"role": "user", "content": "Call probe_tool"}], "tools": [{"type": "function", "function": {"name": "probe_tool", "parameters": {"type": "object", "properties": {}, "additionalProperties": false}}}], "tool_choice": "required", "max_tokens": max_tokens, "temperature": 0}),
        ),
        (
            "structured_output",
            json!({"model": model, "messages": [{"role": "user", "content": "Return ok=true"}], "response_format": {"type": "json_object"}, "max_tokens": max_tokens, "temperature": 0}),
        ),
        (
            "reasoning",
            json!({"model": model, "messages": [{"role": "user", "content": "What is 17*19?"}], "reasoning_effort": "low", "max_tokens": max_tokens, "temperature": 0}),
        ),
    ]
}

async fn execute_probe(
    client: &Client,
    endpoint: &str,
    credential: Option<&str>,
    capability: &'static str,
    payload: &Value,
) -> Result<ProbeResult, SyncError> {
    let started = std::time::Instant::now();
    let mut request = client.post(endpoint).json(payload);
    if let Some(credential) = credential {
        request = request.bearer_auth(credential);
    }
    let response = request.send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(SyncError::ResponseTooLarge);
    }
    let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    let passed = status.is_success() && probe_response_matches(capability, &body);
    Ok(ProbeResult {
        capability,
        passed,
        status: status.as_u16(),
        latency_ms: started.elapsed().as_millis(),
        evidence_hash: format!("sha256:{:x}", Sha256::digest(&bytes)),
    })
}

fn probe_response_matches(capability: &str, body: &Value) -> bool {
    let message = body.pointer("/choices/0/message");
    match capability {
        "text" => message
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .is_some_and(|content| !content.is_empty()),
        "tool_calling" => message
            .and_then(|message| message.get("tool_calls"))
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty()),
        "structured_output" => message
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .is_some_and(|content| serde_json::from_str::<Value>(content).is_ok()),
        "reasoning" => message.is_some_and(|message| {
            ["reasoning", "reasoning_content"].iter().any(|field| {
                message
                    .get(*field)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            })
        }),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_candidate(
    candidate_path: &Path,
    catalog_path: &Path,
    catalog_manifest_path: &Path,
    route_path: &Path,
    control_manifest_path: &Path,
    signing_key_env: Option<&str>,
    json_output: bool,
) -> Result<(), BoxError> {
    let candidate: CandidateBundle = serde_json::from_slice(&fs::read(candidate_path)?)?;
    if candidate.schema_version != 1
        || candidate.candidate_hash
            != hash_candidate(
                &candidate.inventory_hash,
                candidate.provider.as_ref(),
                &candidate.models,
            )?
        || !candidate.complete
        || candidate.models.iter().any(|model| {
            model.reviewed.is_none()
                || !model.unresolved_fields.is_empty()
                || !model.quarantine_reasons.is_empty()
        })
    {
        return Err("candidate is incomplete, quarantined, or failed integrity validation".into());
    }
    let current_source = fs::read(catalog_path)?;
    let current = CatalogSnapshot::from_json_str(std::str::from_utf8(&current_source)?)?;
    let mut document = current.document();
    if let Some(provider) = candidate.provider {
        match document
            .providers
            .iter_mut()
            .find(|existing| existing.id == provider.id)
        {
            Some(existing) => *existing = provider,
            None => document.providers.push(provider),
        }
    }
    for reviewed in candidate
        .models
        .into_iter()
        .filter_map(|model| model.reviewed)
    {
        match document
            .models
            .iter_mut()
            .find(|existing| existing.id == reviewed.model.id)
        {
            Some(existing) => *existing = reviewed.model,
            None => document.models.push(reviewed.model),
        }
    }
    let validated = CatalogSnapshot::from_document(document)?;
    let mut catalog_source = serde_json::to_vec_pretty(&validated.document())?;
    catalog_source.push(b'\n');
    let catalog_manifest = CatalogManifest::from_source(&catalog_source, &validated);
    let mut catalog_manifest_source = serde_json::to_vec_pretty(&catalog_manifest)?;
    catalog_manifest_source.push(b'\n');
    let route_source = fs::read(route_path)?;
    let control = published_control_manifest(&catalog_source, &route_source, signing_key_env)?;
    let mut control_source = serde_json::to_vec_pretty(&control)?;
    control_source.push(b'\n');
    atomic_publish_bundle([
        (catalog_path, catalog_source.as_slice()),
        (catalog_manifest_path, catalog_manifest_source.as_slice()),
        (control_manifest_path, control_source.as_slice()),
    ])?;
    if json_output {
        println!(
            "{}",
            json!({"published": true, "revision": control.revision, "catalog_hash": validated.hashes().content})
        );
    } else {
        println!(
            "published revision={} catalog={}",
            control.revision,
            validated.hashes().content
        );
    }
    Ok(())
}

fn published_control_manifest(
    catalog: &[u8],
    route: &[u8],
    signing_key_env: Option<&str>,
) -> Result<PublishedControlManifest, BoxError> {
    let catalog_sha256 = digest_prefixed(catalog);
    let route_sha256 = digest_prefixed(route);
    let revision = digest_prefixed(format!("{catalog_sha256}\n{route_sha256}").as_bytes());
    let payload = format!("1\n{revision}\n{catalog_sha256}\n{route_sha256}");
    let signature = signing_key_env
        .map(|variable| {
            let key = env::var(variable)
                .map_err(|_| SyncError::MissingCredential(variable.to_owned()))?;
            if key.is_empty() {
                return Err(SyncError::MissingCredential(variable.to_owned()));
            }
            Ok(hmac_sha256(key.as_bytes(), payload.as_bytes()))
        })
        .transpose()?;
    Ok(PublishedControlManifest {
        schema_version: 1,
        revision,
        catalog_sha256,
        route_sha256,
        signature,
    })
}

fn atomic_publish_bundle(files: [(&Path, &[u8]); 3]) -> Result<(), SyncError> {
    let staged = files
        .iter()
        .map(|(path, bytes)| stage_file(path, bytes))
        .collect::<Result<Vec<_>, _>>()?;
    for (path, _) in &files {
        let previous = previous_path(path);
        if path.exists() {
            fs::copy(path, previous)?;
        }
    }
    for ((path, _), staged_path) in files.iter().zip(&staged) {
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(staged_path, path)?;
    }
    Ok(())
}

fn stage_file(path: &Path, bytes: &[u8]) -> Result<PathBuf, SyncError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!(
        ".{}.{}.publish.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    if staged.exists() {
        fs::remove_file(&staged)?;
    }
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&staged)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(staged)
}

fn rollback_bundle(
    catalog: &Path,
    catalog_manifest: &Path,
    control_manifest: &Path,
    json_output: bool,
) -> Result<(), BoxError> {
    let paths = [catalog, catalog_manifest, control_manifest];
    let previous = paths
        .iter()
        .map(|path| fs::read(previous_path(path)))
        .collect::<Result<Vec<_>, _>>()?;
    atomic_publish_bundle([
        (catalog, previous[0].as_slice()),
        (catalog_manifest, previous[1].as_slice()),
        (control_manifest, previous[2].as_slice()),
    ])?;
    if json_output {
        println!("{}", json!({"rolled_back": true}));
    } else {
        println!("rolled back previous Catalog/control bundle");
    }
    Ok(())
}

fn write_control_manifest(
    catalog_path: &Path,
    route_path: &Path,
    output_path: &Path,
    signing_key_env: Option<&str>,
    json_output: bool,
) -> Result<(), BoxError> {
    let catalog = fs::read(catalog_path)?;
    CatalogSnapshot::from_json_str(std::str::from_utf8(&catalog)?)?;
    let route = fs::read(route_path)?;
    serde_json::from_slice::<Value>(&route)?;
    let manifest = published_control_manifest(&catalog, &route, signing_key_env)?;
    let mut encoded = serde_json::to_vec_pretty(&manifest)?;
    encoded.push(b'\n');
    replace_control_file(output_path, &encoded)?;
    if json_output {
        println!("{}", serde_json::to_string(&manifest)?);
    } else {
        println!(
            "control revision={} path={}",
            manifest.revision,
            output_path.display()
        );
    }
    Ok(())
}

fn replace_control_file(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    let staged = stage_file(path, bytes)?;
    let previous = previous_path(path);
    let had_current = path.exists();
    if had_current {
        fs::copy(path, &previous)?;
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(&staged, path) {
        if had_current {
            let _ = fs::copy(&previous, path);
        }
        return Err(error.into());
    }
    Ok(())
}

fn previous_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.previous", path.display()))
}

fn digest_prefixed(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> String {
    const BLOCK_BYTES: usize = 64;
    let mut normalized = [0_u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; BLOCK_BYTES];
    for index in 0..BLOCK_BYTES {
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

fn candidate_validation(
    instance: &ProviderInstance,
    catalog: &CatalogSnapshot,
    upstream_id: &str,
    review: Option<&ReviewedProjection>,
) -> (Vec<String>, Vec<String>) {
    const EVIDENCE_FIELDS: [&str; 5] = ["pricing", "capabilities", "compat", "lifecycle", "source"];
    let Some(review) = review else {
        return (
            EVIDENCE_FIELDS.iter().map(ToString::to_string).collect(),
            vec!["review_required".to_owned()],
        );
    };
    let unresolved = EVIDENCE_FIELDS
        .iter()
        .filter(|field| {
            review
                .evidence
                .get(**field)
                .is_none_or(|source| source.trim().is_empty())
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut reasons = Vec::new();
    if review.upstream_id != upstream_id || review.model.upstream_id != upstream_id {
        reasons.push("upstream_id_mismatch".to_owned());
    }
    if review.model.provider.as_str() != instance.catalog_provider_id {
        reasons.push("provider_mismatch".to_owned());
    }
    if catalog.model(&review.model.id).is_some_and(|existing| {
        existing.upstream_id != review.model.upstream_id
            || existing.provider != review.model.provider
    }) {
        reasons.push("canonical_id_conflict".to_owned());
    }
    if review.model.aliases.iter().any(|alias| {
        catalog
            .resolve_model(alias)
            .is_some_and(|existing| existing.canonical_id != &review.model.id)
    }) {
        reasons.push("alias_conflict".to_owned());
    }
    if matches!(
        review.model.lifecycle,
        urouter_ai::catalog::LifecycleStatus::Retired
    ) {
        reasons.push("retired_model_cannot_be_published".to_owned());
    }
    (unresolved, reasons)
}

fn hash_candidate(
    inventory_hash: &str,
    provider: Option<&ProviderSpec>,
    models: &[CandidateModel],
) -> Result<String, serde_json::Error> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(inventory_hash, provider, models))?)
    ))
}

fn load_latest_inventory(
    state_dir: &Path,
    instance_id: &str,
) -> Result<InventorySnapshot, SyncError> {
    let latest = read_latest(state_dir, instance_id)?;
    let relative = Path::new(&latest.snapshot);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SyncError::InvalidResponse(
            "latest snapshot path is unsafe".to_owned(),
        ));
    }
    let snapshot: InventorySnapshot =
        serde_json::from_slice(&fs::read(state_dir.join(instance_id).join(relative))?)?;
    if !snapshot.complete
        || snapshot.instance_id != instance_id
        || snapshot.content_hash != latest.content_hash
        || hash_models(&snapshot.models)? != snapshot.content_hash
    {
        return Err(SyncError::InvalidResponse(
            "latest snapshot failed integrity validation".to_owned(),
        ));
    }
    Ok(snapshot)
}

fn print_status(status: &InventoryStatus, json: bool) -> Result<(), serde_json::Error> {
    if json {
        println!("{}", serde_json::to_string(status)?);
    } else {
        println!(
            "instance={} discovered={} cataloged={}",
            status.instance_id, status.discovered, status.cataloged
        );
        for id in &status.new_upstream_ids {
            println!("new: {id}");
        }
        for id in &status.missing_upstream_ids {
            println!("missing: {id}");
        }
    }
    Ok(())
}

fn load_registry(path: &Path) -> Result<LoadedRegistry, SyncError> {
    let registry: ProviderRegistry = serde_json::from_slice(&fs::read(path)?)?;
    validate_registry(&registry)?;
    Ok(LoadedRegistry {
        registry,
        base_dir: path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
    })
}

fn validate_registry(registry: &ProviderRegistry) -> Result<(), SyncError> {
    if registry.schema_version != 1 {
        return Err(SyncError::UnsupportedRegistrySchema);
    }
    let mut ids = BTreeSet::new();
    for instance in &registry.instances {
        if !ids.insert(instance.id.as_str()) {
            return Err(SyncError::DuplicateInstance(instance.id.clone()));
        }
        validate_instance(instance)?;
    }
    Ok(())
}

fn validate_instance(instance: &ProviderInstance) -> Result<(), SyncError> {
    let invalid_id = instance.id.is_empty()
        || !instance.id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        });
    let invalid = |message: &str| SyncError::InvalidInstance {
        instance: instance.id.clone(),
        message: message.to_owned(),
    };
    if invalid_id {
        return Err(invalid(
            "id must contain only lowercase ASCII letters, digits, dash, underscore or dot",
        ));
    }
    if instance.vendor.trim().is_empty()
        || instance.catalog_provider_id.trim().is_empty()
        || instance.credential_scope.trim().is_empty()
    {
        return Err(invalid(
            "vendor, catalog_provider_id and credential_scope must not be empty",
        ));
    }
    if instance.runtime.protocols.is_empty() {
        return Err(invalid("runtime protocols must not be empty"));
    }
    validate_url_template(&instance.runtime.base_url).map_err(|error| invalid(&error))?;
    if instance
        .runtime
        .auth_env
        .as_deref()
        .is_some_and(str::is_empty)
    {
        return Err(invalid("runtime auth_env must not be empty"));
    }
    match &instance.discovery {
        DiscoveryConfig::OpenAiModels { url, auth_env, .. } => {
            validate_url_template(url).map_err(|error| invalid(&error))?;
            if auth_env.as_deref().is_some_and(str::is_empty) {
                return Err(invalid("discovery auth_env must not be empty"));
            }
        }
        DiscoveryConfig::BailianCatalog {
            url,
            auth_env,
            page_size,
            max_pages,
            ..
        } => {
            validate_url_template(url).map_err(|error| invalid(&error))?;
            if auth_env.is_empty() || *page_size == 0 || *max_pages == 0 {
                return Err(invalid(
                    "bailian auth_env, page_size and max_pages must be non-zero",
                ));
            }
        }
        DiscoveryConfig::ReviewedStatic { path } if path.as_os_str().is_empty() => {
            return Err(invalid("reviewed static path must not be empty"));
        }
        DiscoveryConfig::ReviewedStatic { .. } => {}
    }
    Ok(())
}

fn validate_url_template(template: &str) -> Result<(), String> {
    let expanded = expand_template_with(template, |_| Some("placeholder".to_owned()))
        .map_err(|error| error.to_string())?;
    let url = Url::parse(&expanded).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL scheme must be http or https".to_owned());
    }
    Ok(())
}

fn find_instance<'a>(
    registry: &'a ProviderRegistry,
    id: &str,
) -> Result<&'a ProviderInstance, SyncError> {
    registry
        .instances
        .iter()
        .find(|instance| instance.id == id)
        .ok_or_else(|| SyncError::InstanceNotFound(id.to_owned()))
}

fn driver_for(config: &DiscoveryConfig) -> Box<dyn ProviderDriver> {
    match config {
        DiscoveryConfig::OpenAiModels { .. } => Box::new(OpenAiModelsDriver),
        DiscoveryConfig::BailianCatalog { .. } => Box::new(BailianCatalogDriver),
        DiscoveryConfig::ReviewedStatic { .. } => Box::new(ReviewedStaticDriver),
    }
}

#[async_trait]
impl ProviderDriver for OpenAiModelsDriver {
    fn kind(&self) -> &'static str {
        "open_ai_models"
    }

    async fn discover(
        &self,
        context: &DiscoveryContext<'_>,
    ) -> Result<Vec<RawProviderModel>, SyncError> {
        let DiscoveryConfig::OpenAiModels {
            url,
            auth_env,
            query,
            ..
        } = &context.instance.discovery
        else {
            unreachable!("driver selected from discovery config")
        };
        let url = endpoint_url(url, query)?;
        let request = apply_bearer(context.client.get(url), auth_env.as_deref())?;
        let response = send_json(request).await?;
        parse_openai_models(&response)
    }
}

#[async_trait]
impl ProviderDriver for BailianCatalogDriver {
    fn kind(&self) -> &'static str {
        "bailian_catalog"
    }

    async fn discover(
        &self,
        context: &DiscoveryContext<'_>,
    ) -> Result<Vec<RawProviderModel>, SyncError> {
        let DiscoveryConfig::BailianCatalog {
            url,
            auth_env,
            page_size,
            max_pages,
            query,
            min_request_interval_ms,
        } = &context.instance.discovery
        else {
            unreachable!("driver selected from discovery config")
        };
        let expanded = expand_env_template(url)?;
        let mut all = Vec::new();
        for page_no in 1..=*max_pages {
            if page_no > 1 && *min_request_interval_ms > 0 {
                tokio::time::sleep(Duration::from_millis(*min_request_interval_ms)).await;
            }
            let mut url = Url::parse(&expanded)?;
            {
                let mut pairs = url.query_pairs_mut();
                for (name, value) in query {
                    pairs.append_pair(name, value);
                }
                pairs.append_pair("page_no", &page_no.to_string());
                pairs.append_pair("page_size", &page_size.to_string());
            }
            let request = apply_bearer(context.client.get(url), Some(auth_env))?;
            let response = send_json(request).await?;
            let page = parse_bailian_page(&response)?;
            let page_models = page.models.len();
            all.extend(page.models);
            if page_models == 0
                || all.len() >= page.total
                || page_models < usize::try_from(*page_size).unwrap_or(usize::MAX)
            {
                return Ok(all);
            }
        }
        Err(SyncError::PaginationLimit(*max_pages))
    }
}

#[async_trait]
impl ProviderDriver for ReviewedStaticDriver {
    fn kind(&self) -> &'static str {
        "reviewed_static"
    }

    async fn discover(
        &self,
        context: &DiscoveryContext<'_>,
    ) -> Result<Vec<RawProviderModel>, SyncError> {
        let DiscoveryConfig::ReviewedStatic { path } = &context.instance.discovery else {
            unreachable!("driver selected from discovery config")
        };
        let path = if path.is_absolute() {
            path.clone()
        } else {
            context.base_dir.join(path)
        };
        let value: Value = serde_json::from_slice(&fs::read(path)?)?;
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                SyncError::InvalidResponse("static inventory needs models[]".to_owned())
            })?;
        models.iter().map(raw_model_from_id).collect()
    }
}

fn endpoint_url(template: &str, query: &BTreeMap<String, String>) -> Result<Url, SyncError> {
    let expanded = expand_env_template(template)?;
    let mut url = Url::parse(&expanded)?;
    url.query_pairs_mut().extend_pairs(query);
    Ok(url)
}

fn apply_bearer(
    request: RequestBuilder,
    auth_env: Option<&str>,
) -> Result<RequestBuilder, SyncError> {
    let Some(variable) = auth_env else {
        return Ok(request);
    };
    let value =
        env::var(variable).map_err(|_| SyncError::MissingCredential(variable.to_owned()))?;
    Ok(request.bearer_auth(value))
}

async fn send_json(request: RequestBuilder) -> Result<Value, SyncError> {
    let response = request.send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(SyncError::ResponseTooLarge);
    }
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        let body = body.chars().take(512).collect::<String>();
        return Err(SyncError::Http { status, body });
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn parse_openai_models(value: &Value) -> Result<Vec<RawProviderModel>, SyncError> {
    if value.get("object").and_then(Value::as_str) != Some("list") {
        return Err(SyncError::InvalidResponse(
            "OpenAI model response object must be list".to_owned(),
        ));
    }
    let models = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| SyncError::InvalidResponse("OpenAI response needs data[]".to_owned()))?;
    models.iter().map(raw_model_from_id).collect()
}

struct BailianPage {
    total: usize,
    models: Vec<RawProviderModel>,
}

fn parse_bailian_page(value: &Value) -> Result<BailianPage, SyncError> {
    let output = value
        .get("output")
        .ok_or_else(|| SyncError::InvalidResponse("Bailian response needs output".to_owned()))?;
    let models = output
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| SyncError::InvalidResponse("Bailian output needs models[]".to_owned()))?
        .iter()
        .map(|raw| {
            let upstream_id = raw.get("model").and_then(Value::as_str).ok_or_else(|| {
                SyncError::InvalidResponse("Bailian model needs model ID".to_owned())
            })?;
            validate_upstream_id(upstream_id)?;
            Ok(RawProviderModel {
                upstream_id: upstream_id.to_owned(),
                name: raw.get("name").and_then(Value::as_str).map(str::to_owned),
                raw: raw.clone(),
            })
        })
        .collect::<Result<Vec<_>, SyncError>>()?;
    let total = output
        .get("total")
        .and_then(Value::as_u64)
        .and_then(|total| usize::try_from(total).ok())
        .unwrap_or(models.len());
    Ok(BailianPage { total, models })
}

fn raw_model_from_id(raw: &Value) -> Result<RawProviderModel, SyncError> {
    let upstream_id = raw
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| SyncError::InvalidResponse("model needs string id".to_owned()))?;
    validate_upstream_id(upstream_id)?;
    Ok(RawProviderModel {
        upstream_id: upstream_id.to_owned(),
        name: raw.get("name").and_then(Value::as_str).map(str::to_owned),
        raw: raw.clone(),
    })
}

fn validate_upstream_id(id: &str) -> Result<(), SyncError> {
    if id.is_empty() || id.len() > 512 || id.trim() != id || id.chars().any(char::is_control) {
        return Err(SyncError::InvalidResponse(
            "model ID is empty, oversized or ambiguous".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_models(models: Vec<RawProviderModel>) -> Result<Vec<RawProviderModel>, SyncError> {
    let mut normalized = BTreeMap::new();
    for model in models {
        if let Some(existing) = normalized.get(&model.upstream_id) {
            if existing != &model {
                return Err(SyncError::ConflictingModel(model.upstream_id));
            }
        } else {
            normalized.insert(model.upstream_id.clone(), model);
        }
    }
    Ok(normalized.into_values().collect())
}

fn hash_models(models: &[RawProviderModel]) -> Result<String, serde_json::Error> {
    let digest = Sha256::digest(serde_json::to_vec(models)?);
    Ok(format!("sha256:{digest:x}"))
}

fn persist_snapshot(
    state_dir: &Path,
    snapshot: &InventorySnapshot,
    fetched_at: u64,
) -> Result<PathBuf, SyncError> {
    let instance_dir = state_dir.join(&snapshot.instance_id);
    let snapshots_dir = instance_dir.join("snapshots");
    fs::create_dir_all(&snapshots_dir)?;
    let hash = snapshot
        .content_hash
        .strip_prefix("sha256:")
        .unwrap_or(&snapshot.content_hash);
    let relative = PathBuf::from("snapshots").join(format!("{hash}.json"));
    let snapshot_path = instance_dir.join(&relative);
    if !snapshot_path.exists() {
        write_new_json(&snapshot_path, snapshot)?;
    }
    let latest = LatestSnapshot {
        schema_version: 1,
        instance_id: snapshot.instance_id.clone(),
        content_hash: snapshot.content_hash.clone(),
        snapshot: relative.to_string_lossy().replace('\\', "/"),
        last_fetched_at_unix: fetched_at,
        models: snapshot.models.len(),
    };
    replace_latest_json(&instance_dir.join("latest.json"), &latest)?;
    Ok(snapshot_path)
}

fn write_new_json(path: &Path, value: &impl Serialize) -> Result<(), SyncError> {
    let parent = path.parent().ok_or_else(|| {
        SyncError::InvalidResponse("snapshot path has no parent directory".to_owned())
    })?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

fn replace_latest_json(path: &Path, value: &impl Serialize) -> Result<(), SyncError> {
    let parent = path.parent().ok_or_else(|| {
        SyncError::InvalidResponse("latest path has no parent directory".to_owned())
    })?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".latest.{}.tmp", std::process::id()));
    if temp.exists() {
        fs::remove_file(&temp)?;
    }
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;

    let previous = parent.join("latest.previous.json");
    if previous.exists() {
        fs::remove_file(&previous)?;
    }
    let had_latest = path.exists();
    if had_latest {
        fs::rename(path, &previous)?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        if had_latest {
            let _ = fs::rename(&previous, path);
        }
        return Err(error.into());
    }
    Ok(())
}

fn read_latest(state_dir: &Path, instance_id: &str) -> Result<LatestSnapshot, SyncError> {
    let path = state_dir.join(instance_id).join("latest.json");
    if !path.exists() {
        return Err(SyncError::SnapshotNotFound(instance_id.to_owned()));
    }
    let latest: LatestSnapshot = serde_json::from_slice(&fs::read(path)?)?;
    if latest.schema_version != 1 || latest.instance_id != instance_id {
        return Err(SyncError::InvalidResponse(
            "latest snapshot metadata is invalid".to_owned(),
        ));
    }
    Ok(latest)
}

fn expand_env_template(template: &str) -> Result<String, SyncError> {
    expand_template_with(template, |variable| env::var(variable).ok())
}

fn expand_template_with(
    template: &str,
    mut resolve: impl FnMut(&str) -> Option<String>,
) -> Result<String, SyncError> {
    let mut result = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(start) = remaining.find("${") {
        result.push_str(&remaining[..start]);
        let after = &remaining[start + 2..];
        let end = after.find('}').ok_or_else(|| {
            SyncError::InvalidEndpointTemplate("unclosed environment variable".to_owned())
        })?;
        let variable = &after[..end];
        if variable.is_empty()
            || !variable
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(SyncError::InvalidEndpointTemplate(format!(
                "invalid environment variable {variable}"
            )));
        }
        result.push_str(
            &resolve(variable)
                .ok_or_else(|| SyncError::MissingEndpointVariable(variable.to_owned()))?,
        );
        remaining = &after[end + 1..];
    }
    if remaining.contains('}') {
        return Err(SyncError::InvalidEndpointTemplate(
            "unmatched closing brace".to_owned(),
        ));
    }
    result.push_str(remaining);
    Ok(result)
}

fn unix_timestamp() -> Result<u64, std::time::SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn wire_api_name(api: &WireApi) -> &'static str {
    match api {
        WireApi::OpenAiChat => "open_ai_chat",
        WireApi::OpenAiResponses => "open_ai_responses",
        WireApi::AnthropicMessages => "anthropic_messages",
        WireApi::Custom(_) => "custom",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn instance(discovery: DiscoveryConfig) -> ProviderInstance {
        ProviderInstance {
            id: "test-main".to_owned(),
            vendor: "test".to_owned(),
            catalog_provider_id: "test".to_owned(),
            runtime: RuntimeConfig {
                base_url: "https://example.com/v1".to_owned(),
                protocols: vec![WireApi::OpenAiChat],
                auth_env: Some("TEST_API_KEY".to_owned()),
            },
            discovery,
            credential_scope: "test-main".to_owned(),
            enabled: true,
        }
    }

    #[test]
    fn parses_and_sorts_openai_inventory() {
        let models = parse_openai_models(&json!({
            "object": "list",
            "data": [
                {"id": "z/model", "object": "model"},
                {"id": "a/model", "object": "model"}
            ]
        }))
        .unwrap();
        let normalized = normalize_models(models).unwrap();
        assert_eq!(normalized[0].upstream_id, "a/model");
        assert_eq!(normalized[1].upstream_id, "z/model");
    }

    #[test]
    fn parses_bailian_rich_inventory() {
        let page = parse_bailian_page(&json!({
            "output": {
                "total": 1,
                "page_no": 1,
                "page_size": 20,
                "models": [{
                    "model": "qwen-plus",
                    "name": "Qwen Plus",
                    "features": ["function-calling"],
                    "context_window": 131_072
                }]
            }
        }))
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.models[0].upstream_id, "qwen-plus");
        assert_eq!(page.models[0].name.as_deref(), Some("Qwen Plus"));
        assert_eq!(
            page.models[0].raw["features"][0],
            Value::String("function-calling".to_owned())
        );
    }

    #[test]
    fn rejects_conflicting_duplicate_ids() {
        let models = vec![
            RawProviderModel {
                upstream_id: "same".to_owned(),
                name: Some("first".to_owned()),
                raw: json!({"id": "same", "version": 1}),
            },
            RawProviderModel {
                upstream_id: "same".to_owned(),
                name: Some("second".to_owned()),
                raw: json!({"id": "same", "version": 2}),
            },
        ];
        assert!(matches!(
            normalize_models(models),
            Err(SyncError::ConflictingModel(id)) if id == "same"
        ));
    }

    #[test]
    fn validates_registry_without_reading_credentials() {
        let registry = ProviderRegistry {
            schema_version: 1,
            instances: vec![instance(DiscoveryConfig::BailianCatalog {
                url: "https://${BAILIAN_WORKSPACE_ID}.example/api/v1/models".to_owned(),
                auth_env: "DASHSCOPE_API_KEY".to_owned(),
                page_size: 100,
                max_pages: 10,
                query: BTreeMap::new(),
                min_request_interval_ms: 0,
            })],
        };
        validate_registry(&registry).unwrap();
    }

    #[test]
    fn environment_template_is_strict() {
        assert_eq!(
            expand_template_with("https://${WORKSPACE}.example/v1", |name| {
                (name == "WORKSPACE").then(|| "ws-1".to_owned())
            })
            .unwrap(),
            "https://ws-1.example/v1"
        );
        assert!(expand_template_with("https://${bad}.example", |_| None).is_err());
        assert!(expand_template_with("https://${UNCLOSED.example", |_| None).is_err());
    }

    #[tokio::test]
    async fn reviewed_static_driver_reads_registry_relative_file() {
        let root = std::env::temp_dir().join(format!(
            "urouter-provider-sync-{}-{}",
            std::process::id(),
            unix_timestamp().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("models.json"),
            serde_json::to_vec(&json!({
                "models": [{"id": "reviewed/model", "name": "Reviewed"}]
            }))
            .unwrap(),
        )
        .unwrap();
        let instance = instance(DiscoveryConfig::ReviewedStatic {
            path: PathBuf::from("models.json"),
        });
        let client = Client::new();
        let context = DiscoveryContext {
            client: &client,
            instance: &instance,
            base_dir: &root,
        };
        let models = ReviewedStaticDriver.discover(&context).await.unwrap();
        assert_eq!(models[0].upstream_id, "reviewed/model");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn openai_driver_fetches_a_real_http_fixture() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /v1/models?type=text HTTP/1.1"));
            let body = serde_json::to_string(&json!({
                "object": "list",
                "data": [{"id": "fixture/chat-model", "object": "model"}]
            }))
            .unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let instance = instance(DiscoveryConfig::OpenAiModels {
            url: format!("http://{address}/v1/models"),
            auth_env: None,
            query: BTreeMap::from([("type".to_owned(), "text".to_owned())]),
            min_request_interval_ms: 0,
        });
        let client = Client::builder().no_proxy().build().unwrap();
        let context = DiscoveryContext {
            client: &client,
            instance: &instance,
            base_dir: Path::new("."),
        };
        let models = OpenAiModelsDriver.discover(&context).await.unwrap();
        assert_eq!(models[0].upstream_id, "fixture/chat-model");
        server.await.unwrap();
    }

    // --- Driver error and throttling paths (docs/test-coverage-plan.md Phase 5)
    //
    // Discovery output passes human review, candidate quarantine and
    // control-last publish before it can reach the active Catalog, so the goal
    // here is not overall coverage. It is that a provider behaving badly — a
    // 401, a truncated page set, a body that is not the documented shape — is
    // refused with a typed reason instead of yielding a partial inventory that
    // looks complete.

    /// Serves `responses` in order on one connection each, then stops.
    async fn fixture_server(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = vec![0_u8; 8192];
                let _ = socket.read(&mut buffer).await;
                let reason = if status == 200 { "OK" } else { "ERROR" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (address.to_string(), handle)
    }

    fn discovery_client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    /// Present on every platform the workspace builds for.
    const ALWAYS_PRESENT_ENV: &str = "PATH";

    /// An upstream error must surface as a typed HTTP failure carrying the
    /// status, not as an empty inventory that would read as "this provider has
    /// no models" and silently retire every model it publishes.
    #[tokio::test]
    async fn discovery_reports_upstream_error_statuses_with_their_body() {
        for status in [401_u16, 429, 500] {
            let (address, server) =
                fixture_server(vec![(status, r#"{"error":"denied"}"#.to_owned())]).await;
            let instance = instance(DiscoveryConfig::OpenAiModels {
                url: format!("http://{address}/v1/models"),
                auth_env: None,
                query: BTreeMap::new(),
                min_request_interval_ms: 0,
            });
            let client = discovery_client();
            let context = DiscoveryContext {
                client: &client,
                instance: &instance,
                base_dir: Path::new("."),
            };
            match OpenAiModelsDriver.discover(&context).await {
                Err(SyncError::Http {
                    status: reported,
                    body,
                }) => {
                    assert_eq!(reported.as_u16(), status);
                    assert!(body.contains("denied"), "{body}");
                }
                other => panic!("HTTP {status} was not reported: {other:?}"),
            }
            server.abort();
        }
    }

    /// A 200 whose body is not the documented shape is refused rather than
    /// parsed into an empty list.
    #[tokio::test]
    async fn discovery_refuses_a_successful_response_of_the_wrong_shape() {
        let (address, server) =
            fixture_server(vec![(200, r#"{"object":"error","data":[]}"#.to_owned())]).await;
        let instance = instance(DiscoveryConfig::OpenAiModels {
            url: format!("http://{address}/v1/models"),
            auth_env: None,
            query: BTreeMap::new(),
            min_request_interval_ms: 0,
        });
        let client = discovery_client();
        let context = DiscoveryContext {
            client: &client,
            instance: &instance,
            base_dir: Path::new("."),
        };
        assert!(matches!(
            OpenAiModelsDriver.discover(&context).await,
            Err(SyncError::InvalidResponse(_))
        ));
        server.abort();
    }

    /// A configured credential that is absent must fail before the request is
    /// sent, so an unauthenticated call is never made against a paid endpoint.
    #[test]
    fn a_missing_credential_fails_before_the_request_is_sent() {
        let client = discovery_client();
        let request = client.get("https://example.com/v1/models");
        match apply_bearer(request, Some("UROUTER_TEST_ABSENT_CREDENTIAL")) {
            Err(SyncError::MissingCredential(name)) => {
                assert_eq!(name, "UROUTER_TEST_ABSENT_CREDENTIAL");
            }
            other => panic!("expected MissingCredential, got {other:?}"),
        }
        // No configured credential is not an error: loopback and unauthenticated
        // inventories are legitimate.
        assert!(apply_bearer(client.get("https://example.com/v1/models"), None).is_ok());
    }

    /// Pagination must terminate. A provider that keeps reporting more pages
    /// than configured is refused rather than looped over indefinitely.
    #[tokio::test]
    async fn bailian_pagination_stops_at_the_configured_page_limit() {
        // Every page is full and claims a larger total, so the driver never sees
        // a natural end.
        let page = |page_no: u32| {
            (
                200_u16,
                serde_json::to_string(&json!({
                    "output": {
                        "total": 100,
                        "page_no": page_no,
                        "page_size": 2,
                        "models": [
                            {"model": format!("m-{page_no}-a")},
                            {"model": format!("m-{page_no}-b")}
                        ]
                    }
                }))
                .unwrap(),
            )
        };
        let (address, server) = fixture_server(vec![page(1), page(2)]).await;
        let instance = instance(DiscoveryConfig::BailianCatalog {
            url: format!("http://{address}/models"),
            // The workspace forbids `unsafe_code` and `env::set_var` is unsafe in
            // edition 2024, so the test borrows a variable that always exists.
            // Only the presence of a credential matters here, not its value.
            auth_env: ALWAYS_PRESENT_ENV.to_owned(),
            page_size: 2,
            max_pages: 2,
            query: BTreeMap::new(),
            min_request_interval_ms: 0,
        });
        let client = discovery_client();
        let context = DiscoveryContext {
            client: &client,
            instance: &instance,
            base_dir: Path::new("."),
        };
        match BailianCatalogDriver.discover(&context).await {
            Err(SyncError::PaginationLimit(limit)) => assert_eq!(limit, 2),
            other => panic!("expected PaginationLimit, got {other:?}"),
        }
        server.abort();
    }

    /// A short page means the inventory is complete, so the driver stops early
    /// rather than spending the remaining page budget on empty requests.
    #[tokio::test]
    async fn bailian_pagination_stops_early_on_a_short_page() {
        let body = serde_json::to_string(&json!({
            "output": {
                "total": 100,
                "page_no": 1,
                "page_size": 10,
                "models": [{"model": "only-one"}]
            }
        }))
        .unwrap();
        let (address, server) = fixture_server(vec![(200, body)]).await;
        let instance = instance(DiscoveryConfig::BailianCatalog {
            url: format!("http://{address}/models"),
            auth_env: ALWAYS_PRESENT_ENV.to_owned(),
            page_size: 10,
            max_pages: 5,
            query: BTreeMap::new(),
            min_request_interval_ms: 0,
        });
        let client = discovery_client();
        let context = DiscoveryContext {
            client: &client,
            instance: &instance,
            base_dir: Path::new("."),
        };
        let models = BailianCatalogDriver.discover(&context).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].upstream_id, "only-one");
        server.abort();
    }

    /// Malformed Bailian pages are rejected by shape rather than yielding a
    /// partially parsed inventory.
    #[test]
    fn bailian_pages_are_rejected_by_shape() {
        for value in [
            json!({}),
            json!({"output": {}}),
            json!({"output": {"models": [{"name": "no id"}]}}),
        ] {
            assert!(
                matches!(
                    parse_bailian_page(&value),
                    Err(SyncError::InvalidResponse(_))
                ),
                "accepted a malformed page: {value}"
            );
        }
    }

    /// The static driver is the reviewed-inventory path. A missing or malformed
    /// file must be typed, not a panic or an empty inventory.
    #[tokio::test]
    async fn reviewed_static_driver_reports_missing_and_malformed_inventories() {
        let root = env::temp_dir().join(format!("urouter-static-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let client = discovery_client();

        let absent = instance(DiscoveryConfig::ReviewedStatic {
            path: PathBuf::from("absent.json"),
        });
        assert!(matches!(
            ReviewedStaticDriver
                .discover(&DiscoveryContext {
                    client: &client,
                    instance: &absent,
                    base_dir: &root,
                })
                .await,
            Err(SyncError::Io(_))
        ));

        fs::write(root.join("bad.json"), b"{\"models\": {}}").unwrap();
        let malformed = instance(DiscoveryConfig::ReviewedStatic {
            path: PathBuf::from("bad.json"),
        });
        assert!(matches!(
            ReviewedStaticDriver
                .discover(&DiscoveryContext {
                    client: &client,
                    instance: &malformed,
                    base_dir: &root,
                })
                .await,
            Err(SyncError::InvalidResponse(_))
        ));

        fs::remove_dir_all(root).unwrap();
    }

    /// An endpoint template naming an unset variable must fail with the variable
    /// name, so a misconfigured registry is diagnosable without guessing.
    #[test]
    fn endpoint_templates_report_the_missing_variable_by_name() {
        match endpoint_url(
            "https://${UROUTER_TEST_ABSENT_HOST}/v1/models",
            &BTreeMap::new(),
        ) {
            Err(SyncError::MissingEndpointVariable(name)) => {
                assert_eq!(name, "UROUTER_TEST_ABSENT_HOST");
            }
            other => panic!("expected MissingEndpointVariable, got {other:?}"),
        }
    }

    #[test]
    fn unreviewed_candidate_is_quarantined_with_all_required_evidence() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let (unresolved, reasons) = candidate_validation(
            &instance(DiscoveryConfig::ReviewedStatic {
                path: PathBuf::from("models.json"),
            }),
            &catalog,
            "new-model",
            None,
        );
        assert_eq!(
            unresolved,
            ["pricing", "capabilities", "compat", "lifecycle", "source"]
        );
        assert_eq!(reasons, ["review_required"]);
    }

    #[test]
    fn probe_matcher_requires_capability_specific_evidence() {
        assert!(probe_response_matches(
            "text",
            &json!({"choices": [{"message": {"content": "OK"}}]})
        ));
        assert!(probe_response_matches(
            "tool_calling",
            &json!({"choices": [{"message": {"tool_calls": [{"id": "call-1"}]}}]})
        ));
        assert!(probe_response_matches(
            "structured_output",
            &json!({"choices": [{"message": {"content": "{\"ok\":true}"}}]})
        ));
        assert!(probe_response_matches(
            "reasoning",
            &json!({"choices": [{"message": {"reasoning_content": "17*19=323"}}]})
        ));
        assert!(!probe_response_matches(
            "tool_calling",
            &json!({"choices": [{"message": {"content": "I cannot call it"}}]})
        ));
    }

    #[test]
    fn hmac_implementation_matches_standard_vector() {
        assert_eq!(
            hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog"),
            "hmac-sha256:f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn publish_bundle_can_restore_previous_revision() {
        let root = std::env::temp_dir().join(format!(
            "urouter-publish-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let catalog = root.join("catalog.json");
        let manifest = root.join("manifest.json");
        let control = root.join("control.json");
        for path in [&catalog, &manifest, &control] {
            fs::write(path, b"old").unwrap();
        }
        atomic_publish_bundle([
            (catalog.as_path(), b"new-catalog"),
            (manifest.as_path(), b"new-manifest"),
            (control.as_path(), b"new-control"),
        ])
        .unwrap();
        assert_eq!(fs::read(&control).unwrap(), b"new-control");
        rollback_bundle(&catalog, &manifest, &control, false).unwrap();
        assert_eq!(fs::read(&catalog).unwrap(), b"old");
        assert_eq!(fs::read(&manifest).unwrap(), b"old");
        assert_eq!(fs::read(&control).unwrap(), b"old");
        fs::remove_dir_all(root).unwrap();
    }
}
