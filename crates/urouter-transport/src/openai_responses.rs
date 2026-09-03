use serde_json::{Value, json};
use urouter_protocol::{LossPolicy, from_openai_chat, to_openai_responses};
use urouter_types::WireApi;

use crate::{ProviderTransport, TransportContext, TransportError, apply_param_policy, is_streaming};

/// The `OpenAI` Responses wire.
pub(crate) struct OpenAiResponsesTransport;

impl ProviderTransport for OpenAiResponsesTransport {
    fn wire(&self) -> WireApi {
        WireApi::OpenAiResponses
    }

    fn endpoint(
        &self,
        ctx: &TransportContext<'_>,
        _streaming: bool,
    ) -> Result<String, TransportError> {
        Ok(ctx.join("responses"))
    }

    fn build_request(
        &self,
        ctx: &TransportContext<'_>,
        request: &Value,
    ) -> Result<Value, TransportError> {
        if is_streaming(request) {
            return Err(TransportError::StreamingUnsupported {
                api: "open_ai_responses".to_owned(),
            });
        }
        let normalized =
            from_openai_chat(request).map_err(|error| TransportError::Conversion(error.to_string()))?;
        let (mut converted, _) = to_openai_responses(&normalized, LossPolicy::Reject)
            .map_err(|error| TransportError::SemanticLoss(error.to_string()))?;
        converted["model"] = ctx.model.upstream_id.clone().into();
        if let Some(object) = converted.as_object_mut() {
            apply_param_policy(object, &ctx.model.compat.param_policy);
        }
        Ok(converted)
    }

    fn parse_response(
        &self,
        _ctx: &TransportContext<'_>,
        body: Value,
    ) -> Result<Value, TransportError> {
        Ok(responses_to_chat(&body))
    }
}

/// Collapse a Responses object back into a Chat Completions response.
pub fn responses_to_chat(body: &Value) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if part.get("type").and_then(Value::as_str) == Some("output_text")
                                && let Some(value) = part.get("text").and_then(Value::as_str)
                            {
                                text.push_str(value);
                            }
                        }
                    }
                }
                Some("reasoning") => {
                    if let Some(parts) = item.get("summary").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(value) = part.get("text").and_then(Value::as_str) {
                                reasoning.push_str(value);
                            }
                        }
                    }
                }
                Some("function_call") => tool_calls.push(json!({
                    "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": item.get("arguments").cloned().unwrap_or_else(|| "{}".into())
                    }
                })),
                _ => {}
            }
        }
    }
    let mut message = json!({"role": "assistant", "content": text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let finish_reason = if message.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    json!({
        "id": body.get("id").cloned().unwrap_or_else(|| "resp".into()),
        "object": "chat.completion",
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": body.get("usage").cloned().unwrap_or(Value::Null)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, model_with, provider};
    use urouter_ai::Compat;

    #[test]
    fn streaming_is_refused_with_a_named_api_rather_than_hanging() {
        let provider = provider();
        let model = model_with(Compat::default());
        let ctx = context(&provider, &model, "https://api.example.com/v1");
        let error = OpenAiResponsesTransport
            .build_request(&ctx, &json!({"stream": true, "messages": []}))
            .unwrap_err();
        assert_eq!(
            error,
            TransportError::StreamingUnsupported {
                api: "open_ai_responses".to_owned()
            }
        );
    }

    #[test]
    fn the_response_collapses_text_reasoning_and_tool_calls() {
        let body = json!({
            "id": "resp_1",
            "model": "m",
            "output": [
                {"type": "reasoning", "summary": [{"text": "thinking"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}
            ]
        });
        let chat = responses_to_chat(&body);
        let message = &chat["choices"][0]["message"];
        assert_eq!(message["content"], json!("hi"));
        assert_eq!(message["reasoning_content"], json!("thinking"));
        assert_eq!(message["tool_calls"][0]["function"]["name"], json!("f"));
        assert_eq!(chat["choices"][0]["finish_reason"], json!("tool_calls"));
    }
}
