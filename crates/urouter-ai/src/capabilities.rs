use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ThinkingSupport {
    Unsupported,
    Boolean,
    Levels(BTreeSet<String>),
    TokenBudget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CacheRetention {
    ProviderDefault,
    Seconds(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptCacheSupport {
    pub enabled: bool,
    pub min_cacheable_tokens: u64,
    pub retention: CacheRetention,
    pub explicit_control: bool,
    pub session_affinity_header: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub input_modalities: BTreeSet<Modality>,
    pub tool_calling: bool,
    pub structured_output: bool,
    pub reasoning: ThinkingSupport,
    pub prompt_cache: PromptCacheSupport,
}

impl Capabilities {
    #[must_use]
    pub fn supports(&self, requirement: &CapabilityRequirement) -> bool {
        self.context_window >= requirement.min_context_window
            && requirement
                .input_modalities
                .is_subset(&self.input_modalities)
            && (!requirement.tool_calling || self.tool_calling)
            && (!requirement.structured_output || self.structured_output)
            && (!requirement.reasoning || !matches!(self.reasoning, ThinkingSupport::Unsupported))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityRequirement {
    pub min_context_window: u64,
    pub input_modalities: BTreeSet<Modality>,
    pub tool_calling: bool,
    pub structured_output: bool,
    pub reasoning: bool,
}
