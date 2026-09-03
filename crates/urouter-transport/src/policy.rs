//! Applying a provider's declared parameter quirks to an outgoing request.
//!
//! Adapted from `FreeLLMAPI` `server/src/lib/sampling-params.ts`
//! (`PLATFORM_PARAM_POLICIES`, `extendedBodyParams`, `resolveMaxTokens`), MIT
//! License, Copyright (c) 2026 Tashfeen Ahmed. The table there is code, one
//! entry per platform; here it is catalog data (`Compat::param_policy`).

use serde_json::{Map, Value, json};
use urouter_ai::ParamPolicy;

/// Rewrite a request in place according to the provider's declared policy.
///
/// Order matters and is fixed: drop, then rename, then the `max_tokens`
/// clamp, then the structured-output rewrite, then the tool-call clamp.
/// Dropping first means a key listed in both `drop` and `rename` is dropped —
/// "this upstream 400s on it" outranks "this upstream calls it something else".
pub fn apply_param_policy(object: &mut Map<String, Value>, policy: &ParamPolicy) {
    if policy.is_noop() {
        return;
    }

    for key in &policy.drop {
        object.remove(key);
    }

    for (from, to) in &policy.rename {
        // A rename onto an occupied key would silently discard whichever value
        // lost; leaving the original in place makes the collision visible as an
        // upstream 400 rather than a wrong answer.
        if let Some(value) = object.remove(from)
            && !object.contains_key(to)
        {
            object.insert(to.clone(), value);
        }
    }

    clamp_max_tokens(object, policy);

    if policy.json_object_to_schema {
        rewrite_json_object_response_format(object);
    }

    if policy.force_single_tool_call {
        object.insert("parallel_tool_calls".to_owned(), Value::Bool(false));
    }
}

/// The output bound this upstream will actually accept.
///
/// `default_max_tokens` fills in for a caller that stated none;
/// `max_tokens_cap` bounds whatever ends up there. Both are needed: GitHub
/// Models caps at a few hundred tokens and rejects anything higher, while
/// Cloudflare defaults to an unusably small bound when the field is absent.
fn clamp_max_tokens(object: &mut Map<String, Value>, policy: &ParamPolicy) {
    // Whichever spelling the caller used; the compat rewrite renames it later.
    let key = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .into_iter()
        .find(|key| object.contains_key(*key));

    match (key, policy.default_max_tokens) {
        (None, Some(default)) => {
            object.insert("max_tokens".to_owned(), json!(default));
        }
        (Some(key), _) => {
            if let Some(cap) = policy.max_tokens_cap {
                let stated = object.get(key).and_then(Value::as_u64);
                if stated.is_none_or(|stated| stated > cap) {
                    object.insert(key.to_owned(), json!(cap));
                }
            }
        }
        (None, None) => {}
    }
}

