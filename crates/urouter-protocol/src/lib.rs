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
