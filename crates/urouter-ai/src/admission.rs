use serde::{Deserialize, Serialize};
use urouter_types::ModelId;

use crate::{
    capabilities::{CapabilityRequirement, Modality},
    catalog::{CatalogSnapshot, LifecycleStatus, ModelSpec},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exclusion {
    pub model: ModelId,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionResult {
    pub eligible: Vec<ModelId>,
    pub excluded: Vec<Exclusion>,
}

#[must_use]
pub fn eligible_models(
    catalog: &CatalogSnapshot,
    requirement: &CapabilityRequirement,
) -> AdmissionResult {
    let mut eligible = Vec::new();
    let mut excluded = Vec::new();
    for model in catalog.models() {
        let reasons = exclusion_reasons(model, requirement);
        if reasons.is_empty() {
            eligible.push(model.id.clone());
        } else {
            excluded.push(Exclusion {
                model: model.id.clone(),
                reasons,
            });
        }
    }
    AdmissionResult { eligible, excluded }
}

fn exclusion_reasons(model: &ModelSpec, requirement: &CapabilityRequirement) -> Vec<String> {
    let mut reasons = Vec::new();
    if !matches!(model.lifecycle, LifecycleStatus::Active) {
        reasons.push("model_not_active".to_owned());
    }
    if model.capabilities.context_window < requirement.min_context_window {
        reasons.push("context_window_too_small".to_owned());
    }
    for modality in requirement
        .input_modalities
        .difference(&model.capabilities.input_modalities)
    {
        reasons.push(format!("missing_modality:{}", modality_name(modality)));
    }
    if requirement.tool_calling && !model.capabilities.tool_calling {
        reasons.push("tool_calling_unsupported".to_owned());
    }
    if requirement.structured_output && !model.capabilities.structured_output {
        reasons.push("structured_output_unsupported".to_owned());
    }
    if requirement.reasoning
        && matches!(
            model.capabilities.reasoning,
            crate::capabilities::ThinkingSupport::Unsupported
        )
    {
        reasons.push("reasoning_unsupported".to_owned());
    }
    reasons
}

fn modality_name(modality: &Modality) -> &'static str {
    match modality {
        Modality::Text => "text",
        Modality::Image => "image",
        Modality::Audio => "audio",
        Modality::Video => "video",
    }
}
