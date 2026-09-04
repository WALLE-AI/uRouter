//! The outbound provider adapter layer.
//!
//! Before this crate, the whole outbound path was three `match model.api` arms
//! plus a `rewrite_request` free function inside the gateway binary. Three
//! consequences fell out of that shape:
//!
//! * **The URL path was a hardcoded three-way match.** Any provider whose chat
//!   path is not `{base}/chat/completions` — Gemini's
//!   `models/{id}:generateContent`, Cloudflare's account-scoped path, a
//!   job-polling relay — was inexpressible.
//! * **`WireApi::Custom(_)` was named but not implemented.** The catalog schema
//!   and serde both accepted it; three runtime sites rejected it with "custom
//!   provider APIs require an installed transport adapter". This crate is that
//!   extension point.
//! * **Five of `Compat`'s seven fields were dead.** They were validated and
//!   hashed into the catalog manifest and then never consulted at request-build
//!   time.
//!
//! The design goal is the one the reference implementation demonstrates: adding
//! a provider should be a catalog entry, not a code change. Around 34 of its 41
//! platforms are a single parameterised registration. Here the parameters live
//! in `ProviderSpec`/`ModelSpec` instead of a constructor call, so a new
//! provider ships as a signed, reviewable, rollback-able catalog revision.
//!
//! Everything here is pure: transports shape values and derive URLs, they never
//! perform I/O. The gateway owns the HTTP client, the credential resolution and
//! the timeouts.

mod anthropic;
mod gemini;
mod openai_chat;
mod openai_responses;
mod policy;

#[cfg(test)]
mod tests_support;

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;
use urouter_ai::{ModelSpec, ProviderSpec, catalog::CatalogSnapshot};
use urouter_types::{ModelId, WireApi};

pub use anthropic::anthropic_to_chat;
pub use openai_responses::responses_to_chat;
pub use policy::apply_param_policy;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    /// No transport is installed for this wire API. Carries the API name so the
    /// operator sees which one to install rather than a generic rejection.
    #[error("no transport is installed for the {0} provider API")]
    UnsupportedWireApi(String),
    #[error("request could not be converted for the upstream: {0}")]
    Conversion(String),
    #[error("request loses required semantics on this upstream: {0}")]
    SemanticLoss(String),
    #[error("{api} transport does not support streaming handoff")]
    StreamingUnsupported { api: String },
    #[error("upstream response could not be interpreted: {0}")]
    Response(String),
}

/// Everything a transport needs to shape one request, resolved by the caller.
pub struct TransportContext<'a> {
    pub provider: &'a ProviderSpec,
    pub model: &'a ModelSpec,
    /// The endpoint base, already templated and already overridden by any
    /// deployment-level `base_url`. Transports append their own path to it.
    pub base_url: &'a str,
}

