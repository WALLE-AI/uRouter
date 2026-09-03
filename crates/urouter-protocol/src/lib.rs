use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use urouter_types::WireApi;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedRequest {
    pub schema_version: u16,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub response_schema: Option<Value>,
    pub max_output_tokens: Option<u64>,
    pub stream: bool,
    pub extensions: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { url: String },
    Json { value: Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct TransportCapabilities {
    pub api: WireApi,
    pub developer_role: bool,
    pub tools: bool,
    pub images: bool,
    pub structured_output: bool,
    pub reasoning: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossPolicy {
    Reject,
    AllowDocumented,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LossReport {
    pub source_api: WireApi,
    pub target_api: WireApi,
    pub losses: Vec<SemanticLoss>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticLoss {
    pub path: String,
    pub kind: LossKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossKind {
    RoleDowngraded,
    ReasoningDropped,
    ImageDropped,
    ToolDropped,
    StructuredOutputDropped,
}

pub fn from_openai_chat(value: &Value) -> Result<NormalizedRequest, ProtocolError> {
    let model = required_string(value, "model")?;
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(ProtocolError::MissingField("messages"))?
        .iter()
        .map(openai_message)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = parse_openai_tools(value.get("tools"))?;
    Ok(NormalizedRequest {
        schema_version: 1,
        model,
        messages,
        tools,
        response_schema: value.get("response_format").cloned(),
        max_output_tokens: value
            .get("max_completion_tokens")
            .or_else(|| value.get("max_tokens"))
            .and_then(Value::as_u64),
        stream: value
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        extensions: value.get("urouter").cloned().unwrap_or(Value::Null),
    })
}

pub fn to_openai_chat(
    request: &NormalizedRequest,
    capabilities: &TransportCapabilities,
    policy: LossPolicy,
) -> Result<(Value, LossReport), ProtocolError> {
    let mut report = LossReport {
        source_api: WireApi::OpenAiChat,
        target_api: capabilities.api.clone(),
        losses: Vec::new(),
    };
    let messages = request
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| render_openai_message(message, index, capabilities, &mut report))
        .collect::<Vec<_>>();
    validate_optional_capabilities(request, capabilities, &mut report);
    reject_losses(policy, &report)?;
    let mut output = json!({"model": request.model, "messages": messages});
    let object = output.as_object_mut().expect("object literal");
    if !request.tools.is_empty() && capabilities.tools {
        object.insert(
            "tools".to_owned(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({"type": "function", "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters
                        }})
                    })
                    .collect(),
            ),
        );
    }
    if let Some(response_schema) = &request.response_schema
        && capabilities.structured_output
    {
        object.insert("response_format".to_owned(), response_schema.clone());
    }
    if let Some(maximum) = request.max_output_tokens {
        object.insert("max_tokens".to_owned(), maximum.into());
    }
    if request.stream {
        object.insert("stream".to_owned(), true.into());
    }
    if !request.extensions.is_null() {
        object.insert("urouter".to_owned(), request.extensions.clone());
    }
    Ok((output, report))
}

pub fn from_openai_responses(value: &Value) -> Result<NormalizedRequest, ProtocolError> {
    let model = required_string(value, "model")?;
    let messages = match value.get("input") {
        Some(Value::String(text)) => vec![Message {
            role: Role::User,
            content: vec![ContentPart::Text { text: text.clone() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: None,
        }],
        Some(Value::Array(items)) => items
            .iter()
            .map(openai_responses_item)
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(ProtocolError::MissingField("input")),
    };
    Ok(NormalizedRequest {
        schema_version: 1,
        model,
        messages,
        tools: parse_responses_tools(value.get("tools"))?,
        response_schema: value
            .get("text")
            .and_then(|text| text.get("format"))
            .cloned(),
        max_output_tokens: value.get("max_output_tokens").and_then(Value::as_u64),
        stream: value
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        extensions: value.get("urouter").cloned().unwrap_or(Value::Null),
    })
}

pub fn to_openai_responses(
    request: &NormalizedRequest,
    policy: LossPolicy,
) -> Result<(Value, LossReport), ProtocolError> {
    let mut report = LossReport {
        source_api: WireApi::OpenAiChat,
        target_api: WireApi::OpenAiResponses,
        losses: Vec::new(),
    };
    let mut input = Vec::new();
    for (index, message) in request.messages.iter().enumerate() {
        if message.reasoning.is_some() {
            report.losses.push(SemanticLoss {
                path: format!("messages[{index}].reasoning"),
                kind: LossKind::ReasoningDropped,
                detail: "Responses does not accept replayed plaintext reasoning items".to_owned(),
            });
        }
        if message.role == Role::Tool {
            if let Some(call_id) = &message.tool_call_id {
                let output = message
                    .content
                    .iter()
                    .map(content_part_text)
                    .collect::<String>();
                input.push(
                    json!({"type": "function_call_output", "call_id": call_id, "output": output}),
                );
            }
            continue;
        }
        input.push(json!({
            "role": role_name(message.role),
            "content": message.content.iter().map(responses_content_part).collect::<Vec<_>>()
        }));
        input.extend(message.tool_calls.iter().map(|call| {
            json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments.to_string()
            })
        }));
    }
    reject_losses(policy, &report)?;
    let mut output = json!({"model": request.model, "input": input});
    let object = output.as_object_mut().expect("object literal");
    if !request.tools.is_empty() {
        object.insert(
            "tools".to_owned(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({"type": "function", "name": tool.name, "description": tool.description, "parameters": tool.parameters})
                    })
                    .collect(),
            ),
        );
    }
    if let Some(maximum) = request.max_output_tokens {
        object.insert("max_output_tokens".to_owned(), maximum.into());
    }
    if let Some(schema) = &request.response_schema {
        object.insert("text".to_owned(), json!({"format": schema}));
    }
    if request.stream {
        object.insert("stream".to_owned(), true.into());
    }
    Ok((output, report))
}

