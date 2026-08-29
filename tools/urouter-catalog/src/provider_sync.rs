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
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use urouter_ai::catalog::CatalogSnapshot;
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
        } = &context.instance.discovery
        else {
            unreachable!("driver selected from discovery config")
        };
        let expanded = expand_env_template(url)?;
        let mut all = Vec::new();
        for page_no in 1..=*max_pages {
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
}
