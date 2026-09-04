use serde_json::{Map, Value};
use urouter_ai::{MaxTokensField, ModelSpec};
use urouter_types::WireApi;

use crate::{ProviderTransport, TransportContext, TransportError, apply_param_policy};

/// The `OpenAI` Chat Completions wire, and the one most upstreams speak.
///
/// This transport is entirely parameterised by catalog data — model id, compat
/// rewrites, parameter policy — which is what makes "add a provider" a catalog
/// entry rather than a code change for the large majority of providers.
pub(crate) struct OpenAiChatTransport;

impl ProviderTransport for OpenAiChatTransport {
    fn wire(&self) -> WireApi {
        WireApi::OpenAiChat
    }

    fn endpoint(
        &self,
        ctx: &TransportContext<'_>,
        _streaming: bool,
    ) -> Result<String, TransportError> {
        Ok(ctx.join("chat/completions"))
    }

    fn build_request(
        &self,
        ctx: &TransportContext<'_>,
        request: &Value,
    ) -> Result<Value, TransportError> {
        let mut request = request.clone();
        let object = request
            .as_object_mut()
            .ok_or_else(|| TransportError::Conversion("request must be a JSON object".to_owned()))?;
        // The routing contract is ours, never the upstream's.
        object.remove("urouter");
        object.insert(
            "model".to_owned(),
            Value::String(ctx.model.upstream_id.clone()),
        );
        rewrite_compat(object, ctx.model);
        apply_param_policy(object, &ctx.param_policy());
        Ok(request)
    }

    fn parse_response(
        &self,
        _ctx: &TransportContext<'_>,
        body: Value,
    ) -> Result<Value, TransportError> {
        Ok(body)
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}

/// The `Compat` rewrites that apply to every OpenAI-shaped upstream.
pub(crate) fn rewrite_compat(object: &mut Map<String, Value>, model: &ModelSpec) {
    if model.compat.supports_developer_role == Some(false)
        && let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut)
    {
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("developer") {
                message["role"] = Value::String("system".to_owned());
            }
        }
    }
    normalize_max_tokens(object, model);
    apply_usage_in_streaming(object, model);
}

/// Collapse the three spellings of the output bound into whichever one this
/// upstream implements.
pub(crate) fn normalize_max_tokens(object: &mut Map<String, Value>, model: &ModelSpec) {
    let value = object
        .remove("max_tokens")
        .or_else(|| object.remove("max_completion_tokens"))
        .or_else(|| object.remove("max_output_tokens"));
    let Some(value) = value else {
        return;
    };
    let field = match model.compat.max_tokens_field {
        Some(MaxTokensField::MaxCompletionTokens) => "max_completion_tokens",
        Some(MaxTokensField::MaxOutputTokens) => "max_output_tokens",
        _ => "max_tokens",
    };
    object.insert(field.to_owned(), value);
}

/// Ask for usage on the final stream chunk, where the upstream supports it.
///
/// Without this the token counts for a streamed call are unknown, and both the
/// quota settlement and the cost settlement fall back to the estimate. It is
/// one of the `Compat` fields that was declared, hashed and never read.
fn apply_usage_in_streaming(object: &mut Map<String, Value>, model: &ModelSpec) {
    if model.compat.supports_usage_in_streaming != Some(true) {
        return;
    }
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let options = object
        .entry("stream_options")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(options) = options.as_object_mut() {
        // Never override an explicit caller preference.
        options
            .entry("include_usage")
            .or_insert(Value::Bool(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, model_with};
    use serde_json::json;
    use urouter_ai::Compat;

    #[test]
    fn the_routing_contract_never_reaches_the_upstream() {
        let model = model_with(Compat::default());
        let provider = crate::tests_support::provider();
        let ctx = context(&provider, &model, "https://api.example.com/v1");
        let built = OpenAiChatTransport
            .build_request(
                &ctx,
                &json!({"model": "urouter/auto", "urouter": {"task": {"id": "t"}}, "messages": []}),
            )
            .unwrap();
        assert!(built.get("urouter").is_none());
        assert_eq!(built["model"], json!("upstream-model"));
    }

    #[test]
    fn the_developer_role_is_downgraded_only_where_declared() {
        let provider = crate::tests_support::provider();
        let unsupported = model_with(Compat {
            supports_developer_role: Some(false),
            ..Compat::default()
        });
        let ctx = context(&provider, &unsupported, "https://api.example.com/v1");
        let built = OpenAiChatTransport
            .build_request(
                &ctx,
                &json!({"messages": [{"role": "developer", "content": "x"}]}),
            )
            .unwrap();
        assert_eq!(built["messages"][0]["role"], json!("system"));

        let supported = model_with(Compat {
            supports_developer_role: Some(true),
            ..Compat::default()
        });
        let ctx = context(&provider, &supported, "https://api.example.com/v1");
        let built = OpenAiChatTransport
            .build_request(
                &ctx,
                &json!({"messages": [{"role": "developer", "content": "x"}]}),
            )
            .unwrap();
        assert_eq!(built["messages"][0]["role"], json!("developer"));
    }

    #[test]
    fn the_output_bound_is_renamed_to_the_field_the_upstream_implements() {
        let provider = crate::tests_support::provider();
        let model = model_with(Compat {
            max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
            ..Compat::default()
        });
        let ctx = context(&provider, &model, "https://api.example.com/v1");
        let built = OpenAiChatTransport
            .build_request(&ctx, &json!({"max_tokens": 128, "messages": []}))
            .unwrap();
        assert_eq!(built["max_completion_tokens"], json!(128));
        assert!(built.get("max_tokens").is_none());
    }

    /// `supports_usage_in_streaming` was declared in the catalog, validated,
    /// hashed into the manifest — and never read. Without it a streamed call
    /// settles quota and cost from an estimate rather than the real counts.
    #[test]
    fn usage_is_requested_on_streams_where_the_upstream_reports_it() {
        let provider = crate::tests_support::provider();
        let model = model_with(Compat {
            supports_usage_in_streaming: Some(true),
            ..Compat::default()
        });
        let ctx = context(&provider, &model, "https://api.example.com/v1");

        let streamed = OpenAiChatTransport
            .build_request(&ctx, &json!({"stream": true, "messages": []}))
            .unwrap();
        assert_eq!(streamed["stream_options"]["include_usage"], json!(true));

        // Not a stream: nothing added.
        let unary = OpenAiChatTransport
            .build_request(&ctx, &json!({"messages": []}))
            .unwrap();
        assert!(unary.get("stream_options").is_none());

        // An explicit caller preference wins.
        let explicit = OpenAiChatTransport
            .build_request(
                &ctx,
                &json!({"stream": true, "stream_options": {"include_usage": false}, "messages": []}),
            )
            .unwrap();
        assert_eq!(explicit["stream_options"]["include_usage"], json!(false));
    }

    #[test]
    fn the_endpoint_is_the_conventional_chat_path() {
        let provider = crate::tests_support::provider();
        let model = model_with(Compat::default());
        let ctx = context(&provider, &model, "https://api.example.com/v1/");
        assert_eq!(
            OpenAiChatTransport.endpoint(&ctx, false).unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
    }
}