pub fn from_anthropic_messages(value: &Value) -> Result<NormalizedRequest, ProtocolError> {
    let model = required_string(value, "model")?;
    let mut messages = Vec::new();
    if let Some(system) = value.get("system") {
        messages.push(Message {
            role: Role::System,
            content: anthropic_content(system)?,
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: None,
        });
    }
    messages.extend(
        value
            .get("messages")
            .and_then(Value::as_array)
            .ok_or(ProtocolError::MissingField("messages"))?
            .iter()
            .map(anthropic_message)
            .collect::<Result<Vec<_>, _>>()?,
    );
    let tools = value
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    Ok(ToolDefinition {
                        name: required_string(tool, "name")?,
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        parameters: tool
                            .get("input_schema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object"})),
                    })
                })
                .collect::<Result<Vec<_>, ProtocolError>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(NormalizedRequest {
        schema_version: 1,
        model,
        messages,
        tools,
        response_schema: None,
        max_output_tokens: value.get("max_tokens").and_then(Value::as_u64),
        stream: value
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        extensions: value.get("urouter").cloned().unwrap_or(Value::Null),
    })
}

pub fn to_anthropic_messages(
    request: &NormalizedRequest,
    policy: LossPolicy,
) -> Result<(Value, LossReport), ProtocolError> {
    let mut report = LossReport {
        source_api: WireApi::OpenAiChat,
        target_api: WireApi::AnthropicMessages,
        losses: Vec::new(),
    };
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for (index, message) in request.messages.iter().enumerate() {
        if message.reasoning.is_some() {
            report.losses.push(SemanticLoss {
                path: format!("messages[{index}].reasoning"),
                kind: LossKind::ReasoningDropped,
                detail: "Anthropic does not accept replayed plaintext reasoning".to_owned(),
            });
        }
        if matches!(message.role, Role::System | Role::Developer) {
            if message.role == Role::Developer {
                report.losses.push(SemanticLoss {
                    path: format!("messages[{index}].role"),
                    kind: LossKind::RoleDowngraded,
                    detail: "developer role merged into Anthropic system".to_owned(),
                });
            }
            system.extend(message.content.iter().map(anthropic_content_part));
            continue;
        }
        let mut content = if message.role == Role::Tool {
            message.tool_call_id.as_ref().map_or_else(
                || {
                    report.losses.push(SemanticLoss {
                        path: format!("messages[{index}].tool_call_id"),
                        kind: LossKind::ToolDropped,
                        detail: "Anthropic tool_result requires a tool_use_id".to_owned(),
                    });
                    Vec::new()
                },
                |tool_call_id| {
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": message.content.iter().map(content_part_text).collect::<String>()
                    })]
                },
            )
        } else {
            message
                .content
                .iter()
                .map(anthropic_content_part)
                .collect::<Vec<_>>()
        };
        content.extend(message.tool_calls.iter().map(|call| {
            json!({"type": "tool_use", "id": call.id, "name": call.name, "input": call.arguments})
        }));
        messages.push(json!({
            "role": if message.role == Role::Assistant { "assistant" } else { "user" },
            "content": content
        }));
    }
    if request.response_schema.is_some() {
        report.losses.push(SemanticLoss {
            path: "response_schema".to_owned(),
            kind: LossKind::StructuredOutputDropped,
            detail: "Anthropic Messages has no equivalent response schema field".to_owned(),
        });
    }
    reject_losses(policy, &report)?;
    let mut output = json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.max_output_tokens.unwrap_or(1024)
    });
    let object = output.as_object_mut().expect("object literal");
    if !system.is_empty() {
        object.insert("system".to_owned(), Value::Array(system));
    }
    if !request.tools.is_empty() {
        object.insert(
            "tools".to_owned(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({"name": tool.name, "description": tool.description, "input_schema": tool.parameters})
                    })
                    .collect(),
            ),
        );
    }
    if request.stream {
        object.insert("stream".to_owned(), true.into());
    }
    Ok((output, report))
}

fn validate_optional_capabilities(
    request: &NormalizedRequest,
    capabilities: &TransportCapabilities,
    report: &mut LossReport,
) {
    if !request.tools.is_empty() && !capabilities.tools {
        report.losses.push(SemanticLoss {
            path: "tools".to_owned(),
            kind: LossKind::ToolDropped,
            detail: "target transport does not support tools".to_owned(),
        });
    }
    if request.response_schema.is_some() && !capabilities.structured_output {
        report.losses.push(SemanticLoss {
            path: "response_schema".to_owned(),
            kind: LossKind::StructuredOutputDropped,
            detail: "target transport does not support structured output".to_owned(),
        });
    }
}

