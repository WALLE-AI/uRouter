use axum::http::HeaderMap;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

const TASK_HEADER: &str = "x-urouter-task-id";
const CONVERSATION_HEADER: &str = "x-urouter-conversation-id";
const BRANCH_HEADER: &str = "x-urouter-branch-id";
const TURN_HEADER: &str = "x-urouter-turn-id";
const CALL_KIND_HEADER: &str = "x-urouter-call-kind";
const MIGRATION_HEADER: &str = "x-urouter-migration-boundary";

#[derive(Debug, Error)]
pub(crate) enum AdapterError {
    #[error("unsupported agent adapter: {0}")]
    UnsupportedHarness(String),
    #[error("adapter requires model=urouter/auto")]
    AutoModelRequired,
    #[error("request already contains an urouter contract")]
    ContractConflict,
    #[error("required adapter header is missing: {0}")]
    MissingHeader(&'static str),
    #[error("adapter header is not valid UTF-8: {0}")]
    InvalidHeader(&'static str),
    #[error("unsupported adapter call kind: {0}")]
    InvalidCallKind(String),
    #[error("unsupported migration boundary: {0}")]
    InvalidMigrationBoundary(String),
    #[error("adapter request must be a JSON object")]
    InvalidRequest,
}

pub(crate) fn adapt_agent_request(
    harness: &str,
    headers: &HeaderMap,
    request: &mut Value,
) -> Result<(), AdapterError> {
    if !matches!(harness, "aionui" | "workbuddy") {
        return Err(AdapterError::UnsupportedHarness(harness.to_owned()));
    }
    let object = request
        .as_object_mut()
        .ok_or(AdapterError::InvalidRequest)?;
    if object.get("model").and_then(Value::as_str) != Some("urouter/auto") {
        return Err(AdapterError::AutoModelRequired);
    }
    if object.contains_key("urouter") {
        return Err(AdapterError::ContractConflict);
    }

    let conversation = required_header(headers, CONVERSATION_HEADER)?;
    let task = optional_header(headers, TASK_HEADER)?.unwrap_or_else(|| conversation.clone());
    let branch = optional_header(headers, BRANCH_HEADER)?.unwrap_or_else(|| "main".to_owned());
    let turn = required_header(headers, TURN_HEADER)?;
    let call_kind =
        optional_header(headers, CALL_KIND_HEADER)?.unwrap_or_else(|| "primary".to_owned());
    let (role, value_class, workload) = match call_kind.as_str() {
        "primary" => ("primary", "primary", "execute"),
        "plan" => ("primary", "primary", "plan"),
        "verify" => ("primary", "primary", "verify"),
        "title" => ("auxiliary", "auxiliary", "extract"),
        "summary" | "compress" => ("auxiliary", "auxiliary", "compress"),
        _ => return Err(AdapterError::InvalidCallKind(call_kind)),
    };
    let migration = optional_header(headers, MIGRATION_HEADER)?;
    if let Some(value) = migration.as_deref()
        && !matches!(
            value,
            "new_task"
                | "after_compaction"
                | "tool_round_completed"
                | "before_first_assistant_token"
                | "explicit_user_retry"
                | "terminal_provider_failure"
        )
    {
        return Err(AdapterError::InvalidMigrationBoundary(value.to_owned()));
    }

    let prompt_profile_hash = hash_projection(&prompt_profile_projection(object));
    let toolset_hash = hash_projection(object.get("tools").unwrap_or(&Value::Array(Vec::new())));
    object.insert(
        "urouter".to_owned(),
        json!({
            "contract_version": 2,
            "task": {"id": task},
            "agent": {
                "harness": harness,
                "prompt_profile_hash": prompt_profile_hash,
                "toolset_hash": toolset_hash
            },
            "call": {"role": role, "migration_boundary": migration},
            "trace": {
                "conversation": conversation,
                "branch": branch,
                "turn": turn
            },
            "hint": {"value_class": value_class, "workload": workload},
            "data_policy": {
                "recording": "metadata_only",
                "allow_training": false,
                "allow_remote_judge": false,
                "retention_days": 7
            }
        }),
    );
    Ok(())
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Result<String, AdapterError> {
    optional_header(headers, name)?.ok_or(AdapterError::MissingHeader(name))
}

fn optional_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<String>, AdapterError> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| AdapterError::InvalidHeader(name))
        })
        .transpose()
}

