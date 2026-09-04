use std::{collections::BTreeMap, fmt, fmt::Write as _, sync::Arc};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use urouter_types::{CatalogHash, ModelId, ProviderId, WireApi};

use crate::{
    auth::AuthSpec, capabilities::Capabilities, compat::Compat, compat::ParamPolicy,
    endpoint::EndpointPlan, pricing::ModelCost,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FactConfidence {
    Official,
    Verified,
    Community,
    Override,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMeta {
    pub source: String,
    pub checked_at: String,
    pub confidence: FactConfidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LifecycleStatus {
    Active,
    Deprecated,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSpec {
    pub id: ProviderId,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub built_in: bool,
    pub auth: AuthSpec,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Request quirks that apply to every model of this provider.
    ///
    /// Quirks are overwhelmingly a PROVIDER property — "Mistral 422s on unknown
    /// keys", "GitHub Models caps `max_tokens` at 400" — not a per-model one. A
    /// model may still narrow it further via `Compat::param_policy`; the two are
    /// merged with the model winning.
    #[serde(default)]
    pub param_policy: ParamPolicy,
    /// Per-provider request timeout. `None` uses the gateway default.
    ///
    /// One global timeout cannot serve both a sub-second edge inference API and
    /// a queue-based volunteer network that legitimately takes minutes: the
    /// short bound makes the slow provider permanently unusable, and the long
    /// one makes a dead fast provider hold a lease for minutes.
    #[serde(default)]
    pub timeout_millis: Option<u64>,
    pub source: SourceMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub id: ModelId,
    pub upstream_id: String,
    pub name: String,
    pub provider: ProviderId,
    pub api: WireApi,
    pub base_url: Option<String>,
    pub cost: ModelCost,
    pub capabilities: Capabilities,
    pub compat: Compat,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub aliases: Vec<ModelId>,
    pub lifecycle: LifecycleStatus,
    pub source: SourceMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogDocument {
    pub schema_version: u16,
    #[serde(default)]
    pub providers: Vec<ProviderSpec>,
    #[serde(default)]
    pub models: Vec<ModelSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogHashes {
    pub content: CatalogHash,
    pub pricing: CatalogHash,
    pub capabilities: CatalogHash,
    pub compat: CatalogHash,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelVariantKey {
    pub provider: ProviderId,
    pub upstream_id: String,
    pub api: WireApi,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelResolution<'a> {
    pub canonical_id: &'a ModelId,
    pub model: &'a ModelSpec,
    pub matched_alias: Option<&'a ModelId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogManifest {
    pub schema_version: u16,
    pub hashes: CatalogHashes,
    pub catalog_file: CatalogHash,
}

impl CatalogManifest {
    #[must_use]
    pub fn from_source(source: &[u8], catalog: &CatalogSnapshot) -> Self {
        Self {
            schema_version: catalog.schema_version,
            hashes: catalog.hashes.clone(),
            catalog_file: hash_bytes(source),
        }
    }

    #[must_use]
    pub fn matches(&self, source: &[u8], catalog: &CatalogSnapshot) -> bool {
        self.schema_version == catalog.schema_version
            && self.hashes == catalog.hashes
            && self.catalog_file == hash_bytes(source)
    }
}

#[derive(Debug, Clone)]
pub struct CatalogSnapshot {
    schema_version: u16,
    providers: Arc<BTreeMap<ProviderId, ProviderSpec>>,
    models: Arc<BTreeMap<ModelId, ModelSpec>>,
    aliases: Arc<BTreeMap<ModelId, ModelId>>,
    variants: Arc<BTreeMap<ModelVariantKey, ModelId>>,
    hashes: CatalogHashes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub code: String,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "catalog validation failed with {} issue(s)",
            self.issues.len()
        )
    }
}

impl std::error::Error for ValidationReport {}

#[derive(Debug, Error)]
pub enum CatalogLoadError {
    #[error("catalog JSON is invalid: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] ValidationReport),
}

impl CatalogSnapshot {
    pub fn from_json_str(input: &str) -> Result<Self, CatalogLoadError> {
        let document: CatalogDocument = serde_json::from_str(input)?;
        Self::from_document(document).map_err(Into::into)
    }

    pub fn from_document(mut document: CatalogDocument) -> Result<Self, ValidationReport> {
        document
            .providers
            .sort_by(|left, right| left.id.cmp(&right.id));
        document
            .models
            .sort_by(|left, right| left.id.cmp(&right.id));
        for model in &mut document.models {
            model.aliases.sort();
        }
        let issues = validate_document(&document);
        if !issues.is_empty() {
            return Err(ValidationReport { issues });
        }

        let hashes = CatalogHashes {
            content: hash_serializable(&document),
            pricing: hash_serializable(
                &document
                    .models
                    .iter()
                    .map(|model| (&model.id, &model.cost))
                    .collect::<Vec<_>>(),
            ),
            capabilities: hash_serializable(
                &document
                    .models
                    .iter()
                    .map(|model| (&model.id, &model.capabilities))
                    .collect::<Vec<_>>(),
            ),
            compat: hash_serializable(
                &document
                    .models
                    .iter()
                    .map(|model| (&model.id, &model.compat))
                    .collect::<Vec<_>>(),
            ),
        };
        let providers = document
            .providers
            .into_iter()
            .map(|provider| (provider.id.clone(), provider))
            .collect();
        let models: BTreeMap<_, _> = document
            .models
            .into_iter()
            .map(|model| (model.id.clone(), model))
            .collect();
        let aliases = models
            .values()
            .flat_map(|model| {
                model
                    .aliases
                    .iter()
                    .cloned()
                    .map(|alias| (alias, model.id.clone()))
            })
            .collect();
        let variants = models
            .values()
            .map(|model| {
                (
                    ModelVariantKey {
                        provider: model.provider.clone(),
                        upstream_id: model.upstream_id.clone(),
                        api: model.api.clone(),
                    },
                    model.id.clone(),
                )
            })
            .collect();

        Ok(Self {
            schema_version: document.schema_version,
            providers: Arc::new(providers),
            models: Arc::new(models),
            aliases: Arc::new(aliases),
            variants: Arc::new(variants),
            hashes,
        })
    }

    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    #[must_use]
    pub fn hashes(&self) -> &CatalogHashes {
        &self.hashes
    }

    pub fn providers(&self) -> impl Iterator<Item = &ProviderSpec> {
        self.providers.values()
    }

    pub fn models(&self) -> impl Iterator<Item = &ModelSpec> {
        self.models.values()
    }

    #[must_use]
    pub fn provider(&self, id: &ProviderId) -> Option<&ProviderSpec> {
        self.providers.get(id)
    }

    #[must_use]
    pub fn model(&self, id: &ModelId) -> Option<&ModelSpec> {
        self.resolve_model(id).map(|resolution| resolution.model)
    }

    #[must_use]
    pub fn resolve_model(&self, id: &ModelId) -> Option<ModelResolution<'_>> {
        if let Some((canonical_id, model)) = self.models.get_key_value(id) {
            return Some(ModelResolution {
                canonical_id,
                model,
                matched_alias: None,
            });
        }
        let (alias, canonical_id) = self.aliases.get_key_value(id)?;
        Some(ModelResolution {
            canonical_id,
            model: self.models.get(canonical_id)?,
            matched_alias: Some(alias),
        })
    }

    #[must_use]
    pub fn model_variant(
        &self,
        provider: &ProviderId,
        upstream_id: &str,
        api: &WireApi,
    ) -> Option<&ModelSpec> {
        let key = ModelVariantKey {
            provider: provider.clone(),
            upstream_id: upstream_id.to_owned(),
            api: api.clone(),
        };
        self.variants.get(&key).and_then(|id| self.models.get(id))
    }

    #[must_use]
    pub fn canonical_model_id(&self, id: &ModelId) -> Option<&ModelId> {
        self.models
            .get_key_value(id)
            .map(|(canonical, _)| canonical)
            .or_else(|| self.aliases.get(id))
    }

    #[must_use]
    pub fn document(&self) -> CatalogDocument {
        CatalogDocument {
            schema_version: self.schema_version,
            providers: self.providers.values().cloned().collect(),
            models: self.models.values().cloned().collect(),
        }
    }
}

fn validate_document(document: &CatalogDocument) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if document.schema_version != 1 {
        push_issue(
            &mut issues,
            "unsupported_schema",
            "schema_version",
            "only schema_version=1 is supported",
        );
    }

    let providers = validate_providers(document, &mut issues);
    validate_models(document, &providers, &mut issues);
    issues
}

fn validate_providers<'a>(
    document: &'a CatalogDocument,
    issues: &mut Vec<ValidationIssue>,
) -> BTreeMap<&'a ProviderId, &'a ProviderSpec> {
    let mut providers = BTreeMap::new();
    for (index, provider) in document.providers.iter().enumerate() {
        if providers.insert(&provider.id, provider).is_some() {
            push_issue(
                issues,
                "duplicate_provider",
                format!("providers[{index}].id"),
                format!("duplicate provider {}", provider.id),
            );
        }
        if provider.name.trim().is_empty() {
            push_issue(
                issues,
                "empty_name",
                format!("providers[{index}].name"),
                "provider name must not be empty",
            );
        }
        if let Err(error) = EndpointPlan::for_provider(provider) {
            push_issue(
                issues,
                "invalid_provider_endpoint",
                format!("providers[{index}].base_url"),
                error.to_string(),
            );
        }
    }
    providers
}

fn validate_models(
    document: &CatalogDocument,
    providers: &BTreeMap<&ProviderId, &ProviderSpec>,
    issues: &mut Vec<ValidationIssue>,
) {
    let mut models = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut variants = BTreeMap::new();
    for (index, model) in document.models.iter().enumerate() {
        if models.insert(&model.id, model).is_some() {
            push_issue(
                issues,
                "duplicate_model",
                format!("models[{index}].id"),
                format!("duplicate model {}", model.id),
            );
        }
        if !providers.contains_key(&model.provider) {
            push_issue(
                issues,
                "unknown_provider",
                format!("models[{index}].provider"),
                format!("unknown provider {}", model.provider),
            );
        }
        let variant = (&model.provider, model.upstream_id.as_str(), &model.api);
        if let Some(existing) = variants.insert(variant, &model.id) {
            push_issue(
                issues,
                "duplicate_model_variant",
                format!("models[{index}]"),
                format!(
                    "model variant duplicates {existing}: provider={}, upstream_id={}, api={:?}",
                    model.provider, model.upstream_id, model.api
                ),
            );
        }
        if let Err(error) = model.cost.validate() {
            push_issue(
                issues,
                "invalid_cost",
                format!("models[{index}].cost"),
                error.to_string(),
            );
        }
        if model.capabilities.context_window == 0 {
            push_issue(
                issues,
                "invalid_context_window",
                format!("models[{index}].capabilities.context_window"),
                "context window must be greater than zero",
            );
        }
        if model.capabilities.max_output_tokens == 0
            || model.capabilities.max_output_tokens > model.capabilities.context_window
        {
            push_issue(
                issues,
                "invalid_max_output_tokens",
                format!("models[{index}].capabilities.max_output_tokens"),
                "max output tokens must be within the context window",
            );
        }
        if let Some(provider) = providers.get(&model.provider) {
            if !provider.built_in {
                for missing in model.compat.missing_required_fields() {
                    push_issue(
                        issues,
                        "missing_custom_compat",
                        format!("models[{index}].compat.{missing}"),
                        "custom provider compatibility must be explicit",
                    );
                }
            }
            if let Err(error) = EndpointPlan::for_model(provider, model) {
                push_issue(
                    issues,
                    "invalid_model_endpoint",
                    format!("models[{index}].base_url"),
                    error.to_string(),
                );
            }
        }
        for alias in &model.aliases {
            if aliases.insert(alias, &model.id).is_some() {
                push_issue(
                    issues,
                    "duplicate_alias",
                    format!("models[{index}].aliases"),
                    format!("duplicate alias {alias}"),
                );
            }
        }
    }
    for (alias, target) in aliases {
        if models.contains_key(alias) {
            push_issue(
                issues,
                "alias_shadows_model",
                format!("models[{target}].aliases"),
                format!("alias {alias} shadows a canonical model ID"),
            );
        }
    }
}

fn push_issue(
    issues: &mut Vec<ValidationIssue>,
    code: impl Into<String>,
    path: impl Into<String>,
    message: impl Into<String>,
) {
    issues.push(ValidationIssue {
        code: code.into(),
        path: path.into(),
        message: message.into(),
    });
}

fn hash_serializable(value: &impl Serialize) -> CatalogHash {
    let serialized = serde_json::to_vec(value).expect("catalog canonical data must serialize");
    hash_bytes(&serialized)
}

fn hash_bytes(value: &[u8]) -> CatalogHash {
    let digest = Sha256::digest(value);
    let hex = digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
            hex
        });
    CatalogHash::sha256(hex).expect("SHA-256 output is always a valid catalog hash")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_document_reports_multiple_issues() {
        let report = CatalogSnapshot::from_document(CatalogDocument {
            schema_version: 99,
            providers: vec![],
            models: vec![],
        })
        .unwrap_err();
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].code, "unsupported_schema");
    }
}
