use serde::{Deserialize, Serialize};
use urouter_types::{ModelId, ProviderId};

use crate::{catalog::CatalogSnapshot, pricing::ModelCost};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostOverride {
    pub reason: String,
    pub cost: ModelCost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentSpec {
    pub id: String,
    pub model: ModelId,
    pub provider_override: Option<ProviderId>,
    pub rpm: Option<u64>,
    pub tpm: Option<u64>,
    pub region: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    pub cost_override: Option<CostOverride>,
}

const fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentValidationIssue {
    pub deployment: String,
    pub code: String,
    pub message: String,
}

#[must_use]
pub fn validate_deployments(
    catalog: &CatalogSnapshot,
    deployments: &[DeploymentSpec],
) -> Vec<DeploymentValidationIssue> {
    let mut issues = Vec::new();
    for deployment in deployments {
        if catalog.model(&deployment.model).is_none() {
            issues.push(issue(
                deployment,
                "unknown_model",
                "model is not in the catalog",
            ));
        }
        if deployment.weight == 0 {
            issues.push(issue(
                deployment,
                "zero_weight",
                "weight must be greater than zero",
            ));
        }
        if let Some(cost_override) = &deployment.cost_override {
            if cost_override.reason.trim().is_empty() {
                issues.push(issue(
                    deployment,
                    "missing_override_reason",
                    "cost override requires a non-empty reason",
                ));
            }
            if let Err(error) = cost_override.cost.validate() {
                issues.push(issue(
                    deployment,
                    "invalid_cost_override",
                    error.to_string(),
                ));
            }
        }
    }
    issues
}

fn issue(
    deployment: &DeploymentSpec,
    code: impl Into<String>,
    message: impl Into<String>,
) -> DeploymentValidationIssue {
    DeploymentValidationIssue {
        deployment: deployment.id.clone(),
        code: code.into(),
        message: message.into(),
    }
}
