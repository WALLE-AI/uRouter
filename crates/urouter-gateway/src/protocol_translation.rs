//! Client-facing protocol translation.
//!
//! The Gateway speaks `OpenAI` Chat upstream and converts to the shape the
//! client asked for. Streaming conversion is incremental: SSE events are
//! rewritten as they arrive rather than buffered, so a translated stream keeps
//! the time-to-first-token of the underlying Chat stream.

use super::{
    Body, Bytes, GatewayError, HeaderValue, ProtocolResponse, ProtocolStreamState,
    ProtocolTranslationState, Response, StreamExt, Value, header, json, stream, to_bytes,
};

pub(crate) fn translate_chat_stream_response(
    response: Response,
    protocol: ProtocolResponse,
) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let state = ProtocolStreamState {
        upstream: Box::pin(body.into_data_stream()),
        buffer: Vec::new(),
        protocol,
        translation: ProtocolTranslationState::default(),
        eof: false,
    };
    let translated = stream::unfold(state, |mut state| async move {
        loop {
            if let Some(end) = find_sse_event_end(&state.buffer) {
                let event = state.buffer.drain(..end).collect::<Vec<_>>();
                while state
                    .buffer
                    .first()
                    .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
                {
                    state.buffer.remove(0);
                }
                let bytes =
                    translate_chat_sse_event(&event, state.protocol, &mut state.translation);
                if !bytes.is_empty() {
                    return Some((Ok::<Bytes, axum::Error>(Bytes::from(bytes)), state));
                }
                continue;
            }
            if state.eof {
                return None;
            }
            match state.upstream.next().await {
                Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
                Some(Err(error)) => return Some((Err(error), state)),
                None => {
                    state.eof = true;
                    if !state.buffer.is_empty() {
                        state.buffer.extend_from_slice(b"\n\n");
                    }
                }
            }
        }
    });
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    Response::from_parts(parts, Body::from_stream(translated))
}

pub(crate) fn find_sse_event_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\n\n")
}

pub(crate) fn translate_chat_sse_event(
    event: &[u8],
    protocol: ProtocolResponse,
    state: &mut ProtocolTranslationState,
) -> Vec<u8> {
    let source = String::from_utf8_lossy(event);
    let event_name = source
        .lines()
        .find_map(|line| line.strip_prefix("event:"))
        .map(str::trim);
    let data = source
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if event_name == Some("urouter.decision") {
        return match protocol {
            ProtocolResponse::Responses => named_sse("response.urouter", &data),
            ProtocolResponse::Anthropic => named_sse("urouter.decision", &data),
        };
    }
    if data == "[DONE]" {
        return finish_protocol_stream(protocol, state);
    }
    let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
        return Vec::new();
    };
    if state.response_id.is_empty() {
        chunk
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("urouter-stream")
            .clone_into(&mut state.response_id);
    }
    let mut output = Vec::new();
    if !state.started {
        state.started = true;
        match protocol {
            ProtocolResponse::Responses => append_json_sse(
                &mut output,
                "response.created",
                json!({"type": "response.created", "response": {"id": state.response_id, "status": "in_progress"}}),
            ),
            ProtocolResponse::Anthropic => append_json_sse(
                &mut output,
                "message_start",
                json!({"type": "message_start", "message": {"id": state.response_id, "type": "message", "role": "assistant", "content": [], "stop_reason": null}}),
            ),
        }
    }
    let Some(delta) = chunk.pointer("/choices/0/delta") else {
        return output;
    };
    if let Some(text) = delta.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        append_protocol_delta(&mut output, protocol, state, 0, "text", text, None, None);
    }
    if let Some(reasoning) = delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        append_protocol_delta(
            &mut output,
            protocol,
            state,
            1,
            "reasoning",
            reasoning,
            None,
            None,
        );
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool in tool_calls {
            let index = tool.get("index").and_then(Value::as_u64).unwrap_or(0) + 2;
            let arguments = tool
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            append_protocol_delta(
                &mut output,
                protocol,
                state,
                index,
                "tool",
                arguments,
                tool.get("id").and_then(Value::as_str),
                tool.pointer("/function/name").and_then(Value::as_str),
            );
        }
    }
    output
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_protocol_delta(
    output: &mut Vec<u8>,
    protocol: ProtocolResponse,
    state: &mut ProtocolTranslationState,
    index: u64,
    kind: &str,
    delta: &str,
    id: Option<&str>,
    name: Option<&str>,
) {
    match protocol {
        ProtocolResponse::Responses => match kind {
            "text" => append_json_sse(
                output,
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": delta}),
            ),
            "reasoning" => append_json_sse(
                output,
                "response.reasoning_text.delta",
                json!({"type": "response.reasoning_text.delta", "output_index": 0, "delta": delta}),
            ),
            "tool" => {
                if state.opened_blocks.insert(index) {
                    append_json_sse(
                        output,
                        "response.output_item.added",
                        json!({"type": "response.output_item.added", "output_index": index - 1, "item": {"type": "function_call", "id": id.unwrap_or("call"), "name": name.unwrap_or("tool"), "arguments": ""}}),
                    );
                }
                if !delta.is_empty() {
                    append_json_sse(
                        output,
                        "response.function_call_arguments.delta",
                        json!({"type": "response.function_call_arguments.delta", "output_index": index - 1, "delta": delta}),
                    );
                }
            }
            _ => {}
        },
        ProtocolResponse::Anthropic => {
            if state.opened_blocks.insert(index) {
                let content = match kind {
                    "tool" => {
                        json!({"type": "tool_use", "id": id.unwrap_or("call"), "name": name.unwrap_or("tool"), "input": {}})
                    }
                    "reasoning" => json!({"type": "thinking", "thinking": ""}),
                    _ => json!({"type": "text", "text": ""}),
                };
                append_json_sse(
                    output,
                    "content_block_start",
                    json!({"type": "content_block_start", "index": index, "content_block": content}),
                );
            }
            if !delta.is_empty() {
                let value = match kind {
                    "tool" => json!({"type": "input_json_delta", "partial_json": delta}),
                    "reasoning" => json!({"type": "thinking_delta", "thinking": delta}),
                    _ => json!({"type": "text_delta", "text": delta}),
                };
                append_json_sse(
                    output,
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index, "delta": value}),
                );
            }
        }
    }
}