fn render_openai_message(
    message: &Message,
    index: usize,
    capabilities: &TransportCapabilities,
    report: &mut LossReport,
) -> Value {
    let role = if message.role == Role::Developer && !capabilities.developer_role {
        report.losses.push(SemanticLoss {
            path: format!("messages[{index}].role"),
            kind: LossKind::RoleDowngraded,
            detail: "developer role rewritten to system".to_owned(),
        });
        "system"
    } else {
        role_name(message.role)
    };
    if message.reasoning.is_some() && !capabilities.reasoning {
        report.losses.push(SemanticLoss {
            path: format!("messages[{index}].reasoning"),
            kind: LossKind::ReasoningDropped,
            detail: "target transport does not preserve reasoning".to_owned(),
        });
    }
    let mut content = Vec::new();
    for (part_index, part) in message.content.iter().enumerate() {
        match part {
            ContentPart::Text { text } => content.push(json!({"type": "text", "text": text})),
            ContentPart::ImageUrl { url } if capabilities.images => {
                content.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            ContentPart::ImageUrl { .. } => report.losses.push(SemanticLoss {
                path: format!("messages[{index}].content[{part_index}]"),
                kind: LossKind::ImageDropped,
                detail: "target transport does not support images".to_owned(),
            }),
            ContentPart::Json { value } => {
                content.push(json!({"type": "text", "text": value.to_string()}));
            }
        }
    }
    let rendered_content =
        if content.len() == 1 && content[0].get("type").and_then(Value::as_str) == Some("text") {
            content[0].get("text").cloned().unwrap_or(Value::Null)
        } else {
            Value::Array(content)
        };
    let mut rendered = json!({"role": role, "content": rendered_content});
    let object = rendered.as_object_mut().expect("object literal");
    if capabilities.reasoning
        && let Some(reasoning) = &message.reasoning
    {
        object.insert("reasoning_content".to_owned(), reasoning.clone().into());
    }
    if !message.tool_calls.is_empty() && capabilities.tools {
        object.insert(
            "tool_calls".to_owned(),
            Value::Array(
                message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({"id": call.id, "type": "function", "function": {
                            "name": call.name, "arguments": call.arguments.to_string()
                        }})
                    })
                    .collect(),
            ),
        );
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        object.insert("tool_call_id".to_owned(), tool_call_id.clone().into());
    }
    rendered
}

fn openai_message(value: &Value) -> Result<Message, ProtocolError> {
    let role = parse_role(value.get("role").and_then(Value::as_str))?;
    let content = openai_content(value.get("content"))?;
    let tool_calls = value
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().map(openai_tool_call).collect())
        .transpose()?
        .unwrap_or_default();
    Ok(Message {
        role,
        content,
        tool_calls,
        tool_call_id: value
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reasoning: value
            .get("reasoning_content")
            .or_else(|| value.get("reasoning"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn openai_content(value: Option<&Value>) -> Result<Vec<ContentPart>, ProtocolError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![ContentPart::Text { text: text.clone() }]),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text" | "input_text") => Ok(ContentPart::Text {
                    text: required_string(part, "text")?,
                }),
                Some("image_url") => Ok(ContentPart::ImageUrl {
                    url: part
                        .pointer("/image_url/url")
                        .and_then(Value::as_str)
                        .ok_or(ProtocolError::MissingField("image_url.url"))?
                        .to_owned(),
                }),
                Some(other) => Err(ProtocolError::UnsupportedContent(other.to_owned())),
                None => Err(ProtocolError::MissingField("content.type")),
            })
            .collect(),
        Some(_) => Err(ProtocolError::InvalidField("content")),
    }
}

fn openai_tool_call(value: &Value) -> Result<ToolCall, ProtocolError> {
    let arguments = value
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::MissingField("function.arguments"))?;
    Ok(ToolCall {
        id: required_string(value, "id")?,
        name: value
            .pointer("/function/name")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingField("function.name"))?
            .to_owned(),
        arguments: serde_json::from_str(arguments)
            .map_err(|_| ProtocolError::InvalidField("function.arguments"))?,
    })
}

fn parse_openai_tools(value: Option<&Value>) -> Result<Vec<ToolDefinition>, ProtocolError> {
    value
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    let function = tool
                        .get("function")
                        .ok_or(ProtocolError::MissingField("tools.function"))?;
                    Ok(ToolDefinition {
                        name: required_string(function, "name")?,
                        description: function
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        parameters: function
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object"})),
                    })
                })
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn parse_responses_tools(value: Option<&Value>) -> Result<Vec<ToolDefinition>, ProtocolError> {
    value
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    Ok(ToolDefinition {
                        name: required_string(tool, "name")?,
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        parameters: tool
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object"})),
                    })
                })
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn openai_responses_item(value: &Value) -> Result<Message, ProtocolError> {
    let role = parse_role(value.get("role").and_then(Value::as_str))?;
    Ok(Message {
        role,
        content: openai_content(value.get("content"))?,
        tool_calls: Vec::new(),
        tool_call_id: None,
        reasoning: None,
    })
}

fn responses_content_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({"type": "input_text", "text": text}),
        ContentPart::ImageUrl { url } => json!({"type": "input_image", "image_url": url}),
        ContentPart::Json { value } => {
            json!({"type": "input_text", "text": value.to_string()})
        }
    }
}

fn content_part_text(part: &ContentPart) -> String {
    match part {
        ContentPart::Text { text } => text.clone(),
        ContentPart::ImageUrl { url } => url.clone(),
        ContentPart::Json { value } => value.to_string(),
    }
}

