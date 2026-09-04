//! Google's native `generateContent` wire.
//!
//! This transport exists as much to prove the abstraction as to serve Gemini.
//! Its endpoint is `models/{upstream_id}:generateContent` — the model id is in
//! the PATH, and streaming uses a different verb entirely
//! (`:streamGenerateContent?alt=sse`). Neither shape can be expressed as a
//! constant suffix appended to a base URL, which is exactly what the previous
//! hardcoded three-way `match model.api` could produce. Any provider whose path
//! is not `{base}/<fixed>` was unreachable.

use serde_json::{Map, Value, json};
use urouter_types::WireApi;

use crate::{ProviderTransport, TransportContext, TransportError};

/// The `WireApi::Custom` discriminant this transport registers under.
pub const GEMINI_WIRE_API: &str = "gemini_generate_content";

pub(crate) struct GeminiTransport;

impl ProviderTransport for GeminiTransport {
    fn wire(&self) -> WireApi {
        WireApi::Custom(GEMINI_WIRE_API.to_owned())
    }

    fn endpoint(
        &self,
        ctx: &TransportContext<'_>,
        streaming: bool,
    ) -> Result<String, TransportError> {
        let verb = if streaming {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        Ok(ctx.join(&format!("models/{}:{verb}", ctx.model.upstream_id)))
    }

    fn build_request(
        &self,
        ctx: &TransportContext<'_>,
        request: &Value,
    ) -> Result<Value, TransportError> {
        let messages = request
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| TransportError::Conversion("request has no messages".to_owned()))?;

        let mut contents = Vec::new();
        let mut system_parts = Vec::new();
        for message in messages {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
            let text = message_text(message);
            if text.is_empty() {
                continue;
            }
            // Gemini carries the system prompt out-of-band rather than as a
            // turn, and `developer` has no counterpart at all.
            if role == "system" || role == "developer" {
                system_parts.push(json!({"text": text}));
                continue;
            }
            contents.push(json!({
                "role": if role == "assistant" { "model" } else { "user" },
                "parts": [{"text": text}]
            }));
        }

        let mut body = Map::new();
        body.insert("contents".to_owned(), Value::Array(contents));
        if !system_parts.is_empty() {
            body.insert(
                "systemInstruction".to_owned(),
                json!({"parts": system_parts}),
            );
        }

        let mut generation = Map::new();
        if let Some(bound) = request
            .get("max_tokens")
            .or_else(|| request.get("max_completion_tokens"))
            .or_else(|| request.get("max_output_tokens"))
            .and_then(Value::as_u64)
        {
            let bound = ctx
                .param_policy()
                .max_tokens_cap
                .map_or(bound, |cap| bound.min(cap));
            generation.insert("maxOutputTokens".to_owned(), json!(bound));
        }
        for (from, to) in [("temperature", "temperature"), ("top_p", "topP")] {
            if let Some(value) = request.get(from) {
                generation.insert(to.to_owned(), value.clone());
            }
        }
        if !generation.is_empty() {
            body.insert("generationConfig".to_owned(), Value::Object(generation));
        }

        Ok(Value::Object(body))
    }

    fn parse_response(
        &self,
        ctx: &TransportContext<'_>,
        body: Value,
    ) -> Result<Value, TransportError> {
        let mut text = String::new();
        if let Some(candidates) = body.get("candidates").and_then(Value::as_array)
            && let Some(parts) = candidates
                .first()
                .and_then(|candidate| candidate.pointer("/content/parts"))
                .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(value) = part.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
        }
        let input = body
            .pointer("/usageMetadata/promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = body
            .pointer("/usageMetadata/candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Ok(json!({
            "id": "gemini",
            "object": "chat.completion",
            "model": ctx.model.upstream_id.clone(),
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": input,
                "completion_tokens": output,
                "total_tokens": input.saturating_add(output)
            }
        }))
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}

