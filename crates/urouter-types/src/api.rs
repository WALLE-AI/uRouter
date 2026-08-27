use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WireApi {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    Custom(String),
}