fn anthropic_message(value: &Value) -> Result<Message, ProtocolError> {
    let role = parse_role(value.get("role").and_then(Value::as_str))?;
    let parts = anthropic_content(
        value
            .get("content")
            .ok_or(ProtocolError::MissingField("content"))?,
    )?;
    let blocks = value.get("content").and_then(Value::as_array);
    let tool_calls = blocks
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|part| {
            Ok(ToolCall {
                id: required_string(part, "id")?,
                name: required_string(part, "name")?,
                arguments: part.get("input").cloned().unwrap_or_else(|| json!({})),
            })
        })
        .collect::<Result<Vec<_>, ProtocolError>>()?;
    let tool_call_id = blocks
        .into_iter()
        .flatten()
        .find(|part| part.get("type").and_then(Value::as_str) == Some("tool_result"))
        .and_then(|part| part.get("tool_use_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Message {
        role,
        content: parts,
        tool_calls,
        tool_call_id,
        reasoning: None,
    })
}

fn anthropic_content(value: &Value) -> Result<Vec<ContentPart>, ProtocolError> {
    match value {
        Value::String(text) => Ok(vec![ContentPart::Text { text: text.clone() }]),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    Some(required_string(part, "text").map(|text| ContentPart::Text { text }))
                }
                Some("tool_result") => Some(Ok(ContentPart::Json {
                    value: part.get("content").cloned().unwrap_or(Value::Null),
                })),
                Some("image") => part
                    .pointer("/source/url")
                    .and_then(Value::as_str)
                    .map(|url| {
                        Ok(ContentPart::ImageUrl {
                            url: url.to_owned(),
                        })
                    }),
                _ => None,
            })
            .collect(),
        _ => Err(ProtocolError::InvalidField("content")),
    }
}

fn anthropic_content_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({"type": "text", "text": text}),
        ContentPart::ImageUrl { url } => {
            json!({"type": "image", "source": {"type": "url", "url": url}})
        }
        ContentPart::Json { value } => json!({"type": "text", "text": value.to_string()}),
    }
}

// ── Gemini generateContent ──────────────────────────────────────────────────

/// Read Google's native `generateContent` request shape.
///
/// Two structural differences from every other inbound protocol, both of which
/// this has to undo:
///
/// * The system prompt is out-of-band in `systemInstruction`, not a turn.
/// * The assistant turn is spelled `model`.
///
/// The model id is NOT in the body — it is in the URL path — so the caller
/// supplies it.
pub fn from_gemini_generate_content(
    value: &Value,
    model: &str,
    stream: bool,
) -> Result<NormalizedRequest, ProtocolError> {
    let mut messages = Vec::new();
    if let Some(system) = value.get("systemInstruction") {
        let content = gemini_parts(system)?;
        if !content.is_empty() {
            messages.push(Message {
                role: Role::System,
                content,
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning: None,
            });
        }
    }
    for turn in value
        .get("contents")
        .and_then(Value::as_array)
        .ok_or(ProtocolError::MissingField("contents"))?
    {
        let role = match turn.get("role").and_then(Value::as_str) {
            Some("model") => Role::Assistant,
            _ => Role::User,
        };
        messages.push(Message {
            role,
            content: gemini_parts(turn)?,
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: None,
        });
    }
    if messages.is_empty() {
        return Err(ProtocolError::MissingField("contents"));
    }
    Ok(NormalizedRequest {
        schema_version: 1,
        model: model.to_owned(),
        messages,
        tools: Vec::new(),
        response_schema: None,
        max_output_tokens: value
            .pointer("/generationConfig/maxOutputTokens")
            .and_then(Value::as_u64),
        stream,
        extensions: value.get("urouter").cloned().unwrap_or(Value::Null),
    })
}

fn gemini_parts(node: &Value) -> Result<Vec<ContentPart>, ProtocolError> {
    let Some(parts) = node.get("parts").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut content = Vec::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            content.push(ContentPart::Text {
                text: text.to_owned(),
            });
        } else if let Some(uri) = part.pointer("/fileData/fileUri").and_then(Value::as_str) {
            content.push(ContentPart::ImageUrl {
                url: uri.to_owned(),
            });
        } else {
            // Inline blobs, function calls and executable code have no faithful
            // representation in the IR yet. Refusing is the honest answer:
            // dropping them silently would answer a different question than the
            // caller asked.
            return Err(ProtocolError::UnsupportedContent("gemini_part".to_owned()));
        }
    }
    Ok(content)
}

/// Render an `OpenAI` Chat response as a Gemini `generateContent` response.
#[must_use]
pub fn chat_to_gemini_response(chat: &Value) -> Value {
    let text = chat
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let finish = match chat
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        Some("length") => "MAX_TOKENS",
        Some("content_filter") => "SAFETY",
        _ => "STOP",
    };
    let prompt = chat
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = chat
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": text}]},
            "finishReason": finish,
            "index": 0
        }],
        "usageMetadata": {
            "promptTokenCount": prompt,
            "candidatesTokenCount": completion,
            "totalTokenCount": prompt.saturating_add(completion)
        },
        "modelVersion": chat.get("model").cloned().unwrap_or(Value::Null)
    })
}

// ── Ollama /api/chat ────────────────────────────────────────────────────────

