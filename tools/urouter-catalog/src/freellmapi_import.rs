//! Import `FreeLLMAPI`'s provider registry into uRouter catalog providers.
//!
//! Reads `server/src/providers/index.ts` and `server/src/lib/sampling-params.ts`
//! from a `FreeLLMAPI` checkout (MIT License, Copyright (c) 2026 Tashfeen
//! Ahmed) and emits `ProviderSpec` entries.
//!
//! ## Why the registry and not the model rows
//!
//! `FreeLLMAPI`'s model data lives in 26 imperative `SQLite` migrations that
//! insert, update and delete each other; the final set exists only after the
//! chain runs. Replaying that by parsing TypeScript would be unverifiable — and
//! the file's own comment says model data has been maintained in the published
//! catalog since V25, so a replay reproduces a set its authors no longer keep
//! current. The REGISTRY is different: it is 41 flat, declarative
//! registrations plus one policy table, and it is the file that changes when a
//! platform is added.
//!
//! Models therefore arrive the way uRouter gets every other fact — through
//! `sync discover` against each provider's own `/v1/models`, quarantined and
//! reviewed. This importer builds the pool's CAPACITY; discovery fills it.
//!
//! ## What is asserted and what is not
//!
//! Everything emitted here is transcribed from the source file: platform id,
//! display name, base URL, extra headers, keyless flag, timeout, and the
//! parameter policy. Nothing is inferred about a model. Confidence is
//! `community` because the upstream facts are `FreeLLMAPI`'s observations, not
//! this project's.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use thiserror::Error;

/// One `register(new XProvider({ ... }))` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisteredPlatform {
    pub adapter: String,
    pub platform: String,
    pub name: String,
    pub base_url: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
    pub keyless: bool,
    pub force_single_tool_call: bool,
    pub timeout_ms: Option<u64>,
}

/// One entry of `PLATFORM_PARAM_POLICIES`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PlatformPolicy {
    pub drop: Vec<String>,
    pub rename: BTreeMap<String, String>,
    pub json_object_to_schema: bool,
    pub default_max_tokens: Option<u64>,
    pub max_tokens_cap: Option<u64>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ImportError {
    #[error("no provider registrations found; is this providers/index.ts?")]
    NoRegistrations,
}

/// Strip `//` and `/* */` comments so a commented-out registration or a URL
/// mentioned in prose is never mistaken for a real one.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    let (mut in_line, mut in_block, mut in_string) = (false, false, None::<u8>);
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(quote) = in_string {
            out.push(byte as char);
            if byte == b'\\' && index + 1 < bytes.len() {
                out.push(bytes[index + 1] as char);
                index += 2;
                continue;
            }
            if byte == quote {
                in_string = None;
            }
            index += 1;
            continue;
        }
        if in_line {
            if byte == b'\n' {
                in_line = false;
                out.push('\n');
            }
            index += 1;
            continue;
        }
        if in_block {
            if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                in_block = false;
                index += 2;
                continue;
            }
            if byte == b'\n' {
                out.push('\n');
            }
            index += 1;
            continue;
        }
        match byte {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                in_line = true;
                index += 2;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                in_block = true;
                index += 2;
            }
            b'\'' | b'"' | b'`' => {
                in_string = Some(byte);
                out.push(byte as char);
                index += 1;
            }
            _ => {
                out.push(byte as char);
                index += 1;
            }
        }
    }
    out
}

/// The `{ ... }` starting at `open`, brace-matched and string-aware.
fn balanced_object(source: &str, open: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut depth = 0_i32;
    let mut in_string = None::<u8>;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        if let Some(quote) = in_string {
            if *byte == quote {
                in_string = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' | b'`' => in_string = Some(*byte),
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return source.get(open..=offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// A `key: 'value'` string field.
fn string_field(object: &str, key: &str) -> Option<String> {
    let at = find_key(object, key)?;
    let rest = object.get(at..)?;
    let start = rest.find(['\'', '"'])?;
    let quote = rest.as_bytes()[start];
    let end = rest.get(start + 1..)?.find(quote as char)?;
    rest.get(start + 1..start + 1 + end).map(ToOwned::to_owned)
}

fn bool_field(object: &str, key: &str) -> bool {
    find_key(object, key).is_some_and(|at| {
        object
            .get(at..)
            .and_then(|rest| rest.split(',').next())
            .is_some_and(|value| value.contains("true"))
    })
}

fn number_field(object: &str, key: &str) -> Option<u64> {
    let at = find_key(object, key)?;
    let rest = object.get(at..)?;
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    digits.parse().ok()
}

/// Offset just past `key:` at the object's own nesting level.
///
/// Matching `key:` anywhere would find it inside a nested object — `name:` in
/// `extraHeaders` is a real example — and silently read the wrong value.
fn find_key(object: &str, key: &str) -> Option<usize> {
    let bytes = object.as_bytes();
    let mut depth = 0_i32;
    let mut in_string = None::<u8>;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(quote) = in_string {
            if byte == quote {
                in_string = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' | b'`' => in_string = Some(byte),
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            _ => {
                if depth == 1
                    && object.get(index..).is_some_and(|rest| {
                        rest.starts_with(key)
                            && rest
                                .get(key.len()..)
                                .is_some_and(|tail| tail.trim_start().starts_with(':'))
                    })
                    && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
                {
                    let colon = object.get(index..)?.find(':')?;
                    return Some(index + colon + 1);
                }
            }
        }
        index += 1;
    }
    None
}