impl TransportContext<'_> {
    /// The request quirks in force: the provider's, narrowed by the model's.
    #[must_use]
    pub fn param_policy(&self) -> urouter_ai::ParamPolicy {
        self.model
            .compat
            .param_policy
            .merged_over(&self.provider.param_policy)
    }

    /// Join the transport's path onto the base, tolerating a trailing slash on
    /// either side.
    #[must_use]
    pub fn join(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

/// One upstream wire format.
///
/// Implementations are stateless and shared; the registry hands out `&dyn`
/// references.
pub trait ProviderTransport: Send + Sync {
    /// The wire API this transport serves.
    fn wire(&self) -> WireApi;

    /// The full URL for a chat-shaped call.
    ///
    /// This is a method rather than a constant suffix precisely because it is
    /// not always a suffix: Gemini interpolates the model id into the path and
    /// picks a different verb for streaming.
    fn endpoint(&self, ctx: &TransportContext<'_>, streaming: bool) -> Result<String, TransportError>;

    /// Shape an OpenAI-Chat-shaped request into this upstream's wire format.
    ///
    /// The caller has already stripped the `urouter` contract block; the
    /// transport is responsible for the model id, the compat rewrites and the
    /// provider's parameter policy.
    fn build_request(
        &self,
        ctx: &TransportContext<'_>,
        request: &Value,
    ) -> Result<Value, TransportError>;

    /// Map a successful upstream body back to an OpenAI-Chat-shaped response.
    fn parse_response(
        &self,
        ctx: &TransportContext<'_>,
        body: Value,
    ) -> Result<Value, TransportError>;

    /// Whether this transport can stream. A `false` here is what produces a
    /// clear `StreamingUnsupported` instead of a hung or truncated stream.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Extra headers this wire format requires beyond the provider's own.
    fn extra_headers(&self, _ctx: &TransportContext<'_>) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

/// The installed transports, looked up by wire API.
///
/// Registration is explicit rather than inferred so that an unknown `api` in a
/// catalog entry fails loudly at dispatch with the API name, and so a catalog
/// feed cannot introduce a wire format this binary does not implement.
pub struct TransportRegistry {
    transports: Vec<Box<dyn ProviderTransport>>,
}

impl Default for TransportRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl TransportRegistry {
    #[must_use]
    pub fn with_builtins() -> Self {
        Self {
            transports: vec![
                Box::new(openai_chat::OpenAiChatTransport),
                Box::new(openai_responses::OpenAiResponsesTransport),
                Box::new(anthropic::AnthropicMessagesTransport),
                Box::new(gemini::GeminiTransport),
            ],
        }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self {
            transports: Vec::new(),
        }
    }

    #[must_use]
    pub fn install(mut self, transport: Box<dyn ProviderTransport>) -> Self {
        self.transports.push(transport);
        self
    }

    #[must_use]
    pub fn get(&self, api: &WireApi) -> Option<&dyn ProviderTransport> {
        self.transports
            .iter()
            .find(|transport| transport.wire() == *api)
            .map(AsRef::as_ref)
    }

    pub fn resolve(&self, api: &WireApi) -> Result<&dyn ProviderTransport, TransportError> {
        self.get(api)
            .ok_or_else(|| TransportError::UnsupportedWireApi(wire_api_name(api)))
    }

    /// Whether every model in a catalog has an installed transport.
    ///
    /// This is the gate a scheduled catalog feed runs before publishing: a
    /// revision that references a wire format this binary cannot speak must be
    /// rejected at load, not discovered on the first request that routes to it.
    #[must_use]
    pub fn unsupported_models(&self, catalog: &CatalogSnapshot) -> Vec<ModelId> {
        catalog
            .models()
            .filter(|model| self.get(&model.api).is_none())
            .map(|model| model.id.clone())
            .collect()
    }
}

#[must_use]
pub fn wire_api_name(api: &WireApi) -> String {
    match api {
        WireApi::OpenAiChat => "open_ai_chat".to_owned(),
        WireApi::OpenAiResponses => "open_ai_responses".to_owned(),
        WireApi::AnthropicMessages => "anthropic_messages".to_owned(),
        WireApi::Custom(name) => name.clone(),
        _ => "unknown".to_owned(),
    }
}

/// Whether the request asked for a streamed response.
#[must_use]
pub fn is_streaming(request: &Value) -> bool {
    request.get("stream").and_then(Value::as_bool) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_registry_covers_every_shipped_wire_api() {
        let registry = TransportRegistry::with_builtins();
        for api in [
            WireApi::OpenAiChat,
            WireApi::OpenAiResponses,
            WireApi::AnthropicMessages,
            WireApi::Custom("gemini_generate_content".to_owned()),
        ] {
            assert!(registry.get(&api).is_some(), "{api:?} has no transport");
        }
    }

    #[test]
    fn an_unknown_wire_api_names_itself_in_the_error() {
        let registry = TransportRegistry::with_builtins();
        let error = registry
            .resolve(&WireApi::Custom("cohere_v2".to_owned()))
            .err()
            .expect("an uninstalled wire api must not resolve");
        assert_eq!(
            error,
            TransportError::UnsupportedWireApi("cohere_v2".to_owned())
        );
        // The message must name the API — the old rejection said only "custom
        // provider APIs require an installed transport adapter", which does not
        // tell an operator what to install.
        assert!(error.to_string().contains("cohere_v2"));
    }

    #[test]
    fn joining_tolerates_slashes_on_either_side() {
        let base = "https://api.example.com/v1/";
        assert_eq!(
            format!("{}/{}", base.trim_end_matches('/'), "chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
    }
}