fn prompt_profile_projection(request: &Map<String, Value>) -> Value {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter(|message| {
                    matches!(
                        message.get("role").and_then(Value::as_str),
                        Some("system" | "developer")
                    )
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Array(messages)
}

fn hash_projection(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical_json(value, &mut canonical);
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

fn write_canonical_json(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => {
            output.push_str(&serde_json::to_string(value).expect("JSON string serialization"));
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).expect("JSON key serialization"));
                output.push(':');
                write_canonical_json(value, output);
            }
            output.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(call_kind: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONVERSATION_HEADER, "conversation-a".parse().unwrap());
        headers.insert(TURN_HEADER, "turn-a".parse().unwrap());
        headers.insert(CALL_KIND_HEADER, call_kind.parse().unwrap());
        headers
    }

    #[test]
    fn aionui_primary_maps_existing_ids_and_derives_stable_hashes() {
        let mut request = json!({
            "model": "urouter/auto",
            "messages": [
                {"content": "rules", "role": "system"},
                {"role": "user", "content": "first turn"}
            ],
            "tools": [{"function": {"parameters": {"type": "object"}, "name": "read"}, "type": "function"}]
        });
        adapt_agent_request("aionui", &headers("primary"), &mut request).unwrap();
        assert_eq!(request["urouter"]["task"]["id"], "conversation-a");
        assert_eq!(request["urouter"]["trace"]["branch"], "main");
        assert_eq!(request["urouter"]["call"]["role"], "primary");
        assert_eq!(request["urouter"]["hint"]["workload"], "execute");
        assert_eq!(
            request["urouter"]["agent"]["prompt_profile_hash"]
                .as_str()
                .unwrap()
                .len(),
            71
        );
        assert_eq!(
            request["urouter"]["agent"]["toolset_hash"]
                .as_str()
                .unwrap()
                .len(),
            71
        );
    }

    #[test]
    fn workbuddy_summary_is_auxiliary_and_object_order_does_not_change_hashes() {
        let mut first = json!({
            "model": "urouter/auto",
            "messages": [{"role": "developer", "content": {"a": 1, "b": 2}}],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {"a": 1, "b": 2}}}]
        });
        let mut second = json!({
            "model": "urouter/auto",
            "messages": [{"content": {"b": 2, "a": 1}, "role": "developer"}],
            "tools": [{"function": {"parameters": {"b": 2, "a": 1}, "name": "read"}, "type": "function"}]
        });
        adapt_agent_request("workbuddy", &headers("summary"), &mut first).unwrap();
        adapt_agent_request("workbuddy", &headers("summary"), &mut second).unwrap();
        assert_eq!(first["urouter"]["call"]["role"], "auxiliary");
        assert_eq!(first["urouter"]["hint"]["workload"], "compress");
        assert_eq!(
            first["urouter"]["agent"]["prompt_profile_hash"],
            second["urouter"]["agent"]["prompt_profile_hash"]
        );
        assert_eq!(
            first["urouter"]["agent"]["toolset_hash"],
            second["urouter"]["agent"]["toolset_hash"]
        );
    }

    #[test]
    fn adapter_rejects_ambiguous_or_incomplete_requests() {
        let mut conflicting = json!({"model": "urouter/auto", "urouter": {}});
        assert!(matches!(
            adapt_agent_request("aionui", &headers("primary"), &mut conflicting),
            Err(AdapterError::ContractConflict)
        ));
        let mut missing_turn = json!({"model": "urouter/auto", "messages": []});
        assert!(matches!(
            adapt_agent_request("aionui", &HeaderMap::new(), &mut missing_turn),
            Err(AdapterError::MissingHeader(CONVERSATION_HEADER))
        ));
    }
}