/// Flatten `OpenAI`'s two content shapes (a string, or an array of parts) into
/// plain text. Non-text parts are dropped here rather than mistranslated.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, model_with, provider};
    use urouter_ai::{Compat, ParamPolicy};

    /// The whole point of the trait: a path that interpolates the model id and
    /// switches verb for streaming. The previous hardcoded suffix match could
    /// not express either.
    #[test]
    fn the_model_id_lives_in_the_path_and_streaming_changes_the_verb() {
        let provider = provider();
        let model = model_with(Compat::default());
        let ctx = context(&provider, &model, "https://generativelanguage.googleapis.com/v1beta");
        assert_eq!(
            GeminiTransport.endpoint(&ctx, false).unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models/upstream-model:generateContent"
        );
        assert_eq!(
            GeminiTransport.endpoint(&ctx, true).unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models/upstream-model:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn the_system_prompt_moves_out_of_the_turn_sequence() {
        let provider = provider();
        let model = model_with(Compat::default());
        let ctx = context(&provider, &model, "https://example.com/v1beta");
        let built = GeminiTransport
            .build_request(
                &ctx,
                &json!({"messages": [
                    {"role": "system", "content": "be brief"},
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": "hello"}
                ]}),
            )
            .unwrap();
        assert_eq!(built["systemInstruction"]["parts"][0]["text"], json!("be brief"));
        assert_eq!(built["contents"].as_array().unwrap().len(), 2);
        assert_eq!(built["contents"][0]["role"], json!("user"));
        // Gemini spells the assistant turn "model".
        assert_eq!(built["contents"][1]["role"], json!("model"));
    }

    #[test]
    fn sampling_params_are_translated_into_generation_config() {
        let provider = provider();
        let model = model_with(Compat {
            param_policy: ParamPolicy {
                max_tokens_cap: Some(1_024),
                ..ParamPolicy::default()
            },
            ..Compat::default()
        });
        let ctx = context(&provider, &model, "https://example.com/v1beta");
        let built = GeminiTransport
            .build_request(
                &ctx,
                &json!({
                    "messages": [{"role": "user", "content": "hi"}],
                    "max_tokens": 8_000,
                    "temperature": 0.3,
                    "top_p": 0.9
                }),
            )
            .unwrap();
        let config = &built["generationConfig"];
        assert_eq!(config["maxOutputTokens"], json!(1_024), "cap must apply");
        assert_eq!(config["temperature"], json!(0.3));
        assert_eq!(config["topP"], json!(0.9));
        // Nothing OpenAI-shaped leaks through.
        assert!(built.get("max_tokens").is_none());
        assert!(built.get("messages").is_none());
    }

    #[test]
    fn the_response_maps_back_to_chat_shape_with_usage() {
        let provider = provider();
        let model = model_with(Compat::default());
        let ctx = context(&provider, &model, "https://example.com/v1beta");
        let chat = GeminiTransport
            .parse_response(
                &ctx,
                json!({
                    "candidates": [{"content": {"parts": [{"text": "hi "}, {"text": "there"}]}}],
                    "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2}
                }),
            )
            .unwrap();
        assert_eq!(chat["choices"][0]["message"]["content"], json!("hi there"));
        assert_eq!(chat["usage"]["total_tokens"], json!(7));
    }

    #[test]
    fn array_content_parts_are_flattened() {
        let provider = provider();
        let model = model_with(Compat::default());
        let ctx = context(&provider, &model, "https://example.com/v1beta");
        let built = GeminiTransport
            .build_request(
                &ctx,
                &json!({"messages": [{"role": "user", "content": [
                    {"type": "text", "text": "a"},
                    {"type": "image_url", "image_url": {"url": "http://x"}},
                    {"type": "text", "text": "b"}
                ]}]}),
            )
            .unwrap();
        assert_eq!(built["contents"][0]["parts"][0]["text"], json!("ab"));
    }
}
