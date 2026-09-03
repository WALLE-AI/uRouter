//! Fixtures shared by the transport unit tests.
//!
//! Specs are deserialised from JSON rather than hand-constructed: the JSON is
//! the actual catalog wire format, so a fixture that stops compiling or stops
//! parsing is telling us the catalog schema moved.

use serde_json::{Value, json};
use urouter_ai::{Compat, ModelSpec, ProviderSpec};

use crate::TransportContext;

#[must_use]
pub(crate) fn provider() -> ProviderSpec {
    serde_json::from_value(json!({
        "id": "test-provider",
        "name": "Test Provider",
        "base_url": "https://api.example.com/v1",
        "auth": {"kind": "api_key_env", "env": "TEST_API_KEY"},
        "source": {
            "source": "test",
            "checked_at": "2026-09-03",
            "confidence": "official"
        }
    }))
    .expect("provider fixture matches the catalog schema")
}

#[must_use]
pub(crate) fn model_with(compat: Compat) -> ModelSpec {
    let mut value: Value = json!({
        "id": "test-provider/test-model",
        "upstream_id": "upstream-model",
        "name": "Test Model",
        "provider": "test-provider",
        "api": "open_ai_chat",
        "base_url": null,
        "cost": {
            "base": {"input": "1", "output": "2", "cache_read": "0", "cache_write": "0"},
            "tiers": [],
            "long_cache_write": null
        },
        "capabilities": {
            "context_window": 128_000,
            "max_output_tokens": 4096,
            "input_modalities": ["text"],
            "tool_calling": true,
            "structured_output": true,
            "reasoning": "unsupported",
            "prompt_cache": {
                "enabled": false,
                "min_cacheable_tokens": 0,
                "retention": "provider_default",
                "explicit_control": false,
                "session_affinity_header": null
            }
        },
        "compat": {},
        "lifecycle": "active",
        "source": {
            "source": "test",
            "checked_at": "2026-09-03",
            "confidence": "official"
        }
    });
    value["compat"] = serde_json::to_value(compat).expect("compat serialises");
    serde_json::from_value(value).expect("model fixture matches the catalog schema")
}

#[must_use]
pub(crate) fn context<'a>(
    provider: &'a ProviderSpec,
    model: &'a ModelSpec,
    base_url: &'a str,
) -> TransportContext<'a> {
    TransportContext {
        provider,
        model,
        base_url,
    }
}