pub(crate) fn finish_protocol_stream(
    protocol: ProtocolResponse,
    state: &mut ProtocolTranslationState,
) -> Vec<u8> {
    let mut output = Vec::new();
    match protocol {
        ProtocolResponse::Responses => append_json_sse(
            &mut output,
            "response.completed",
            json!({"type": "response.completed", "response": {"id": state.response_id, "status": "completed"}}),
        ),
        ProtocolResponse::Anthropic => {
            for index in &state.opened_blocks {
                append_json_sse(
                    &mut output,
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                );
            }
            append_json_sse(
                &mut output,
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}}),
            );
            append_json_sse(&mut output, "message_stop", json!({"type": "message_stop"}));
        }
    }
    output
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn append_json_sse(output: &mut Vec<u8>, event: &str, value: Value) {
    let data = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
    output.extend_from_slice(&named_sse(event, &data));
}

pub(crate) fn named_sse(event: &str, data: &str) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

pub(crate) async fn translate_chat_response(
    response: Response,
    protocol: ProtocolResponse,
) -> Result<Response, GatewayError> {
    if !response.status().is_success() {
        return Ok(response);
    }
    let (mut parts, body) = response.into_parts();
    let bytes = to_bytes(body, 16 * 1_024 * 1_024)
        .await
        .map_err(GatewayError::internal)?;
    let chat: Value = serde_json::from_slice(&bytes).map_err(GatewayError::internal)?;
    let translated = match protocol {
        ProtocolResponse::Responses => chat_to_responses(&chat),
        ProtocolResponse::Anthropic => chat_to_anthropic(&chat),
    };
    let body = serde_json::to_vec(&translated).map_err(GatewayError::internal)?;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(Response::from_parts(parts, Body::from(body)))
}

pub(crate) fn chat_to_responses(chat: &Value) -> Value {
    let message = chat
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_default();
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        content.push(json!({"type": "output_text", "text": text, "annotations": []}));
    }
    if let Some(reasoning) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
    {
        content.push(
            json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": reasoning}]}),
        );
    }
    let mut output = vec![json!({
        "type": "message",
        "id": chat.get("id").cloned().unwrap_or(Value::Null),
        "status": "completed",
        "role": "assistant",
        "content": content
    })];
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        output.extend(calls.iter().map(|call| {
            json!({
                "type": "function_call",
                "call_id": call.get("id").cloned().unwrap_or(Value::Null),
                "name": call.pointer("/function/name").cloned().unwrap_or(Value::Null),
                "arguments": call.pointer("/function/arguments").cloned().unwrap_or(Value::Null)
            })
        }));
    }
    json!({
        "id": chat.get("id").cloned().unwrap_or(Value::Null),
        "object": "response",
        "status": "completed",
        "model": chat.get("model").cloned().unwrap_or(Value::Null),
        "output": output,
        "usage": {
            "input_tokens": chat.pointer("/usage/prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": chat.pointer("/usage/completion_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": chat.pointer("/usage/total_tokens").cloned().unwrap_or(json!(0))
        },
        "urouter": chat.get("urouter").cloned().unwrap_or(Value::Null)
    })
}

pub(crate) fn chat_to_anthropic(chat: &Value) -> Value {
    let message = chat
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_default();
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        content.extend(calls.iter().map(|call| {
            let input = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .unwrap_or_else(|| json!({}));
            json!({
                "type": "tool_use",
                "id": call.get("id").cloned().unwrap_or(Value::Null),
                "name": call.pointer("/function/name").cloned().unwrap_or(Value::Null),
                "input": input
            })
        }));
    }
    let finish = chat
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    json!({
        "id": chat.get("id").cloned().unwrap_or(Value::Null),
        "type": "message",
        "role": "assistant",
        "model": chat.get("model").cloned().unwrap_or(Value::Null),
        "content": content,
        "stop_reason": if finish == "tool_calls" { "tool_use" } else { "end_turn" },
        "usage": {
            "input_tokens": chat.pointer("/usage/prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": chat.pointer("/usage/completion_tokens").cloned().unwrap_or(json!(0))
        },
        "urouter": chat.get("urouter").cloned().unwrap_or(Value::Null)
    })
}