/// Read Ollama's native chat request.
///
/// Close to `OpenAI` Chat, with the sampling knobs nested under `options` and the
/// output bound spelled `num_predict`. `stream` defaults to TRUE here, unlike
/// every other protocol — that is Ollama's documented default and clients rely
/// on it.
pub fn from_ollama_chat(value: &Value) -> Result<NormalizedRequest, ProtocolError> {
    let model = required_string(value, "model")?;
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(ProtocolError::MissingField("messages"))?
        .iter()
        .map(|message| {
            Ok(Message {
                role: parse_role(message.get("role").and_then(Value::as_str))?,
                content: vec![ContentPart::Text {
                    text: message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning: None,
            })
        })
        .collect::<Result<Vec<_>, ProtocolError>>()?;
    Ok(NormalizedRequest {
        schema_version: 1,
        model,
        messages,
        tools: Vec::new(),
        response_schema: None,
        max_output_tokens: value
            .pointer("/options/num_predict")
            .and_then(Value::as_u64),
        stream: value.get("stream").and_then(Value::as_bool).unwrap_or(true),
        extensions: value.get("urouter").cloned().unwrap_or(Value::Null),
    })
}

/// Render an `OpenAI` Chat response as an Ollama chat response.
#[must_use]
pub fn chat_to_ollama_response(chat: &Value, model: &str) -> Value {
    let text = chat
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "model": model,
        "created_at": "1970-01-01T00:00:00Z",
        "message": {"role": "assistant", "content": text},
        "done": true,
        "done_reason": chat
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop"),
        "prompt_eval_count": chat.pointer("/usage/prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
        "eval_count": chat.pointer("/usage/completion_tokens").and_then(Value::as_u64).unwrap_or(0)
    })
}

fn required_string(value: &Value, field: &'static str) -> Result<String, ProtocolError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(ProtocolError::MissingField(field))
}

fn parse_role(role: Option<&str>) -> Result<Role, ProtocolError> {
    match role {
        Some("system") => Ok(Role::System),
        Some("developer") => Ok(Role::Developer),
        Some("user") => Ok(Role::User),
        Some("assistant") => Ok(Role::Assistant),
        Some("tool") => Ok(Role::Tool),
        Some(other) => Err(ProtocolError::UnsupportedRole(other.to_owned())),
        None => Err(ProtocolError::MissingField("role")),
    }
}

const fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn reject_losses(policy: LossPolicy, report: &LossReport) -> Result<(), ProtocolError> {
    if policy == LossPolicy::Reject && !report.losses.is_empty() {
        return Err(ProtocolError::SemanticLoss(report.clone()));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("missing required field {0}")]
    MissingField(&'static str),
    #[error("invalid field {0}")]
    InvalidField(&'static str),
    #[error("unsupported role {0}")]
    UnsupportedRole(String),
    #[error("unsupported content part {0}")]
    UnsupportedContent(String),
    #[error("protocol conversion would lose semantics")]
    SemanticLoss(LossReport),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> TransportCapabilities {
        TransportCapabilities {
            api: WireApi::OpenAiChat,
            developer_role: true,
            tools: true,
            images: true,
            structured_output: true,
            reasoning: true,
        }
    }

    #[test]
    fn openai_round_trip_preserves_tools_reasoning_images_and_extensions() {
        let source = json!({
            "model": "urouter/auto",
            "messages": [
                {"role": "developer", "content": "policy"},
                {"role": "user", "content": [
                    {"type": "text", "text": "inspect"},
                    {"type": "image_url", "image_url": {"url": "https://example/image.png"}}
                ]},
                {"role": "assistant", "content": null, "reasoning_content": "brief", "tool_calls": [{
                    "id": "call-1", "type": "function", "function": {"name": "lookup", "arguments": "{\"id\":1}"}
                }]},
                {"role": "tool", "tool_call_id": "call-1", "content": "done"}
            ],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}],
            "response_format": {"type": "json_object"},
            "max_tokens": 100,
            "stream": true,
            "urouter": {"contract_version": 1}
        });
        let normalized = from_openai_chat(&source).unwrap();
        let (round_trip, report) =
            to_openai_chat(&normalized, &capabilities(), LossPolicy::Reject).unwrap();
        assert!(report.losses.is_empty());
        let reparsed = from_openai_chat(&round_trip).unwrap();
        assert_eq!(normalized, reparsed);
    }

    #[test]
    fn unsupported_transport_rejects_instead_of_silently_dropping() {
        let request = from_openai_chat(&json!({
            "model": "urouter/auto",
            "messages": [{"role": "developer", "content": "policy"}],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {}}}]
        }))
        .unwrap();
        let unsupported = TransportCapabilities {
            api: WireApi::Custom("limited".to_owned()),
            developer_role: false,
            tools: false,
            images: false,
            structured_output: false,
            reasoning: false,
        };
        let error = to_openai_chat(&request, &unsupported, LossPolicy::Reject).unwrap_err();
        assert!(matches!(error, ProtocolError::SemanticLoss(_)));
        let (_, report) =
            to_openai_chat(&request, &unsupported, LossPolicy::AllowDocumented).unwrap();
        assert_eq!(report.losses.len(), 2);
    }

    /// A transport that can carry nothing optional, for asserting that each
    /// unsupported feature is reported rather than dropped.
    fn bare_capabilities() -> TransportCapabilities {
        TransportCapabilities {
            api: WireApi::Custom("limited".to_owned()),
            developer_role: false,
            tools: false,
            images: false,
            structured_output: false,
            reasoning: false,
        }
    }

    fn chat(value: &Value) -> NormalizedRequest {
        from_openai_chat(value).unwrap()
    }

    fn kinds(report: &LossReport) -> Vec<LossKind> {
        report.losses.iter().map(|loss| loss.kind).collect()
    }

    // --- LossKind: one condition per variant, on every target that can produce it

    #[test]
    fn role_downgraded_is_reported_by_chat_and_by_anthropic() {
        let request = chat(&json!({
            "model": "m",
            "messages": [{"role": "developer", "content": "policy"}]
        }));

        let (_, report) =
            to_openai_chat(&request, &bare_capabilities(), LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::RoleDowngraded]);

        // Anthropic has no developer role at all: it merges into `system`.
        let (wire, report) = to_anthropic_messages(&request, LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::RoleDowngraded]);
        assert_eq!(wire["system"][0]["text"], "policy");
    }

    #[test]
    fn reasoning_dropped_is_reported_by_every_target_that_cannot_replay_it() {
        let request = chat(&json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": "x", "reasoning_content": "why"}]
        }));

        let (wire, report) =
            to_openai_chat(&request, &bare_capabilities(), LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::ReasoningDropped]);
        assert!(wire["messages"][0].get("reasoning_content").is_none());

        let (_, report) = to_openai_responses(&request, LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::ReasoningDropped]);

        let (_, report) = to_anthropic_messages(&request, LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::ReasoningDropped]);
    }

    #[test]
    fn image_dropped_is_reported_when_the_target_has_no_image_support() {
        let request = chat(&json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "https://example/i.png"}}
            ]}]
        }));
        let (wire, report) =
            to_openai_chat(&request, &bare_capabilities(), LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::ImageDropped]);
        // The text survives and the image is gone, rather than being emitted in
        // a shape the transport cannot read.
        assert_eq!(wire["messages"][0]["content"], "look");
    }

    #[test]
    fn tool_dropped_is_reported_for_an_unsupported_transport_and_for_a_tool_result_without_an_id() {
        let with_tools = chat(&json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {}}}]
        }));
        let (wire, report) = to_openai_chat(
            &with_tools,
            &bare_capabilities(),
            LossPolicy::AllowDocumented,
        )
        .unwrap();
        assert_eq!(kinds(&report), vec![LossKind::ToolDropped]);
        assert!(wire.get("tools").is_none());

        // Anthropic tool_result requires a tool_use_id; a tool message without
        // one cannot be represented at all.
        let orphan_result = chat(&json!({
            "model": "m",
            "messages": [{"role": "tool", "content": "done"}]
        }));
        let (_, report) =
            to_anthropic_messages(&orphan_result, LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::ToolDropped]);
    }

    #[test]
    fn structured_output_dropped_is_reported_by_chat_and_by_anthropic() {
        let request = chat(&json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_object"}
        }));

        let (wire, report) =
            to_openai_chat(&request, &bare_capabilities(), LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::StructuredOutputDropped]);
        assert!(wire.get("response_format").is_none());

        // Anthropic Messages has no response schema field at all.
        let (_, report) = to_anthropic_messages(&request, LossPolicy::AllowDocumented).unwrap();
        assert_eq!(kinds(&report), vec![LossKind::StructuredOutputDropped]);
    }

    /// The load-bearing property of the permissive policy: it tolerates loss but
    /// never hides it. A loss that is dropped without a report leaves no evidence
    /// on the `DecisionRecord` and cannot be attributed afterwards.
    #[test]
    fn allow_documented_still_reports_every_loss_it_tolerates() {
        let request = chat(&json!({
            "model": "m",
            "messages": [
                {"role": "developer", "content": "policy"},
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "https://example/i.png"}}
                ]},
                {"role": "assistant", "content": "x", "reasoning_content": "why"}
            ],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {}}}],
            "response_format": {"type": "json_object"}
        }));

        let rejected = to_openai_chat(&request, &bare_capabilities(), LossPolicy::Reject);
        assert!(matches!(rejected, Err(ProtocolError::SemanticLoss(_))));

        let (_, report) =
            to_openai_chat(&request, &bare_capabilities(), LossPolicy::AllowDocumented).unwrap();
        let reported = kinds(&report);
        for kind in [
            LossKind::RoleDowngraded,
            LossKind::ReasoningDropped,
            LossKind::ImageDropped,
            LossKind::ToolDropped,
            LossKind::StructuredOutputDropped,
        ] {
            assert!(reported.contains(&kind), "{kind:?} was tolerated silently");
        }
        // Every loss carries a path and a reason, so it can be attributed.
        for loss in &report.losses {
            assert!(!loss.path.is_empty());
            assert!(!loss.detail.is_empty());
        }
        assert_eq!(report.source_api, WireApi::OpenAiChat);
        assert_eq!(report.target_api, WireApi::Custom("limited".to_owned()));
    }

    // --- ProtocolError: one test per variant

    #[test]
    fn missing_required_fields_are_reported_by_name() {
        let cases: Vec<(&str, Value)> = vec![
            ("model", json!({"messages": []})),
            ("messages", json!({"model": "m"})),
            (
                "role",
                json!({"model": "m", "messages": [{"content": "x"}]}),
            ),
            (
                "content.type",
                json!({"model": "m", "messages": [{"role": "user", "content": [{"text": "x"}]}]}),
            ),
            (
                "image_url.url",
                json!({"model": "m", "messages": [{"role": "user",
                    "content": [{"type": "image_url", "image_url": {}}]}]}),
            ),
            (
                "function.arguments",
                json!({"model": "m", "messages": [{"role": "assistant", "content": null,
                    "tool_calls": [{"id": "c1", "function": {"name": "f"}}]}]}),
            ),
            (
                "tools.function",
                json!({"model": "m", "messages": [], "tools": [{"type": "function"}]}),
            ),
        ];
        for (field, value) in cases {
            match from_openai_chat(&value) {
                Err(ProtocolError::MissingField(reported)) => {
                    assert_eq!(reported, field, "wrong field reported for {field}");
                }
                other => panic!("{field} was not reported as missing: {other:?}"),
            }
        }

        // Responses and Anthropic report their own required fields.
        assert!(matches!(
            from_openai_responses(&json!({"model": "m"})),
            Err(ProtocolError::MissingField("input"))
        ));
        assert!(matches!(
            from_anthropic_messages(&json!({"model": "m"})),
            Err(ProtocolError::MissingField("messages"))
        ));
        assert!(matches!(
            from_anthropic_messages(&json!({"model": "m", "messages": [{"role": "user"}]})),
            Err(ProtocolError::MissingField("content"))
        ));
    }

    #[test]
    fn invalid_fields_are_distinguished_from_missing_ones() {
        // Content that is neither a string nor an array.
        assert!(matches!(
            from_openai_chat(&json!({"model": "m", "messages": [{"role": "user", "content": 7}]})),
            Err(ProtocolError::InvalidField("content"))
        ));
        // Tool-call arguments are a JSON string; unparseable text is invalid,
        // not missing.
        assert!(matches!(
            from_openai_chat(&json!({"model": "m", "messages": [{"role": "assistant",
                "content": null,
                "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": "not json"}}]}]})),
            Err(ProtocolError::InvalidField("function.arguments"))
        ));
        assert!(matches!(
            from_anthropic_messages(&json!({"model": "m",
                "messages": [{"role": "user", "content": 7}]})),
            Err(ProtocolError::InvalidField("content"))
        ));
    }

    #[test]
    fn an_unknown_role_is_rejected_rather_than_coerced() {
        match from_openai_chat(&json!({
            "model": "m",
            "messages": [{"role": "narrator", "content": "x"}]
        })) {
            Err(ProtocolError::UnsupportedRole(role)) => assert_eq!(role, "narrator"),
            other => panic!("expected UnsupportedRole, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_content_part_is_rejected_rather_than_dropped() {
        match from_openai_chat(&json!({
            "model": "m",
            "messages": [{"role": "user", "content": [{"type": "audio", "audio": {}}]}]
        })) {
            Err(ProtocolError::UnsupportedContent(kind)) => assert_eq!(kind, "audio"),
            other => panic!("expected UnsupportedContent, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_required_string_counts_as_missing() {
        assert!(matches!(
            from_openai_chat(&json!({"model": "", "messages": []})),
            Err(ProtocolError::MissingField("model"))
        ));
    }

    // --- Anthropic and Responses parsing: previously one happy path each

    #[test]
    fn anthropic_parsing_covers_system_tools_tool_use_tool_result_and_images() {
        let request = from_anthropic_messages(&json!({
            "model": "claude",
            "system": "be brief",
            "max_tokens": 256,
            "stream": true,
            "urouter": {"contract_version": 2},
            "tools": [{"name": "lookup", "description": "d", "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {"type": "url", "url": "https://example/i.png"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call-1", "name": "lookup", "input": {"id": 1}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call-1", "content": "done"}
                ]}
            ]
        }))
        .unwrap();

        // `system` becomes a leading System message rather than a side channel.
        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(
            request.messages[0].content,
            vec![ContentPart::Text {
                text: "be brief".to_owned()
            }]
        );
        assert_eq!(
            request.messages[1].content[1],
            ContentPart::ImageUrl {
                url: "https://example/i.png".to_owned()
            }
        );
        assert_eq!(request.messages[2].tool_calls[0].name, "lookup");
        assert_eq!(
            request.messages[2].tool_calls[0].arguments,
            json!({"id": 1})
        );
        assert_eq!(request.messages[3].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(request.tools[0].parameters, json!({"type": "object"}));
        assert_eq!(request.max_output_tokens, Some(256));
        assert!(request.stream);
        assert_eq!(request.extensions, json!({"contract_version": 2}));
        // Anthropic Messages carries no response schema.
        assert_eq!(request.response_schema, None);
    }

    /// A tool definition without `input_schema` defaults to an empty object
    /// schema rather than failing, so a provider that omits it stays routable.
    #[test]
    fn anthropic_tool_without_an_input_schema_defaults_to_an_object() {
        let request = from_anthropic_messages(&json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "lookup"}]
        }))
        .unwrap();
        assert_eq!(request.tools[0].parameters, json!({"type": "object"}));
        assert_eq!(request.tools[0].description, None);
    }

    /// Content blocks the IR has no representation for are skipped rather than
    /// rejected, because Anthropic adds block types independently of uRouter.
    #[test]
    fn anthropic_skips_unknown_content_blocks_instead_of_failing() {
        let request = from_anthropic_messages(&json!({
            "model": "claude",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "keep"},
                {"type": "thinking", "thinking": "ignore"}
            ]}]
        }))
        .unwrap();
        assert_eq!(
            request.messages[0].content,
            vec![ContentPart::Text {
                text: "keep".to_owned()
            }]
        );
    }

    #[test]
    fn responses_parsing_covers_flat_tools_array_input_and_text_format() {
        let request = from_openai_responses(&json!({
            "model": "gpt",
            "input": [
                {"role": "system", "content": "policy"},
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}]}
            ],
            "tools": [{"type": "function", "name": "lookup", "description": "d",
                       "parameters": {"type": "object"}}],
            "text": {"format": {"type": "json_schema"}},
            "max_output_tokens": 64,
            "stream": true,
            "urouter": {"contract_version": 2}
        }))
        .unwrap();

        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(
            request.messages[1].content,
            vec![ContentPart::Text {
                text: "hello".to_owned()
            }]
        );
        // Responses tools are flat, not nested under `function` as in Chat.
        assert_eq!(request.tools[0].name, "lookup");
        assert_eq!(request.tools[0].parameters, json!({"type": "object"}));
        assert_eq!(
            request.response_schema,
            Some(json!({"type": "json_schema"}))
        );
        assert_eq!(request.max_output_tokens, Some(64));
        assert!(request.stream);
        assert_eq!(request.extensions, json!({"contract_version": 2}));
    }

    #[test]
    fn responses_tool_without_a_name_is_rejected() {
        assert!(matches!(
            from_openai_responses(&json!({
                "model": "gpt",
                "input": "hi",
                "tools": [{"type": "function", "parameters": {}}]
            })),
            Err(ProtocolError::MissingField("name"))
        ));
    }

    /// Anthropic has no optional output bound, so the converter substitutes one.
    /// That default changes provider behaviour, so it is pinned rather than left
    /// as an implementation detail.
    #[test]
    fn anthropic_substitutes_a_default_max_tokens_when_the_request_omits_one() {
        let request = chat(&json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert_eq!(request.max_output_tokens, None);
        let (wire, report) = to_anthropic_messages(&request, LossPolicy::Reject).unwrap();
        assert!(report.losses.is_empty());
        assert_eq!(wire["max_tokens"], 1024);

        let bounded = chat(&json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 32
        }));
        assert_eq!(
            to_anthropic_messages(&bounded, LossPolicy::Reject)
                .unwrap()
                .0["max_tokens"],
            32
        );
    }

    #[test]
    fn anthropic_round_trip_preserves_system_tools_and_tool_calls() {
        let source = chat(&json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call-1", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"id\":1}"}
                }]},
                {"role": "tool", "tool_call_id": "call-1", "content": "done"}
            ],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}],
            "max_tokens": 32
        }));
        let (wire, report) = to_anthropic_messages(&source, LossPolicy::Reject).unwrap();
        assert!(report.losses.is_empty());
        let reparsed = from_anthropic_messages(&wire).unwrap();

        assert_eq!(reparsed.model, source.model);
        assert_eq!(reparsed.max_output_tokens, source.max_output_tokens);
        assert_eq!(reparsed.tools, source.tools);
        assert_eq!(reparsed.messages[0].role, Role::System);
        assert_eq!(
            reparsed.messages[0].content,
            vec![ContentPart::Text {
                text: "be brief".to_owned()
            }]
        );
        // The tool call survives with its identity and arguments intact.
        let call = &reparsed.messages[2].tool_calls[0];
        assert_eq!(call.id, "call-1");
        assert_eq!(call.name, "lookup");
        assert_eq!(call.arguments, json!({"id": 1}));
        // The tool result keeps the id that links it back to the call.
        assert_eq!(reparsed.messages[3].tool_call_id.as_deref(), Some("call-1"));
    }

    /// `to_openai_responses` emits tool traffic as `function_call` /
    /// `function_call_output` items, which carry no `role`. Reparsing therefore
    /// fails by design rather than inventing one. This pins the boundary of the
    /// Responses round trip so it is not mistaken for a defect.
    #[test]
    fn responses_tool_items_are_not_reparseable_and_say_so() {
        let source = chat(&json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": null, "tool_calls": [{
                "id": "call-1", "type": "function",
                "function": {"name": "lookup", "arguments": "{}"}
            }]}]
        }));
        let (wire, _) = to_openai_responses(&source, LossPolicy::Reject).unwrap();
        assert_eq!(wire["input"][1]["type"], "function_call");
        assert!(matches!(
            from_openai_responses(&wire),
            Err(ProtocolError::MissingField("role"))
        ));
    }

    #[test]
    fn responses_and_anthropic_share_the_same_ir() {
        let responses = from_openai_responses(&json!({
            "model": "urouter/auto",
            "input": "hello",
            "max_output_tokens": 20
        }))
        .unwrap();
        let anthropic = to_anthropic_messages(&responses, LossPolicy::Reject)
            .unwrap()
            .0;
        let reparsed = from_anthropic_messages(&anthropic).unwrap();
        assert_eq!(reparsed.messages, responses.messages);
        let response_wire = to_openai_responses(&responses, LossPolicy::Reject)
            .unwrap()
            .0;
        let response_round_trip = from_openai_responses(&response_wire).unwrap();
        assert_eq!(response_round_trip.messages, responses.messages);
    }
}
