use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
    MaxOutputTokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCallFormat {
    OpenAi,
    Anthropic,
    None,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StructuredOutputFormat {
    JsonSchema,
    JsonObject,
    ToolCall,
    None,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ThinkingFormat {
    None,
    OpenAiEffort,
    AnthropicBudget,
    Custom(String),
}

/// What an upstream does with sampling parameters it does not implement.
///
/// Providers disagree, loudly. Mistral 422s on an unknown assistant key, Groq
/// and Cerebras 400 on `reasoning_content`, GitHub Models caps `max_tokens` at
/// a few hundred, Mistral spells `seed` as `random_seed`. Forwarding a request
/// verbatim to all of them is a guaranteed 4xx on most.
///
/// The reference implementation this is modelled on keeps an equivalent table
/// (`PLATFORM_PARAM_POLICIES`) in code, one entry per platform. Here it is
/// catalog DATA instead: a new provider quirk ships as a catalog revision that
/// is hashed, signed, reviewable and rollback-able, rather than as a binary
/// release. That is the whole reason for the difference.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParamPolicy {
    /// Request keys stripped before dispatch, because the upstream rejects them.
    pub drop: Vec<String>,
    /// Request keys renamed before dispatch (`seed` -> `random_seed`).
    pub rename: BTreeMap<String, String>,
    /// Rewrite `response_format: {type: json_object}` into a permissive JSON
    /// schema, for upstreams that only implement the schema form.
    pub json_object_to_schema: bool,
    /// Applied when the caller stated no output bound at all.
    pub default_max_tokens: Option<u64>,
    /// Hard ceiling on the output bound sent upstream.
    pub max_tokens_cap: Option<u64>,
    /// Send at most one tool call; some upstreams reject parallel tool calls
    /// outright rather than degrading.
    pub force_single_tool_call: bool,
}

impl ParamPolicy {
    /// Whether this policy would change a request at all. A default policy is
    /// skipped entirely rather than walking the request body for nothing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Compat {
    pub max_tokens_field: Option<MaxTokensField>,
    pub supports_usage_in_streaming: Option<bool>,
    pub supports_finish_reason: Option<bool>,
    pub supports_developer_role: Option<bool>,
    pub tool_call_format: Option<ToolCallFormat>,
    pub structured_output_format: Option<StructuredOutputFormat>,
    pub thinking_format: Option<ThinkingFormat>,
    /// Provider quirks applied at request-build time. Optional and defaulted:
    /// an absent policy means "forward the request unchanged", which is what
    /// every existing catalog entry already expects.
    pub param_policy: ParamPolicy,
}

impl Compat {
    #[must_use]
    pub fn missing_required_fields(&self) -> Vec<&'static str> {
        let fields = [
            ("max_tokens_field", self.max_tokens_field.is_none()),
            (
                "supports_usage_in_streaming",
                self.supports_usage_in_streaming.is_none(),
            ),
            (
                "supports_finish_reason",
                self.supports_finish_reason.is_none(),
            ),
            (
                "supports_developer_role",
                self.supports_developer_role.is_none(),
            ),
            ("tool_call_format", self.tool_call_format.is_none()),
            (
                "structured_output_format",
                self.structured_output_format.is_none(),
            ),
            ("thinking_format", self.thinking_format.is_none()),
        ];
        fields
            .into_iter()
            .filter_map(|(name, missing)| missing.then_some(name))
            .collect()
    }
}
