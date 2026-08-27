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
