use serde_json::{Value, json};
use urouter_protocol::{LossPolicy, from_openai_chat, to_anthropic_messages};
use urouter_types::WireApi;

use crate::{ProviderTransport, TransportContext, TransportError, apply_param_policy, is_streaming};

/// The Anthropic Messages wire.
pub(crate) struct AnthropicMessagesTransport;

impl ProviderTransport for AnthropicMessagesTransport {
    fn wire(&self) -> WireApi {
        WireApi::AnthropicMessages
    }

    fn endpoint(
        &self,
        ctx: &TransportContext<'_>,
        _streaming: bool,
    ) -> Result<String, TransportError> {
        Ok(ctx.join("messages"))
    }

    fn build_request(
        &self,
        ctx: &TransportContext<'_>,
        request: &Value,
    ) -> Result<Value, TransportError> {
        if is_streaming(request) {
            return Err(TransportError::StreamingUnsupported {
                api: "anthropic_messages".to_owned(),
            });
        }
        let normalized =
            from_openai_chat(request).map_err(|error| TransportError::Conversion(error.to_string()))?;
        let (mut converted, _) = to_anthropic_messages(&normalized, LossPolicy::Reject)
            .map_err(|error| TransportError::SemanticLoss(error.to_string()))?;
        converted["model"] = ctx.model.upstream_id.clone().into();
        if let Some(object) = converted.as_object_mut() {
            apply_param_policy(object, &ctx.param_policy());
        }
        Ok(converted)
    }

    fn parse_response(
        &self,
        _ctx: &TransportContext<'_>,
        body: Value,
    ) -> Result<Value, TransportError> {
        Ok(anthropic_to_chat(&body))
    }
}

/// Collapse an Anthropic Messages response into a Chat Completions response.
pub fn anthropic_to_chat(body: &Value) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    if let Some(content) = body.get("content").and_then(Value::as_array) {
        for part in content {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                Some("tool_use") => tool_calls.push(json!({
                    "id": part.get("id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": part.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": part.get("input").cloned().unwrap_or_else(|| json!({})).to_string()
                    }
                })),
                _ => {}
            }
        }
    }
    let finish_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        message["tool_calls"] = tool_calls.into();
    }
    let input = body
        .pointer("/usage/input_tokens")
        .cloned()
        .unwrap_or(json!(0));
    let output = body
        .pointer("/usage/output_tokens")
        .cloned()
        .unwrap_or(json!(0));
    let total = input
        .as_u64()
        .unwrap_or(0)
        .saturating_add(output.as_u64().unwrap_or(0));
    json!({
        "id": body.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion",
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {"prompt_tokens": input, "completion_tokens": output, "total_tokens": total}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, model_with, provider};
    use urouter_ai::Compat;

    #[test]
    fn the_endpoint_is_the_messages_path() {
        let provider = provider();
        let model = model_with(Compat::default());
        let ctx = context(&provider, &model, "https://api.anthropic.com/v1");
        assert_eq!(
            AnthropicMessagesTransport.endpoint(&ctx, false).unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn tool_use_blocks_become_openai_tool_calls_with_stringified_arguments() {
        let body = json!({
            "id": "msg_1",
            "model": "claude",
            "content": [
                {"type": "text", "text": "sure"},
                {"type": "tool_use", "id": "tu_1", "name": "get_weather", "input": {"city": "SF"}}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 4}
        });
        let chat = anthropic_to_chat(&body);
        let call = &chat["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["function"]["name"], json!("get_weather"));
        // OpenAI states tool arguments as a JSON *string*, not an object.
        assert_eq!(call["function"]["arguments"], json!(r#"{"city":"SF"}"#));
        assert_eq!(chat["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(chat["usage"]["total_tokens"], json!(14));
    }
}