/// A nested `key: { 'a': 'b', ... }` string map.
fn map_field(object: &str, key: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Some(at) = find_key(object, key) else {
        return map;
    };
    let Some(open) = object.get(at..).and_then(|rest| rest.find('{')) else {
        return map;
    };
    let Some(inner) = balanced_object(object, at + open) else {
        return map;
    };
    let body = inner.trim_start_matches('{').trim_end_matches('}');
    for pair in split_top_level(body) {
        if let Some((raw_key, raw_value)) = pair.split_once(':') {
            let name = raw_key.trim().trim_matches(['\'', '"', ' ']).to_owned();
            let value = raw_value.trim().trim_matches(['\'', '"', ' ', ',']).to_owned();
            if !name.is_empty() {
                map.insert(name, value);
            }
        }
    }
    map
}

/// Split on commas that are not inside a nested structure or a string.
fn split_top_level(body: &str) -> Vec<&str> {
    let bytes = body.as_bytes();
    let mut parts = Vec::new();
    let (mut start, mut depth) = (0, 0_i32);
    let mut in_string = None::<u8>;
    for (index, byte) in bytes.iter().enumerate() {
        if let Some(quote) = in_string {
            if *byte == quote {
                in_string = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' | b'`' => in_string = Some(*byte),
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < body.len() {
        parts.push(&body[start..]);
    }
    parts
}

/// Module-level `const NAME = 'value'` declarations.
///
/// A subclass writes `baseUrl: POLLINATIONS_BASE_URL`, not a literal, so the
/// identifier has to be resolved or the provider imports with no endpoint.
fn const_strings(source: &str) -> BTreeMap<String, String> {
    let mut table = BTreeMap::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("const ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        let name = name.trim().trim_end_matches(|c: char| c == ':' || c.is_whitespace());
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            continue;
        }
        let value = value.trim().trim_end_matches(';').trim();
        if value.len() >= 2 && (value.starts_with('\'') || value.starts_with('"')) {
            let quote = value.as_bytes()[0] as char;
            if let Some(end) = value[1..].find(quote) {
                table.insert(name.to_owned(), value[1..=end].to_owned());
            }
        }
    }
    table
}

/// A `key: 'literal'` field, or `key: IDENTIFIER` resolved through `consts`.
fn string_or_const(
    object: &str,
    key: &str,
    consts: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(literal) = string_field(object, key) {
        // A literal wins, but an identifier lookup produces a literal-looking
        // slice too; disambiguate by checking the raw text after the colon.
        let at = find_key(object, key)?;
        let raw = object.get(at..)?.trim_start();
        if raw.starts_with('\'') || raw.starts_with('"') {
            return Some(literal);
        }
    }
    let at = find_key(object, key)?;
    let raw = object.get(at..)?.trim_start();
    let ident: String = raw
        .chars()
        .take_while(|c| c.is_ascii_uppercase() || *c == '_')
        .collect();
    consts.get(&ident).cloned()
}

/// Every platform registered in `providers/index.ts`, plus any registered by a
/// subclass calling `super({ ... })` in the files given to `parse_sources`.
#[cfg(test)]
pub(crate) fn parse_registry(source: &str) -> Result<Vec<RegisteredPlatform>, ImportError> {
    parse_sources(&[source])
}

/// Parse one or more provider sources into a deduplicated platform list.
pub(crate) fn parse_sources(sources: &[&str]) -> Result<Vec<RegisteredPlatform>, ImportError> {
    let mut platforms: Vec<RegisteredPlatform> = Vec::new();
    for source in sources {
        for platform in parse_one(source) {
            if !platforms.iter().any(|known| known.platform == platform.platform) {
                platforms.push(platform);
            }
        }
    }
    if platforms.is_empty() {
        return Err(ImportError::NoRegistrations);
    }
    Ok(platforms)
}

fn parse_one(source: &str) -> Vec<RegisteredPlatform> {
    let source = strip_comments(source);
    let consts = const_strings(&source);
    let mut platforms = Vec::new();
    for marker in ["register(new ", "super("] {
        platforms.extend(parse_marked(&source, marker, &consts));
    }
    // A dedicated adapter declares its identity as class fields rather than as
    // constructor arguments, so it is invisible to the two scanners above.
    if platforms.is_empty()
        && let Some(declared) = parse_class_fields(&source, &consts)
    {
        platforms.push(declared);
    }
    platforms
}

/// A dedicated adapter class: `readonly platform = 'x'`, `readonly name = '…'`.
///
/// The base URL is taken from the file's single URL-valued module constant, or
/// from a `readonly baseUrl` field. A file with SEVERAL candidate URLs (an
/// account-scoped path built inline, a regional fallback) yields `None` rather
/// than a guess — those providers need a reviewed entry, which is also why they
/// have a dedicated adapter in the first place.
fn parse_class_fields(
    source: &str,
    consts: &BTreeMap<String, String>,
) -> Option<RegisteredPlatform> {
    let platform = readonly_field(source, "platform")?;
    let name = readonly_field(source, "name")?;
    let base_url = readonly_field(source, "baseUrl").or_else(|| {
        let urls: Vec<&String> = consts
            .values()
            .filter(|value| value.starts_with("http") && !value.contains('$'))
            .collect();
        // Exactly one candidate, or the choice is not ours to make.
        (urls.len() == 1).then(|| urls[0].clone())
    })?;
    Some(RegisteredPlatform {
        adapter: "DedicatedProvider".to_owned(),
        platform,
        name,
        base_url: Some(base_url),
        extra_headers: BTreeMap::new(),
        keyless: source.contains("keyless = true"),
        force_single_tool_call: false,
        timeout_ms: None,
    })
}

/// `readonly <field> = '<value>'`, tolerating a trailing `as const`.
fn readonly_field(source: &str, field: &str) -> Option<String> {
    for line in source.lines() {
        let line = line.trim();
        if !line.contains("readonly ") {
            continue;
        }
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        let declared = left
            .rsplit(|c: char| c.is_whitespace() || c == ':')
            .find(|token| !token.is_empty())
            .unwrap_or("");
        // `readonly platform: Platform = 'x'` puts the type after the name.
        let declared = if declared == "Platform" || declared == "string" {
            left.split_whitespace()
                .nth(1)
                .unwrap_or("")
                .trim_end_matches(':')
        } else {
            declared
        };
        if declared != field {
            continue;
        }
        let value = right.trim();
        if value.len() >= 2 && (value.starts_with('\'') || value.starts_with('"')) {
            let quote = value.as_bytes()[0] as char;
            if let Some(end) = value[1..].find(quote) {
                return Some(value[1..=end].to_owned());
            }
        }
    }
    None
}

fn parse_marked(
    source: &str,
    marker: &str,
    consts: &BTreeMap<String, String>,
) -> Vec<RegisteredPlatform> {
    let source = source.to_owned();
    let mut platforms = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = source.get(cursor..).and_then(|rest| rest.find(marker)) {
        let at = cursor + offset + marker.len();
        cursor = at;
        // `super(` names no adapter class; the subclass's own wire is resolved
        // from the platform id by the caller.
        let adapter = if marker == "super(" {
            "OpenAICompatProvider".to_owned()
        } else {
            match source
                .get(at..)
                .and_then(|rest| rest.find('(').map(|end| rest[..end].trim().to_owned()))
            {
                Some(adapter) => adapter,
                None => continue,
            }
        };
        // The argument list starts right after the adapter name. A registration
        // whose first argument is not an object literal — `new SailProvider()`,
        // or one taking only a timeout — carries nothing importable.
        let after_paren = if marker == "super(" {
            at
        } else {
            match source.get(at..).and_then(|rest| rest.find('(')) {
                Some(paren) => at + paren + 1,
                None => continue,
            }
        };
        let Some(open) = source
            .get(after_paren..)
            .map(|rest| rest.len() - rest.trim_start().len())
            .filter(|offset| {
                source
                    .as_bytes()
                    .get(after_paren + offset)
                    .is_some_and(|byte| *byte == b'{')
            })
        else {
            continue;
        };
        let Some(object) = balanced_object(&source, after_paren + open) else {
            continue;
        };
        cursor = after_paren + open + object.len();
        let Some(platform) = string_field(object, "platform") else {
            continue;
        };
        let Some(name) = string_field(object, "name") else {
            continue;
        };
        platforms.push(RegisteredPlatform {
            adapter,
            platform,
            name,
            // An empty base URL is a placeholder, not an endpoint: the
            // reference implementation registers `custom` that way so its
            // lookups behave, then builds the real instance per key at runtime.
            base_url: string_or_const(object, "baseUrl", consts)
                .filter(|url| !url.trim().is_empty()),
            extra_headers: map_field(object, "extraHeaders"),
            keyless: bool_field(object, "keyless"),
            force_single_tool_call: bool_field(object, "forceSingleToolCall"),
            timeout_ms: number_field(object, "timeoutMs"),
        });
    }
    platforms
}

/// The `PLATFORM_PARAM_POLICIES` table.
pub(crate) fn parse_policies(source: &str) -> BTreeMap<String, PlatformPolicy> {
    let source = strip_comments(source);
    let mut policies = BTreeMap::new();
    let Some(at) = source.find("PLATFORM_PARAM_POLICIES") else {
        return policies;
    };
    let Some(open) = source.get(at..).and_then(|rest| rest.find('{')) else {
        return policies;
    };
    let Some(table) = balanced_object(&source, at + open) else {
        return policies;
    };
    let body = table
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(table);
    for entry in split_top_level(body) {
        let Some((platform, rest)) = entry.split_once(':') else {
            continue;
        };
        let platform = platform.trim().trim_matches(['\'', '"']).to_owned();
        if platform.is_empty() {
            continue;
        }
        let Some(open) = rest.find('{') else { continue };
        let Some(object) = balanced_object(rest, open) else {
            continue;
        };
        policies.insert(
            platform,
            PlatformPolicy {
                drop: string_array(object, "drop"),
                rename: map_field(object, "rename"),
                json_object_to_schema: bool_field(object, "jsonObjectToSchema"),
                default_max_tokens: number_field(object, "defaultMaxTokens"),
                max_tokens_cap: number_field(object, "maxTokensCap"),
            },
        );
    }
    policies
}

/// A `key: ['a', 'b']` array of strings.
///
/// A spread (`[...EXTENDED_SAMPLING_KEYS]`) yields nothing rather than a
/// guess — the importer reports what the file literally says.
fn string_array(object: &str, key: &str) -> Vec<String> {
    let Some(at) = find_key(object, key) else {
        return Vec::new();
    };
    let Some(rest) = object.get(at..) else {
        return Vec::new();
    };
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let Some(close) = rest.get(open..).and_then(|tail| tail.find(']')) else {
        return Vec::new();
    };
    rest[open + 1..open + close]
        .split(',')
        .map(|item| item.trim().trim_matches(['\'', '"']).to_owned())
        .filter(|item| !item.is_empty() && !item.starts_with("..."))
        .collect()
}

/// The uRouter wire API a `FreeLLMAPI` adapter class corresponds to.
fn wire_api_for(adapter: &str) -> Value {
    match adapter {
        // Google speaks its own generateContent wire.
        "GoogleProvider" => json!({"custom": "gemini_generate_content"}),
        // Sail's stable surface is the Responses API.
        "SailProvider" => json!("open_ai_responses"),
        _ => json!("open_ai_chat"),
    }
}

/// The environment variable a platform's key is read from.
fn credential_env(platform: &str) -> String {
    format!(
        "{}_API_KEY",
        platform.to_uppercase().replace(['-', '.'], "_")
    )
}

/// Build uRouter provider entries from a parsed registry.
///
/// `checked_at` is the caller's date; confidence is always `community` because
/// these are `FreeLLMAPI`'s observations rather than this project's.
pub(crate) fn to_provider_entries(
    platforms: &[RegisteredPlatform],
    policies: &BTreeMap<String, PlatformPolicy>,
    checked_at: &str,
    source_path: &str,
) -> Vec<Value> {
    let mut entries = Vec::new();
    for platform in platforms {
        // A platform with no base URL is built per-key at runtime by the
        // reference implementation (its `custom` relay); there is nothing
        // static to import.
        let Some(base_url) = &platform.base_url else {
            continue;
        };
        let auth = if platform.keyless {
            // No credential to send. `allow_remote` is the explicit
            // acknowledgement that an unauthenticated request leaves the host.
            json!({"kind": "none", "allow_remote": true})
        } else {
            json!({"kind": "api_key_env", "env": credential_env(&platform.platform)})
        };
        let policy = policies.get(&platform.platform).cloned().unwrap_or_default();
        let mut param_policy = serde_json::Map::new();
        if !policy.drop.is_empty() {
            param_policy.insert("drop".to_owned(), json!(policy.drop));
        }
        if !policy.rename.is_empty() {
            param_policy.insert("rename".to_owned(), json!(policy.rename));
        }
        if policy.json_object_to_schema {
            param_policy.insert("json_object_to_schema".to_owned(), json!(true));
        }
        if let Some(default) = policy.default_max_tokens {
            param_policy.insert("default_max_tokens".to_owned(), json!(default));
        }
        if let Some(cap) = policy.max_tokens_cap {
            param_policy.insert("max_tokens_cap".to_owned(), json!(cap));
        }
        if platform.force_single_tool_call {
            param_policy.insert("force_single_tool_call".to_owned(), json!(true));
        }

        let mut entry = serde_json::Map::new();
        entry.insert("id".to_owned(), json!(platform.platform));
        entry.insert("name".to_owned(), json!(platform.name));
        entry.insert("base_url".to_owned(), json!(base_url));
        entry.insert("built_in".to_owned(), json!(false));
        entry.insert("auth".to_owned(), auth);
        if !platform.extra_headers.is_empty() {
            entry.insert("headers".to_owned(), json!(platform.extra_headers));
        }
        if !param_policy.is_empty() {
            entry.insert("param_policy".to_owned(), Value::Object(param_policy));
        }
        if let Some(timeout) = platform.timeout_ms {
            entry.insert("timeout_millis".to_owned(), json!(timeout));
        }
        entry.insert(
            "source".to_owned(),
            json!({
                "source": format!("{source_path} ({} adapter)", platform.adapter),
                "checked_at": checked_at,
                "confidence": "community"
            }),
        );
        entries.push(Value::Object(entry));
    }
    entries
}

/// The wire API each imported platform should be discovered and served over.
pub(crate) fn wire_api_hint(platform: &RegisteredPlatform) -> Value {
    wire_api_for(&platform.adapter)
}

/// Build discovery instances for the imported platforms.
///
/// This is the half that makes the pool fill itself: an instance tells
/// `sync discover` where a platform's `/v1/models` lives and which credential
/// to present. Every imported instance is `enabled: false` — a provider whose
/// key is not configured must not be polled, and enabling it is the operator's
/// statement that they have one.
pub(crate) fn to_instance_entries(platforms: &[RegisteredPlatform]) -> Vec<Value> {
    let mut instances = Vec::new();
    for platform in platforms {
        let Some(base_url) = &platform.base_url else {
            continue;
        };
        let wire = wire_api_hint(platform);
        // Only OpenAI-shaped platforms expose a `/v1/models` this tool can read.
        // Emitting a discovery config for the others would produce an instance
        // that fails on every run.
        if wire != json!("open_ai_chat") {
            continue;
        }
        let id = format!("{}-main", platform.platform);
        let mut discovery = serde_json::Map::from_iter([
            ("kind".to_owned(), json!("open_ai_models")),
            (
                "url".to_owned(),
                json!(format!("{}/models", base_url.trim_end_matches('/'))),
            ),
            ("min_request_interval_ms".to_owned(), json!(250)),
        ]);
        if !platform.keyless {
            discovery.insert(
                "auth_env".to_owned(),
                json!(credential_env(&platform.platform)),
            );
        }
        let mut runtime = serde_json::Map::from_iter([
            ("base_url".to_owned(), json!(base_url)),
            ("protocols".to_owned(), json!([wire])),
        ]);
        if !platform.keyless {
            runtime.insert(
                "auth_env".to_owned(),
                json!(credential_env(&platform.platform)),
            );
        }
        instances.push(json!({
            "id": id,
            "vendor": platform.platform,
            "catalog_provider_id": platform.platform,
            "runtime": runtime,
            "discovery": discovery,
            "credential_scope": format!("{}-main", platform.platform),
            "enabled": false
        }));
    }
    instances
}

/// Merge instances into the provider registry, leaving existing ones alone.
pub(crate) fn merge_into_instances(
    path: &std::path::Path,
    entries: &[Value],
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut document: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let instances = document
        .get_mut("instances")
        .and_then(Value::as_array_mut)
        .ok_or("registry has no instances array")?;
    let existing: Vec<String> = instances
        .iter()
        .filter_map(|entry| entry.get("vendor").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect();
    let mut added = 0;
    for entry in entries {
        let Some(vendor) = entry.get("vendor").and_then(Value::as_str) else {
            continue;
        };
        if existing.iter().any(|known| known == vendor) {
            continue;
        }
        instances.push(entry.clone());
        added += 1;
    }
    let mut serialized = serde_json::to_string_pretty(&document)?;
    serialized.push('\n');
    std::fs::write(path, serialized)?;
    Ok(added)
}

/// Merge provider entries into a catalog file, never overwriting one that is
/// already there.
///
/// An existing entry is left alone on purpose: a hand-reviewed provider may
/// carry corrections this importer cannot know about, and silently replacing it
/// would discard that review on the next import.
pub(crate) fn merge_into_catalog(
    path: &std::path::Path,
    entries: &[Value],
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut document: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let providers = document
        .get_mut("providers")
        .and_then(Value::as_array_mut)
        .ok_or("catalog has no providers array")?;
    let existing: Vec<String> = providers
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect();
    let mut added = 0;
    for entry in entries {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if existing.iter().any(|known| known == id) {
            continue;
        }
        providers.push(entry.clone());
        added += 1;
    }
    providers.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .cmp(&b.get("id").and_then(Value::as_str))
    });
    let mut serialized = serde_json::to_string_pretty(&document)?;
    serialized.push('\n');
    std::fs::write(path, serialized)?;
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = r"
import { OpenAICompatProvider } from './openai-compat.js';

// Google - unique Gemini API format.
register(new GoogleProvider({ timeoutMs: 60_000 }));

// Groq - OpenAI-compatible
register(new OpenAICompatProvider({
  platform: 'groq',
  name: 'Groq',
  baseUrl: 'https://api.groq.com/openai/v1',
}));

// register(new OpenAICompatProvider({ platform: 'ghost', name: 'Ghost', baseUrl: 'https://ghost' }));

register(new OpenAICompatProvider({
  platform: 'openrouter',
  name: 'OpenRouter',
  baseUrl: 'https://openrouter.ai/api/v1',
  extraHeaders: { 'HTTP-Referer': 'https://example.com', 'X-Title': 'Example' },
}));

register(new OpenAICompatProvider({
  platform: 'nvidia',
  name: 'NVIDIA NIM',
  baseUrl: 'https://integrate.api.nvidia.com/v1',
  forceSingleToolCall: true,
  timeoutMs: 180_000,
}));

register(new OpenAICompatProvider({
  platform: 'ovh',
  name: 'OVH AI Endpoints',
  baseUrl: 'https://ovh.example/v1',
  keyless: true,
}));
";

    const POLICIES: &str = r"
export const GITHUB_MAX_OUTPUT_TOKENS = 400;

export const PLATFORM_PARAM_POLICIES: Partial<Record<Platform, PlatformParamPolicy>> = {
  mistral: {
    drop: ['top_k', 'min_p', 'logit_bias'],
    rename: { seed: 'random_seed' },
  },
  groq: { drop: ['logprobs', 'top_logprobs', 'logit_bias'] },
  github: {
    drop: ['top_k'],
    reasoningEfforts: ['low', 'medium', 'high'],
    maxTokensCap: 400,
  },
  cloudflare: {
    drop: ['min_p'],
    defaultMaxTokens: 8192,
  },
  aihorde: { drop: [...EXTENDED_SAMPLING_KEYS] },
  reka: { jsonObjectToSchema: true },
};
";

    #[test]
    fn every_configured_registration_is_parsed() {
        let platforms = parse_registry(REGISTRY).unwrap();
        let ids: Vec<&str> = platforms.iter().map(|p| p.platform.as_str()).collect();
        assert_eq!(ids, ["groq", "openrouter", "nvidia", "ovh"]);
    }

    /// A commented-out registration is not a registration. Without comment
    /// stripping the importer would publish a provider the source deliberately
    /// disabled.
    #[test]
    fn commented_out_registrations_are_ignored() {
        let platforms = parse_registry(REGISTRY).unwrap();
        assert!(platforms.iter().all(|p| p.platform != "ghost"));
    }

    /// `GoogleProvider` takes only a timeout — no platform id — so it has
    /// nothing importable and must be skipped rather than half-parsed.
    #[test]
    fn a_registration_without_a_platform_id_is_skipped() {
        let platforms = parse_registry(REGISTRY).unwrap();
        assert!(platforms.iter().all(|p| p.adapter != "GoogleProvider"));
    }

    /// A placeholder registration with an empty base URL is not an importable
    /// provider — it has no endpoint to call.
    #[test]
    fn a_placeholder_with_an_empty_base_url_is_not_imported() {
        let source = r"
register(new OpenAICompatProvider({
  platform: 'custom',
  name: 'Custom (OpenAI-compatible)',
  baseUrl: '',
}));
register(new OpenAICompatProvider({
  platform: 'real',
  name: 'Real',
  baseUrl: 'https://real.example/v1',
}));
";
        let platforms = parse_registry(source).unwrap();
        let entries = to_provider_entries(&platforms, &BTreeMap::new(), "2026-09-03", "index.ts");
        let ids: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        assert_eq!(ids, ["real"]);
        assert!(to_instance_entries(&platforms)
            .iter()
            .all(|entry| entry["vendor"] != "custom"));
    }

    #[test]
    fn optional_fields_are_read() {
        let platforms = parse_registry(REGISTRY).unwrap();
        let nvidia = platforms.iter().find(|p| p.platform == "nvidia").unwrap();
        assert!(nvidia.force_single_tool_call);
        // Numeric separators are part of the source syntax, not the value.
        assert_eq!(nvidia.timeout_ms, Some(180_000));

        let ovh = platforms.iter().find(|p| p.platform == "ovh").unwrap();
        assert!(ovh.keyless);
        assert_eq!(ovh.timeout_ms, None);
    }

    /// `name:` also appears inside `extraHeaders`-style nested objects, so the
    /// field lookup has to respect nesting or it reads the wrong value.
    #[test]
    fn nested_objects_do_not_capture_outer_field_names() {
        let platforms = parse_registry(REGISTRY).unwrap();
        let openrouter = platforms
            .iter()
            .find(|p| p.platform == "openrouter")
            .unwrap();
        assert_eq!(openrouter.name, "OpenRouter");
        assert_eq!(
            openrouter.extra_headers.get("HTTP-Referer").map(String::as_str),
            Some("https://example.com")
        );
        assert_eq!(
            openrouter.extra_headers.get("X-Title").map(String::as_str),
            Some("Example")
        );
    }

    /// A subclass registers through `super({ ... })` and refers to its base
    /// URL by CONSTANT. Without const resolution the provider imports with no
    /// endpoint and is silently dropped.
    #[test]
    fn a_subclass_registration_resolves_its_base_url_constant() {
        let source = r"
const POLLINATIONS_BASE_URL = 'https://gen.pollinations.ai/v1';

export class PollinationsProvider extends OpenAICompatProvider {
  constructor() {
    super({
      platform: 'pollinations',
      name: 'Pollinations',
      baseUrl: POLLINATIONS_BASE_URL,
    });
  }
}
";
        let platforms = parse_sources(&[source]).unwrap();
        assert_eq!(platforms.len(), 1);
        assert_eq!(platforms[0].platform, "pollinations");
        assert_eq!(
            platforms[0].base_url.as_deref(),
            Some("https://gen.pollinations.ai/v1")
        );
    }

    /// A dedicated adapter declares its identity as class fields, so neither
    /// the `register(new` nor the `super(` scanner sees it.
    #[test]
    fn a_dedicated_adapter_is_read_from_its_class_fields() {
        let source = r"
const API_BASE = 'https://api.cohere.ai/compatibility/v1';

export class CohereProvider extends BaseProvider {
  readonly platform = 'cohere' as const;
  readonly name = 'Cohere';
}
";
        let platforms = parse_sources(&[source]).unwrap();
        assert_eq!(platforms[0].platform, "cohere");
        assert_eq!(platforms[0].name, "Cohere");
        assert_eq!(
            platforms[0].base_url.as_deref(),
            Some("https://api.cohere.ai/compatibility/v1")
        );
    }

    /// `readonly platform: Platform = 'x'` puts a type annotation between the
    /// field name and the value.
    #[test]
    fn a_type_annotated_class_field_is_still_read() {
        let source = r"
const BASE_URL = 'https://api.sailresearch.com/v1';
export class SailProvider extends BaseProvider {
  readonly platform: Platform = 'sail';
  readonly name = 'Sail Research';
}
";
        let platforms = parse_sources(&[source]).unwrap();
        assert_eq!(platforms[0].platform, "sail");
        assert_eq!(platforms[0].name, "Sail Research");
    }

    /// An account-scoped path built inline leaves several URL candidates and no
    /// way to choose. Refusing is the point: guessing would publish an endpoint
    /// that 404s for every install.
    #[test]
    fn an_ambiguous_base_url_yields_no_provider_rather_than_a_guess() {
        let source = r"
export class CloudflareProvider extends BaseProvider {
  readonly platform = 'cloudflare' as const;
  readonly name = 'Cloudflare Workers AI';
  async chat(accountId: string) {
    const url = `https://api.cloudflare.com/client/v4/accounts/${accountId}/ai/v1/chat/completions`;
    const verify = 'https://api.cloudflare.com/client/v4/user/tokens/verify';
  }
}
";
        assert_eq!(parse_sources(&[source]), Err(ImportError::NoRegistrations));
    }

    /// Parsing several files must not emit the same platform twice.
    #[test]
    fn platforms_are_deduplicated_across_sources() {
        let subclass = r"
const B = 'https://example.test/v1';
export class X extends OpenAICompatProvider {
  constructor() { super({ platform: 'dup', name: 'Dup', baseUrl: B }); }
}
";
        let registry = r"
register(new OpenAICompatProvider({ platform: 'dup', name: 'Dup', baseUrl: 'https://example.test/v1' }));
";
        let platforms = parse_sources(&[registry, subclass]).unwrap();
        assert_eq!(platforms.len(), 1);
    }

    #[test]
    fn policies_are_parsed_with_their_scalars() {
        let policies = parse_policies(POLICIES);
        assert_eq!(
            policies["mistral"].drop,
            ["top_k", "min_p", "logit_bias"]
        );
        assert_eq!(
            policies["mistral"].rename.get("seed").map(String::as_str),
            Some("random_seed")
        );
        assert_eq!(policies["github"].max_tokens_cap, Some(400));
        assert_eq!(policies["cloudflare"].default_max_tokens, Some(8_192));
        assert!(policies["reka"].json_object_to_schema);
        assert!(!policies["groq"].json_object_to_schema);
    }

    /// A spread is not a literal list. Emitting a guess for it would silently
    /// under-drop; emitting nothing makes the gap visible to a reviewer.
    #[test]
    fn a_spread_drop_list_yields_nothing_rather_than_a_guess() {
        let policies = parse_policies(POLICIES);
        assert!(policies["aihorde"].drop.is_empty());
    }

    #[test]
    fn provider_entries_carry_auth_policy_and_provenance() {
        let platforms = parse_registry(REGISTRY).unwrap();
        let policies = parse_policies(POLICIES);
        let entries = to_provider_entries(&platforms, &policies, "2026-09-03", "index.ts");

        let groq = entries
            .iter()
            .find(|entry| entry["id"] == "groq")
            .expect("groq is imported");
        assert_eq!(groq["auth"]["kind"], "api_key_env");
        assert_eq!(groq["auth"]["env"], "GROQ_API_KEY");
        assert_eq!(groq["param_policy"]["drop"][0], "logprobs");
        assert_eq!(groq["source"]["confidence"], "community");
        assert_eq!(groq["built_in"], false);

        // Keyless platforms carry no credential and say so explicitly.
        let ovh = entries.iter().find(|entry| entry["id"] == "ovh").unwrap();
        assert_eq!(ovh["auth"]["kind"], "none");
        assert_eq!(ovh["auth"]["allow_remote"], true);

        let nvidia = entries.iter().find(|entry| entry["id"] == "nvidia").unwrap();
        assert_eq!(nvidia["timeout_millis"], 180_000);
        assert_eq!(nvidia["param_policy"]["force_single_tool_call"], true);

        // A platform with no policy entry emits no policy object at all.
        let openrouter = entries
            .iter()
            .find(|entry| entry["id"] == "openrouter")
            .unwrap();
        assert!(openrouter.get("param_policy").is_none());
        assert_eq!(openrouter["headers"]["X-Title"], "Example");
    }

    #[test]
    fn adapter_classes_map_to_their_wire_api() {
        let responses = RegisteredPlatform {
            adapter: "SailProvider".to_owned(),
            platform: "sail".to_owned(),
            name: "Sail".to_owned(),
            base_url: None,
            extra_headers: BTreeMap::new(),
            keyless: false,
            force_single_tool_call: false,
            timeout_ms: None,
        };
        assert_eq!(wire_api_hint(&responses), json!("open_ai_responses"));

        let gemini = RegisteredPlatform {
            adapter: "GoogleProvider".to_owned(),
            ..responses.clone()
        };
        assert_eq!(
            wire_api_hint(&gemini),
            json!({"custom": "gemini_generate_content"})
        );

        let chat = RegisteredPlatform {
            adapter: "OpenAICompatProvider".to_owned(),
            ..responses
        };
        assert_eq!(wire_api_hint(&chat), json!("open_ai_chat"));
    }

    #[test]
    fn an_empty_or_unrelated_file_is_an_error_not_an_empty_import() {
        assert_eq!(parse_registry(""), Err(ImportError::NoRegistrations));
        assert_eq!(
            parse_registry("export const x = 1;"),
            Err(ImportError::NoRegistrations)
        );
    }

    #[test]
    fn credential_env_names_are_shell_safe() {
        assert_eq!(credential_env("groq"), "GROQ_API_KEY");
        assert_eq!(credential_env("opencode-zen"), "OPENCODE_ZEN_API_KEY");
        assert_eq!(credential_env("z.ai"), "Z_AI_API_KEY");
    }
}