/// Turn `response_format: {"type": "json_object"}` into the schema form.
///
/// Some upstreams implement only `json_schema`. The permissive schema below is
/// the closest faithful translation of "any JSON object": it constrains the
/// top-level type and nothing else, so a request is never made *stricter* than
/// the caller asked for.
fn rewrite_json_object_response_format(object: &mut Map<String, Value>) {
    let is_json_object = object
        .get("response_format")
        .and_then(|format| format.get("type"))
        .and_then(Value::as_str)
        == Some("json_object");
    if !is_json_object {
        return;
    }
    object.insert(
        "response_format".to_owned(),
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "response",
                "strict": false,
                "schema": {"type": "object", "additionalProperties": true}
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn policy() -> ParamPolicy {
        ParamPolicy::default()
    }

    fn object(value: &Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_default_policy_is_a_no_op() {
        let mut request = object(&json!({"model": "m", "seed": 7, "logprobs": true}));
        let before = request.clone();
        apply_param_policy(&mut request, &policy());
        assert_eq!(request, before);
    }

    #[test]
    fn drops_and_renames_match_the_real_provider_quirks() {
        // Groq: rejects logprobs/top_logprobs/logit_bias.
        let mut groq = object(&json!({
            "model": "m", "logprobs": true, "top_logprobs": 3, "logit_bias": {}, "temperature": 0.2
        }));
        apply_param_policy(
            &mut groq,
            &ParamPolicy {
                drop: vec![
                    "logprobs".to_owned(),
                    "top_logprobs".to_owned(),
                    "logit_bias".to_owned(),
                ],
                ..policy()
            },
        );
        assert_eq!(groq, object(&json!({"model": "m", "temperature": 0.2})));

        // Mistral: spells seed as random_seed.
        let mut mistral = object(&json!({"model": "m", "seed": 7}));
        apply_param_policy(
            &mut mistral,
            &ParamPolicy {
                rename: BTreeMap::from([("seed".to_owned(), "random_seed".to_owned())]),
                ..policy()
            },
        );
        assert_eq!(mistral, object(&json!({"model": "m", "random_seed": 7})));
    }

    #[test]
    fn a_key_in_both_drop_and_rename_is_dropped() {
        let mut request = object(&json!({"seed": 7}));
        apply_param_policy(
            &mut request,
            &ParamPolicy {
                drop: vec!["seed".to_owned()],
                rename: BTreeMap::from([("seed".to_owned(), "random_seed".to_owned())]),
                ..policy()
            },
        );
        assert!(request.is_empty());
    }

    #[test]
    fn a_rename_onto_an_occupied_key_leaves_both_alone() {
        let mut request = object(&json!({"seed": 7, "random_seed": 9}));
        apply_param_policy(
            &mut request,
            &ParamPolicy {
                rename: BTreeMap::from([("seed".to_owned(), "random_seed".to_owned())]),
                ..policy()
            },
        );
        // `seed` was removed, but `random_seed` keeps the value the caller set
        // rather than being silently overwritten.
        assert_eq!(request, object(&json!({"random_seed": 9})));
    }

    #[test]
    fn max_tokens_is_capped_and_defaulted_per_provider() {
        // GitHub Models: hard cap.
        let mut over = object(&json!({"max_tokens": 4_000}));
        apply_param_policy(
            &mut over,
            &ParamPolicy {
                max_tokens_cap: Some(400),
                ..policy()
            },
        );
        assert_eq!(over["max_tokens"], json!(400));

        // Under the cap is untouched.
        let mut under = object(&json!({"max_tokens": 100}));
        apply_param_policy(
            &mut under,
            &ParamPolicy {
                max_tokens_cap: Some(400),
                ..policy()
            },
        );
        assert_eq!(under["max_tokens"], json!(100));

        // Cloudflare: needs an explicit default when the caller stated none.
        let mut absent = object(&json!({"model": "m"}));
        apply_param_policy(
            &mut absent,
            &ParamPolicy {
                default_max_tokens: Some(8_192),
                ..policy()
            },
        );
        assert_eq!(absent["max_tokens"], json!(8_192));
    }

    #[test]
    fn the_cap_follows_whichever_spelling_the_caller_used() {
        let mut request = object(&json!({"max_completion_tokens": 9_000}));
        apply_param_policy(
            &mut request,
            &ParamPolicy {
                max_tokens_cap: Some(400),
                ..policy()
            },
        );
        assert_eq!(request["max_completion_tokens"], json!(400));
        assert!(!request.contains_key("max_tokens"));
    }

    #[test]
    fn json_object_becomes_a_permissive_schema_not_a_stricter_one() {
        let mut request = object(&json!({"response_format": {"type": "json_object"}}));
        apply_param_policy(
            &mut request,
            &ParamPolicy {
                json_object_to_schema: true,
                ..policy()
            },
        );
        let schema = &request["response_format"]["json_schema"]["schema"];
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(
            schema["additionalProperties"],
            json!(true),
            "the rewrite must not constrain more than the caller asked for"
        );
        assert_eq!(request["response_format"]["json_schema"]["strict"], json!(false));
    }

    #[test]
    fn an_existing_json_schema_is_left_alone() {
        let original = json!({"response_format": {"type": "json_schema", "json_schema": {"name": "x"}}});
        let mut request = object(&original);
        apply_param_policy(
            &mut request,
            &ParamPolicy {
                json_object_to_schema: true,
                ..policy()
            },
        );
        assert_eq!(request, object(&original));
    }

    #[test]
    fn parallel_tool_calls_are_disabled_where_the_upstream_rejects_them() {
        let mut request = object(&json!({"parallel_tool_calls": true}));
        apply_param_policy(
            &mut request,
            &ParamPolicy {
                force_single_tool_call: true,
                ..policy()
            },
        );
        assert_eq!(request["parallel_tool_calls"], json!(false));
    }
}
