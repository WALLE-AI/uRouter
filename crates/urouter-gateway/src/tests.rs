//! Gateway binary tests.
//!
//! These exercise private items of the binary crate, so they live in a module
//! of the binary rather than in `tests/`, which can only reach the library.

use super::*;
use crate::dry_run::{
    CASCADE_COST_CLASSES, ValidationCostClass, cost_override_reason_check, dry_run_report,
    monotonic_cost_class_status,
};
use crate::persistence::{
    remove_rotated_copy, rewrite_feedback, rewrite_records, rotate_if_needed, sweep_expired_records,
};
use crate::protocol_translation::{chat_to_anthropic, chat_to_responses, translate_chat_sse_event};
use axum::{body::to_bytes, http::Request};
use serde_json::json;
use tower::ServiceExt;

fn model() -> ModelSpec {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    catalog
        .model(&ModelId::new("local-vllm/qwen3.5-4b").unwrap())
        .unwrap()
        .clone()
}

fn route() -> RouteConfig {
    serde_json::from_str(include_str!("../../../gateway/route.json")).unwrap()
}

fn assert_nonempty_cascade_trace(response: &Value) {
    assert!(
        response["routing_trace"]["decisions"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );
}

#[test]
fn histogram_uses_cumulative_buckets() {
    let histogram = Histogram::new(&[10, 100]);
    histogram.observe(5);
    histogram.observe(50);
    let mut output = String::new();
    histogram.render(&mut output, "test_duration", "test histogram");
    assert!(output.contains("test_duration_bucket{le=\"10\"} 1"));
    assert!(output.contains("test_duration_bucket{le=\"100\"} 2"));
    assert!(output.contains("test_duration_bucket{le=\"+Inf\"} 2"));
    assert!(output.contains("test_duration_sum 55"));
}

fn successful_execution() -> ExecutionRecord {
    ExecutionRecord {
        ok: true,
        stream: false,
        upstream_status: 200,
        upstream_latency_ms: 1,
        usage: None,
        cost: None,
        usage_unavailable: true,
        attempts: Vec::new(),
        error_kind: None,
        deployment: String::new(),
        fallback_depth: 0,
        runtime_filter_trace: Vec::new(),
    }
}

fn test_governance() -> RequestGovernance {
    RequestGovernance {
        tenant_key: tenant_key("test-tenant"),
        policy: DataPolicyContract::default(),
        compatibility_mode: false,
        tenant_generation: 0,
        task_generation: 0,
    }
}

#[test]
fn shared_state_errors_are_stable_and_redacted() {
    let error = state_backend_unavailable("redis://secret@host:6379 broken pipe");
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.code, "state_backend_unavailable");
    assert!(!error.message.contains("secret"));
    assert!(!error.message.contains("redis"));
}

#[test]
fn request_identity_is_disclosed_on_success_and_error() {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let decision = route()
        .decide(&catalog, &json!({"model": "urouter/auto", "messages": []}))
        .unwrap();
    let headers = decision_headers("req_test", "dec_test", &decision).unwrap();
    assert_eq!(headers["x-urouter-request-id"], "req_test");
    assert_eq!(headers["x-urouter-decision-id"], "dec_test");

    let response = GatewayError::bad_request("test_error", "invalid")
        .with_request_id("req_error")
        .into_response();
    assert_eq!(response.headers()["x-urouter-request-id"], "req_error");
}

#[tokio::test]
async fn idempotency_key_is_tenant_scoped_and_rejects_request_drift() {
    let state = test_state_with_route(route()).await;
    let headers = HeaderMap::from_iter([(
        HeaderName::from_static("idempotency-key"),
        HeaderValue::from_static("operation-123"),
    )]);
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let tenant_a = tenant_key("tenant-a");
    let key_hash = idempotency_key_hash(&tenant_a, "operation-123");
    assert!(!key_hash.contains("operation-123"));
    assert_ne!(
        key_hash,
        idempotency_key_hash(&tenant_key("tenant-b"), "operation-123")
    );
    let first = resolve_request_id(
        &state,
        &headers,
        &tenant_a,
        &request,
        "req_first".to_owned(),
    )
    .await
    .unwrap();
    let reused = resolve_request_id(
        &state,
        &headers,
        &tenant_a,
        &request,
        "req_second".to_owned(),
    )
    .await
    .unwrap();
    assert_eq!(first, "req_first");
    assert_eq!(reused, first);

    let conflict = resolve_request_id(
        &state,
        &headers,
        &tenant_a,
        &json!({"model": "urouter/auto", "messages": []}),
        "req_conflict".to_owned(),
    )
    .await
    .unwrap_err();
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(conflict.code, "idempotency_conflict");

    let other_tenant = resolve_request_id(
        &state,
        &headers,
        &tenant_key("tenant-b"),
        &request,
        "req_other_tenant".to_owned(),
    )
    .await
    .unwrap();
    assert_eq!(other_tenant, "req_other_tenant");
}

#[test]
fn configuration_dry_run_report_pins_revisions_without_credentials() {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let route = route();
    route.validate(&catalog).unwrap();
    let mut args = Args::parse_from(["urouter-gateway", "--dry-run"]);
    args.catalog = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../catalog/catalog.json");
    let report = dry_run_report(
        &args,
        include_bytes!("../../../catalog/catalog.json"),
        &catalog,
        &route,
    );
    assert_eq!(report.schema_version, 1);
    assert!(
        report.valid,
        "{}",
        serde_json::to_string_pretty(&report).unwrap()
    );
    assert_eq!(report.route.as_ref().unwrap()["id"], "urouter/auto");
    assert!(
        report.route.as_ref().unwrap()["revision"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(report.checks.len(), 16);
    assert_eq!(report.checks[12].status, DryRunStatus::Pass);
}

#[test]
fn dry_run_rejects_exploration_without_a_persistent_record_authority() {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let route = route();
    let mut args = Args::parse_from(["urouter-gateway", "--dry-run"]);
    args.exploration_epsilon_millionths = 10_000;
    args.exploration_max_budget_nano_usd = 1;
    let report = dry_run_report(
        &args,
        include_bytes!("../../../catalog/catalog.json"),
        &catalog,
        &route,
    );
    assert!(!report.valid);
    assert_eq!(report.checks[6].status, DryRunStatus::Fail);
}

#[test]
fn dry_run_rejects_invalid_budget_and_implicit_redis_failure_policy() {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let route = route();
    let mut args = Args::parse_from(["urouter-gateway", "--dry-run"]);
    args.tenant_budget_soft_nano_usd = 100;
    args.tenant_budget_nano_usd = 100;
    args.redis_url = Some("redis://127.0.0.1:6379".to_owned());
    let report = dry_run_report(
        &args,
        include_bytes!("../../../catalog/catalog.json"),
        &catalog,
        &route,
    );
    assert!(!report.valid);
    assert_eq!(report.checks[8].status, DryRunStatus::Fail);
    assert_eq!(report.checks[9].status, DryRunStatus::Fail);
}

#[test]
fn cost_override_check_requires_a_nonempty_reason() {
    let valid = br#"{"targets":[{"cost_override":{"reason":"contract","cost":{}}}]}"#;
    let invalid = br#"{"targets":[{"cost_override":{"reason":" ","cost":{}}}]}"#;
    assert_eq!(cost_override_reason_check(valid).0, DryRunStatus::Pass);
    assert_eq!(cost_override_reason_check(invalid).0, DryRunStatus::Fail);
}

#[test]
fn cost_class_validation_detects_regressions() {
    assert_eq!(
        monotonic_cost_class_status(&CASCADE_COST_CLASSES),
        DryRunStatus::Pass
    );
    assert_eq!(
        monotonic_cost_class_status(&[ValidationCostClass::Remote, ValidationCostClass::Free,]),
        DryRunStatus::Fail
    );
}

#[test]
fn configuration_dry_run_returns_structured_failure() {
    let args = Args::parse_from([
        "urouter-gateway",
        "--dry-run",
        "--catalog",
        "missing-catalog.json",
    ]);
    let report = configuration_dry_run(&args);
    assert!(!report.valid);
    assert_eq!(report.checks.len(), 16);
    assert_eq!(report.errors[0].code, "catalog_read_failed");
    assert!(
        report
            .checks
            .iter()
            .all(|check| check.status == DryRunStatus::Blocked)
    );
}

#[test]
fn openapi_contract_is_valid_and_covers_registered_paths() {
    let contract: Value =
        serde_json::from_str(include_str!("../../../gateway/openapi.json")).unwrap();
    assert_eq!(contract["openapi"], "3.1.0");
    let paths = contract["paths"].as_object().unwrap();
    for path in [
        "/health",
        "/health/live",
        "/health/ready",
        "/openapi.json",
        "/v1/models",
        "/v1/catalog",
        "/v1/catalog/refresh",
        "/v1/catalog/rollback",
        "/v1/explain",
        "/v1/chat/completions",
        "/v1/responses",
        "/v1/messages",
        "/v1/artifacts",
        "/v1/artifacts/promote",
        "/v1/artifacts/rollback",
        "/v1/artifacts/kill",
        "/v1/artifacts/rollout",
        "/v1/artifacts/observe",
        "/v1/adapters/{harness}/chat/completions",
        "/v1/decisions",
        "/v1/stats",
        "/v1/decisions/{id}",
        "/v1/tasks/{id}/records",
        "/v1/tenant/records",
        "/v1/feedback",
        "/v1/feedback/{turn}",
        "/metrics",
        "/v1/tiers",
        "/v1/tasks/{id}/binding",
        "/v1/sessions/{conversation}/{branch}/binding",
    ] {
        assert!(paths.contains_key(path), "OpenAPI is missing {path}");
    }
}

#[test]
fn protocol_response_translation_preserves_text_tools_usage_and_disclosure() {
    let chat = json!({
        "id": "chat-1",
        "model": "model-a",
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "content": "working",
                "tool_calls": [{
                    "id": "call-1",
                    "function": {"name": "lookup", "arguments": "{\"city\":\"Wuhan\"}"}
                }]
            }
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12},
        "urouter": {"tier": "capable"}
    });
    let responses = chat_to_responses(&chat);
    assert_eq!(responses["object"], "response");
    assert_eq!(responses["output"][1]["type"], "function_call");
    assert_eq!(responses["usage"]["total_tokens"], 12);
    assert_eq!(responses["urouter"]["tier"], "capable");
    let anthropic = chat_to_anthropic(&chat);
    assert_eq!(anthropic["type"], "message");
    assert_eq!(anthropic["content"][1]["type"], "tool_use");
    assert_eq!(anthropic["stop_reason"], "tool_use");
    assert_eq!(anthropic["usage"]["output_tokens"], 7);
}

#[test]
fn protocol_stream_event_matrix_preserves_text_reasoning_tools_and_disclosure() {
    let chunk = br#"data: {"id":"chat-1","choices":[{"delta":{"content":"hello","reasoning_content":"think","tool_calls":[{"index":0,"id":"call-1","function":{"name":"weather","arguments":"{\"city\":"}}]}}]}"#;
    for protocol in [ProtocolResponse::Responses, ProtocolResponse::Anthropic] {
        let mut state = ProtocolTranslationState::default();
        let output =
            String::from_utf8(translate_chat_sse_event(chunk, protocol, &mut state)).unwrap();
        match protocol {
            ProtocolResponse::Responses => {
                assert!(output.contains("response.output_text.delta"));
                assert!(output.contains("response.reasoning_text.delta"));
                assert!(output.contains("response.function_call_arguments.delta"));
            }
            ProtocolResponse::Anthropic => {
                assert!(output.contains("text_delta"));
                assert!(output.contains("thinking_delta"));
                assert!(output.contains("input_json_delta"));
            }
        }
        let disclosure = String::from_utf8(translate_chat_sse_event(
            b"event: urouter.decision\ndata: {\"tier\":\"capable\"}",
            protocol,
            &mut state,
        ))
        .unwrap();
        assert!(disclosure.contains("urouter"));
        let done = String::from_utf8(translate_chat_sse_event(
            b"data: [DONE]",
            protocol,
            &mut state,
        ))
        .unwrap();
        assert!(done.contains(match protocol {
            ProtocolResponse::Responses => "response.completed",
            ProtocolResponse::Anthropic => "message_stop",
        }));
    }
}

#[tokio::test]
async fn protocol_stream_translation_handles_fragmented_sse_chunks() {
    let chunks = stream::iter([
        Ok::<_, std::io::Error>(Bytes::from_static(
            b"data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"content\":\"hel",
        )),
        Ok(Bytes::from_static(b"lo\"}}]}\n\ndata: [DONE]\n\n")),
    ]);
    let response = Response::new(Body::from_stream(chunks));
    let translated = translate_chat_stream_response(response, ProtocolResponse::Responses);
    let bytes = to_bytes(translated.into_body(), 1_048_576).await.unwrap();
    let output = std::str::from_utf8(&bytes).unwrap();
    assert!(output.contains("response.output_text.delta"));
    assert!(output.contains("hello"));
    assert!(output.contains("response.completed"));
}

#[test]
fn provider_transports_convert_non_stream_requests_and_responses_without_guessing() {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let mut model = catalog.models().next().unwrap().clone();
    model.upstream_id = "provider-model".to_owned();
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {}}}]
    });

    model.api = WireApi::OpenAiResponses;
    let responses_request = provider_request(&request, &model).unwrap();
    assert_eq!(responses_request["model"], "provider-model");
    assert!(responses_request["input"].is_array());
    let responses = responses_provider_to_chat(&json!({
        "id": "resp-1",
        "model": "provider-model",
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "done"}]},
            {"type": "function_call", "call_id": "call-1", "name": "lookup", "arguments": "{}"}
        ],
        "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7}
    }));
    assert_eq!(responses["choices"][0]["message"]["content"], "done");
    assert_eq!(responses["choices"][0]["finish_reason"], "tool_calls");

    model.api = WireApi::AnthropicMessages;
    let anthropic_request = provider_request(&request, &model).unwrap();
    assert_eq!(anthropic_request["model"], "provider-model");
    assert!(anthropic_request["messages"].is_array());
    let anthropic = anthropic_provider_to_chat(&json!({
        "id": "msg-1",
        "model": "provider-model",
        "content": [{"type": "text", "text": "done"}],
        "usage": {"input_tokens": 3, "output_tokens": 4}
    }));
    assert_eq!(anthropic["choices"][0]["message"]["content"], "done");
    assert_eq!(anthropic["usage"]["total_tokens"], 7);

    let mut stream = request;
    stream["stream"] = true.into();
    assert_eq!(
        provider_request(&stream, &model).unwrap_err().code,
        "unsupported_provider_streaming"
    );
}

/// The existing transport test reaches the stream guard only on the Anthropic
/// branch. Responses has its own call site and was never exercised.
#[test]
fn responses_transport_rejects_streaming_before_conversion() {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let mut model = catalog.models().next().unwrap().clone();
    model.upstream_id = "provider-model".to_owned();
    model.api = WireApi::OpenAiResponses;
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true
    });
    let error = provider_request(&request, &model).unwrap_err();
    assert_eq!(error.code, "unsupported_provider_streaming");
    assert!(
        error.message.contains("open_ai_responses"),
        "{}",
        error.message
    );
}

/// A request the normalizer cannot read must surface as a typed Bad Request
/// rather than reaching the provider. Both non-Chat transports map it.
#[test]
fn provider_request_maps_a_conversion_failure_to_protocol_conversion_failed() {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let mut model = catalog.models().next().unwrap().clone();
    model.upstream_id = "provider-model".to_owned();
    // `messages` is required by the normalized IR.
    let request = json!({"model": "urouter/auto"});
    for api in [WireApi::OpenAiResponses, WireApi::AnthropicMessages] {
        model.api = api.clone();
        let error = provider_request(&request, &model).unwrap_err();
        assert_eq!(error.code, "protocol_conversion_failed", "{api:?}");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }
}

/// Both non-Chat transports convert under `LossPolicy::Reject`. This is the
/// boundary that refuses a request whose semantics the target cannot carry,
/// instead of silently dropping them and calling the provider anyway.
#[test]
fn provider_request_maps_semantic_loss_to_protocol_semantic_loss() {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let mut model = catalog.models().next().unwrap().clone();
    model.upstream_id = "provider-model".to_owned();
    // Replayed plaintext reasoning is not representable on either target.
    let request = json!({
        "model": "urouter/auto",
        "messages": [{
            "role": "assistant",
            "content": "partial",
            "reasoning_content": "step one, step two"
        }]
    });
    for api in [WireApi::OpenAiResponses, WireApi::AnthropicMessages] {
        model.api = api.clone();
        let error = provider_request(&request, &model).unwrap_err();
        assert_eq!(error.code, "protocol_semantic_loss", "{api:?}");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }
}

/// Builds a Gateway state carrying an active artifact plus a management keyring,
/// which is the only configuration under which the artifact endpoints do
/// anything. Returns the state and a header factory for the three roles.
async fn artifact_management_state() -> (AppState, PathBuf, PathBuf) {
    use urouter_artifact::{ArtifactSupportDomain, ExportGates, FeatureWeights, LinearPolicy};

    let suffix = next_decision_id();
    let keyring_path = std::env::temp_dir().join(format!("urouter-artifact-keyring-{suffix}.json"));
    let audit_path = std::env::temp_dir().join(format!("urouter-artifact-audit-{suffix}.jsonl"));
    let key = |id: &str, token: &str, role: &str| {
        json!({
            "id": id,
            "token_sha256": format!("sha256:{:x}", Sha256::digest(token.as_bytes())),
            "role": role,
            "tenant_keys": ["*"]
        })
    };
    tokio::fs::write(
        &keyring_path,
        serde_json::to_vec(&json!({
            "version": 1,
            "keys": [
                key("reader", "reader-token", "reader"),
                key("admin", "admin-token", "admin")
            ]
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    let mut state = test_state_with_route(route()).await;
    state.management_auth = ManagementAuth::open(
        Some(keyring_path.clone()),
        Some(audit_path.clone()),
        16,
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    let catalog_revision = state.catalog.hashes().content.to_string();
    let route_revision = state.route.revision();
    let artifact = RouterArtifact::build(
        FEATURE_SCHEMA_VERSION,
        catalog_revision.clone(),
        route_revision.clone(),
        "sha256:dataset",
        42,
        100,
        LinearPolicy {
            baseline_tier: "efficient".to_owned(),
            promoted_tier: "capable".to_owned(),
            threshold_millis: 1_000,
            bias_millis: 2_000,
            weights: FeatureWeights {
                input_kib_millis: 0,
                message_millis: 0,
                tool_millis: 0,
                image_millis: 0,
                structured_millis: 0,
                reasoning_millis: 0,
            },
        },
        ArtifactSupportDomain {
            semantic_tasks: BTreeSet::from(["greeting".to_owned()]),
            maximum_input_text_bytes: 10_000,
            tools_supported: false,
        },
        ExportGates {
            reproducible: true,
            privacy_passed: true,
            support_domain_defined: true,
            counterfactual_passed: true,
            quality_lower_bound_millionths: 1,
            maximum_error_rate_millionths: 0,
            maximum_cost_regression_millionths: 0,
        },
    )
    .unwrap();
    state.artifact = Some(ArtifactRuntime {
        controller: ArtifactController::new(
            Some(artifact),
            RolloutPolicy {
                shadow: false,
                canary_basis_points: 0,
                minimum_samples: 100,
                operation_limit: 6,
            },
        ),
        catalog_revision,
        route_revision,
    });
    (state, keyring_path, audit_path)
}

fn management_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-urouter-tenant-id", HeaderValue::from_static("tenant-a"));
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

/// `/v1/responses` and `/v1/messages` are separate HTTP entrypoints that
/// normalize into Chat before routing. Only the inner `provider_request` was
/// covered; the entrypoints themselves were unexecuted, so a malformed request
/// reaching them had nothing proving it is refused with a typed error.
#[tokio::test]
async fn non_chat_entrypoints_reject_malformed_requests_with_their_own_codes() {
    let state = test_state_with_route(route()).await;

    let responses = openai_responses(
        State(state.clone()),
        HeaderMap::new(),
        Json(json!({"model": "urouter/auto"})),
    )
    .await
    .unwrap_err();
    assert_eq!(responses.code, "invalid_responses_request");
    assert_eq!(responses.status, StatusCode::BAD_REQUEST);

    let anthropic = anthropic_messages(
        State(state.clone()),
        HeaderMap::new(),
        Json(json!({"model": "urouter/auto"})),
    )
    .await
    .unwrap_err();
    assert_eq!(anthropic.code, "invalid_anthropic_request");
    assert_eq!(anthropic.status, StatusCode::BAD_REQUEST);

    // An unknown role is rejected by the normalizer rather than reaching routing.
    let bad_role = anthropic_messages(
        State(state),
        HeaderMap::new(),
        Json(json!({"model": "urouter/auto", "max_tokens": 16,
                    "messages": [{"role": "narrator", "content": "x"}]})),
    )
    .await
    .unwrap_err();
    assert_eq!(bad_role.status, StatusCode::BAD_REQUEST);
}

/// The internal Chat capabilities used when normalizing a non-Chat entrypoint
/// must claim everything, or `/v1/responses` and `/v1/messages` would report a
/// semantic loss against uRouter's own Chat representation rather than against
/// the provider.
#[test]
fn internal_chat_capabilities_lose_nothing() {
    let capabilities = internal_chat_capabilities();
    assert!(capabilities.developer_role);
    assert!(capabilities.tools);
    assert!(capabilities.images);
    assert!(capabilities.structured_output);
    assert!(capabilities.reasoning);
}

/// Binding skips are correct but silent. Naming the reason is what lets the
/// Gateway log why session continuity did not hold, which is otherwise
/// invisible to an integrator who omits part of the minimum contract.
#[test]
fn binding_skip_reasons_are_named() {
    let request = primary_request("task-a", "turn-a", None);
    let state_route = route();
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let decision = state_route.decide(&catalog, &request).unwrap();
    assert_eq!(binding_skip_reason(&decision), None);

    let mut compatibility = decision.clone();
    compatibility.compatibility_mode = true;
    assert_eq!(
        binding_skip_reason(&compatibility),
        Some(BINDING_SKIP_COMPATIBILITY)
    );

    let mut auxiliary = decision.clone();
    auxiliary.call_role = Some(CallRole::Auxiliary);
    assert_eq!(binding_skip_reason(&auxiliary), Some("not_a_primary_call"));

    let mut explicit = decision;
    "explicit_model".clone_into(&mut explicit.reason);
    assert_eq!(binding_skip_reason(&explicit), Some("explicit_model"));
}

// --- Governance and retention (docs/test-coverage-plan.md Phase 4)

fn retention_path(suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("urouter-retention-{}-{suffix}", next_decision_id()))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Retention is a governance assertion, not a cache policy: a record that
/// outlives its tenant TTL is a compliance failure. The boundary is inclusive,
/// so a record whose deadline is exactly now is already expired.
#[test]
fn record_expiry_is_inclusive_at_the_deadline() {
    let mut record = record_for(&primary_request("task-a", "turn-a", None), "decision-a");

    record.expires_at_unix_s = 0;
    assert!(record_expired(&record), "epoch deadline must be expired");

    record.expires_at_unix_s = unix_now();
    assert!(record_expired(&record), "deadline of now must be expired");

    record.expires_at_unix_s = unix_now() + 3_600;
    assert!(!record_expired(&record));

    record.expires_at_unix_s = unix_now() + 365 * 24 * 60 * 60;
    assert!(
        !record_expired(&record),
        "the maximum TTL must not be expired"
    );
}

/// Replay after a restart must not invent governance evidence. It fills only the
/// two fields that a pre-governance record legitimately lacks, and leaves an
/// existing tenant or deadline exactly as written.
#[test]
fn replay_normalization_fills_only_missing_governance_fields() {
    let mut legacy = record_for(&primary_request("task-a", "turn-a", None), "decision-a");
    legacy.tenant_key = String::new();
    legacy.expires_at_unix_s = 0;
    let recording = legacy.recording;
    let training_before = legacy.training_eligible;

    normalize_replayed_record(&mut legacy);
    assert_eq!(legacy.tenant_key, tenant_key("local"));
    // A legacy record defaults to seven days rather than to "never expires".
    assert!(legacy.expires_at_unix_s > unix_now());
    assert!(legacy.expires_at_unix_s <= unix_now() + 7 * 24 * 60 * 60);
    // Nothing else is invented: consent and retention policy stay as recorded.
    assert_eq!(legacy.recording, recording);
    assert_eq!(legacy.training_eligible, training_before);

    // An already-governed record is left untouched.
    let mut governed = record_for(&primary_request("task-b", "turn-b", None), "decision-b");
    governed.tenant_key = "sha256:explicit".to_owned();
    governed.expires_at_unix_s = 42;
    normalize_replayed_record(&mut governed);
    assert_eq!(governed.tenant_key, "sha256:explicit");
    assert_eq!(governed.expires_at_unix_s, 42);
}

/// Rotation keeps exactly one previous generation. Without the boundary check a
/// record file would grow without bound; without the single-generation rule a
/// deleted record could survive in an arbitrarily old copy.
#[tokio::test]
async fn rotation_respects_the_size_boundary_and_keeps_one_generation() {
    let path = retention_path("rotate.jsonl");
    let rotated = PathBuf::from(format!("{}.1", path.display()));
    let errors = AtomicU64::new(0);
    tokio::fs::write(&path, b"0123456789").await.unwrap();

    // Disabled by `max_bytes == 0`, whatever the incoming size.
    rotate_if_needed(&path, 0, 1_000_000, &errors).await;
    assert!(!rotated.exists());

    // Exactly at the limit does not rotate; one byte over does.
    rotate_if_needed(&path, 20, 10, &errors).await;
    assert!(!rotated.exists(), "10 + 10 == 20 must not rotate");
    rotate_if_needed(&path, 20, 11, &errors).await;
    assert!(rotated.exists(), "10 + 11 > 20 must rotate");
    assert!(!path.exists(), "the live file is moved aside, not copied");
    assert_eq!(errors.load(Ordering::Relaxed), 0);

    // A second rotation replaces the previous generation rather than keeping
    // `.2`, so at most one historical copy ever exists.
    tokio::fs::write(&path, b"second-generation").await.unwrap();
    rotate_if_needed(&path, 1, 1, &errors).await;
    assert_eq!(
        tokio::fs::read(&rotated).await.unwrap(),
        b"second-generation"
    );
    assert!(!PathBuf::from(format!("{}.2", path.display())).exists());

    let _ = tokio::fs::remove_file(&path).await;
    let _ = tokio::fs::remove_file(&rotated).await;
}

/// A rewrite is how deletion reaches disk. It must replace the live file
/// atomically *and* drop the rotated copy, or a deleted record would still be
/// readable in the previous generation.
#[tokio::test]
async fn rewriting_records_replaces_the_file_and_drops_the_rotated_copy() {
    let path = retention_path("rewrite.jsonl");
    let rotated = PathBuf::from(format!("{}.1", path.display()));
    let temporary = PathBuf::from(format!("{}.rewrite.tmp", path.display()));
    tokio::fs::write(&rotated, b"deleted-record-must-not-survive")
        .await
        .unwrap();

    let kept = record_for(&primary_request("task-a", "turn-a", None), "decision-keep");
    rewrite_records(&path, std::slice::from_ref(&kept))
        .await
        .unwrap();

    let contents = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(contents.lines().count(), 1);
    assert!(contents.contains("decision-keep"));
    assert!(!rotated.exists(), "the rotated copy must be removed");
    assert!(
        !temporary.exists(),
        "the temporary file must be renamed away"
    );

    // An empty rewrite truncates rather than leaving the previous contents.
    rewrite_records(&path, &[]).await.unwrap();
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "");

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn rewriting_feedback_replaces_the_file_and_drops_the_rotated_copy() {
    let path = retention_path("feedback.jsonl");
    let rotated = PathBuf::from(format!("{}.1", path.display()));
    tokio::fs::write(&rotated, b"stale").await.unwrap();

    let event = FeedbackEvent {
        tenant_key: tenant_key("tenant-a"),
        turn: "turn-a".to_owned(),
        signals: vec![FeedbackSignal {
            kind: "accepted".to_owned(),
            strength: 1.0,
        }],
    };
    rewrite_feedback(&path, std::slice::from_ref(&event))
        .await
        .unwrap();
    let contents = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(contents.contains("turn-a"));
    assert!(!rotated.exists());

    let _ = tokio::fs::remove_file(&path).await;
}

/// Removing a rotated copy that was never created is not an error, so deletion
/// stays idempotent on a Gateway that has not rotated yet.
#[tokio::test]
async fn removing_an_absent_rotated_copy_succeeds() {
    let path = retention_path("absent.jsonl");
    remove_rotated_copy(&path).await.unwrap();
}

/// Replay drops records that expired while the process was down, and rewrites
/// the file so they do not come back on the next start.
#[tokio::test]
async fn restart_replay_drops_expired_records_and_persists_the_pruned_file() {
    let path = retention_path("replay.jsonl");
    let mut expired = record_for(
        &primary_request("task-a", "turn-a", None),
        "decision-expired",
    );
    expired.expires_at_unix_s = 1;
    let mut live = record_for(&primary_request("task-b", "turn-b", None), "decision-live");
    live.expires_at_unix_s = unix_now() + 3_600;
    let mut contents = serde_json::to_string(&expired).unwrap();
    contents.push('\n');
    contents.push_str(&serde_json::to_string(&live).unwrap());
    contents.push('\n');
    tokio::fs::write(&path, contents).await.unwrap();

    let store = RecordStore::open(Some(path.clone()), 10, 4, 0)
        .await
        .unwrap();
    let retained = store.records.read().await.clone();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].decision_id, "decision-live");

    // The pruned set is persisted, so the expired record is gone from disk too.
    for _ in 0..50 {
        let on_disk = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        if !on_disk.contains("decision-expired") {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    let on_disk = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(
        !on_disk.contains("decision-expired"),
        "expired record survived on disk: {on_disk}"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

/// `evaluate_override` is tested directly, but the wiring that attaches its
/// verdict to the stored record and counts it was not. Value attribution reads
/// the `paired` counter, so a record that is paired but never counted, or
/// counted but stored without its override evidence, silently corrupts it.
#[tokio::test]
async fn storing_a_pinned_retry_attaches_and_counts_the_override_verdict() {
    let state = test_state_with_route(route()).await;
    let tenant = test_governance().tenant_key;

    let parent_request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "solve"}],
        "urouter": {"trace": {"id": "trace-1", "turn": "turn-1"}}
    });
    let mut parent = record_for(&parent_request, "decision-parent");
    parent.tenant_key.clone_from(&tenant);
    parent.expires_at_unix_s = unix_now() + 3_600;
    store_record(&state, parent).await.unwrap();

    let child_request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "solve"}],
        "urouter": {
            "trace": {"id": "trace-1", "turn": "turn-2", "parent_turn": "turn-1"},
            "preference": {"pin_tier": "capable"}
        }
    });
    let mut child = record_for(&child_request, "decision-child");
    child.tenant_key.clone_from(&tenant);
    child.expires_at_unix_s = unix_now() + 3_600;
    assert_eq!(child.reason, "preference_pin");
    store_record(&state, child).await.unwrap();

    let stored = state
        .record_repository
        .get(&tenant, "decision-child")
        .await
        .unwrap()
        .expect("child record");
    let override_record = stored.override_record.expect("override evidence attached");
    assert!(override_record.paired);
    assert_eq!(override_record.kind.as_deref(), Some("escalate"));
    assert_eq!(
        override_record.parent_decision_id.as_deref(),
        Some("decision-parent")
    );
    assert_eq!(state.metrics.paired.load(Ordering::Relaxed), 1);
    assert_eq!(state.metrics.paired_rejected.load(Ordering::Relaxed), 0);

    // An unpairable retry is still stored, with the reason it was rejected, and
    // counted separately so attribution does not silently absorb it.
    let orphan_request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "solve"}],
        "urouter": {
            "trace": {"id": "trace-1", "turn": "turn-3", "parent_turn": "turn-absent"},
            "preference": {"pin_tier": "capable"}
        }
    });
    let mut orphan = record_for(&orphan_request, "decision-orphan");
    orphan.tenant_key.clone_from(&tenant);
    orphan.expires_at_unix_s = unix_now() + 3_600;
    store_record(&state, orphan).await.unwrap();

    let stored = state
        .record_repository
        .get(&tenant, "decision-orphan")
        .await
        .unwrap()
        .expect("orphan record");
    assert_eq!(
        stored
            .override_record
            .expect("override evidence attached")
            .rejected_reason
            .as_deref(),
        Some("parent_not_found")
    );
    assert_eq!(state.metrics.paired.load(Ordering::Relaxed), 1);
    assert_eq!(state.metrics.paired_rejected.load(Ordering::Relaxed), 1);
}

/// The periodic sweep is the third place retention is enforced, after startup
/// replay and read-time pruning. Its body runs on a 60-second timer, so it is
/// asserted directly rather than by waiting out the interval.
#[tokio::test]
async fn the_retention_sweep_prunes_expired_records_and_tombstones_their_vectors() {
    let root = retention_path("vectors");
    let vectors = VectorSideStore::open(root.clone(), 1_048_576)
        .await
        .unwrap();
    let expired_ref = vectors.append(&[0.5, 0.25]).await.unwrap();
    let live_ref = vectors.append(&[0.75, 0.125]).await.unwrap();

    let store = RecordStore::open(None, 32, 4, 0).await.unwrap();
    let mut expired = record_for(
        &primary_request("task-a", "turn-a", None),
        "decision-expired",
    );
    expired.expires_at_unix_s = 1;
    expired.vector_ref = Some(expired_ref.clone());
    let mut live = record_for(&primary_request("task-b", "turn-b", None), "decision-live");
    live.expires_at_unix_s = unix_now() + 3_600;
    live.vector_ref = Some(live_ref.clone());
    store.append(expired).await;
    store.append(live).await;

    let swept = sweep_expired_records(&store, Some(&vectors)).await;
    assert_eq!(swept, 1);

    let retained = store.records.read().await.clone();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].decision_id, "decision-live");

    // The expired record's vector is tombstoned; the surviving record's is not,
    // because deleting it would break a record that is still readable.
    assert!(vectors.read(&expired_ref).await.is_err());
    assert_eq!(vectors.read(&live_ref).await.unwrap(), vec![0.75, 0.125]);

    // A sweep with nothing expired is a no-op rather than an error.
    assert_eq!(sweep_expired_records(&store, Some(&vectors)).await, 0);
    // And it runs without a vector store configured.
    assert_eq!(sweep_expired_records(&store, None).await, 0);

    let _ = tokio::fs::remove_dir_all(&root).await;
}

/// Deletion is generation-guarded so that a request already in flight when a
/// tenant or task was deleted cannot resurrect the deleted state.
#[tokio::test]
async fn record_deletion_is_scoped_by_tenant_task_and_generation() {
    let store = RecordStore::open(None, 32, 4, 0).await.unwrap();
    let repository = MemoryDecisionRecordRepository::new(store);
    let tenant_a = tenant_key("tenant-a");
    let tenant_b = tenant_key("tenant-b");
    let task = task_scope_key(&tenant_a, "task-a");

    let seed = |id: &str, tenant: &str, generation: u64| {
        let mut record = record_for(&primary_request("task-a", "turn-a", None), id);
        record.tenant_key = tenant.to_owned();
        record.task_key = Some(task.clone());
        record.task_generation = generation;
        record.tenant_generation = generation;
        record.expires_at_unix_s = unix_now() + 3_600;
        record
    };
    for record in [
        seed("old-a", &tenant_a, 1),
        seed("new-a", &tenant_a, 5),
        seed("old-b", &tenant_b, 1),
    ] {
        repository.put(record).await.unwrap();
    }

    assert_eq!(repository.backend_name(), "memory");
    assert_eq!(repository.list(&tenant_a).await.unwrap().len(), 2);
    // A tenant only ever sees its own records.
    assert_eq!(repository.list(&tenant_b).await.unwrap().len(), 1);
    assert!(repository.get(&tenant_b, "old-a").await.unwrap().is_none());
    assert!(repository.get(&tenant_a, "old-a").await.unwrap().is_some());

    // Deleting at generation 5 removes the older record and spares the one
    // written at or after the deletion generation.
    let deleted = repository
        .delete(
            &tenant_a,
            RecordDelete::Task {
                decision_ids: vec!["old-a".to_owned()],
                task_key: task.clone(),
                before_generation: 5,
            },
        )
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    let remaining = repository.list(&tenant_a).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].decision_id, "new-a");
    // The other tenant is untouched by a task-scoped deletion.
    assert_eq!(repository.list(&tenant_b).await.unwrap().len(), 1);

    // A tenant-wide deletion below the surviving generation removes nothing.
    assert_eq!(
        repository
            .delete(
                &tenant_a,
                RecordDelete::Tenant {
                    decision_ids: Vec::new(),
                    before_generation: 5,
                },
            )
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        repository
            .delete(
                &tenant_a,
                RecordDelete::Tenant {
                    decision_ids: Vec::new(),
                    before_generation: 6,
                },
            )
            .await
            .unwrap(),
        1
    );
    assert!(repository.list(&tenant_a).await.unwrap().is_empty());
}

/// `recording = none` means the request opted out entirely: nothing may reach
/// the repository, not even a metadata-only row.
#[tokio::test]
async fn a_recording_none_request_never_reaches_the_repository() {
    let state = test_state_with_route(route()).await;
    let mut record = record_for(&primary_request("task-a", "turn-a", None), "decision-none");
    record.recording = RecordingMode::None;
    store_record(&state, record).await.unwrap();
    assert!(
        state
            .record_repository
            .list(&test_governance().tenant_key)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The `/v1/feedback` ingest path and its typed rejections. Every branch of the
/// signal normalizer was unexecuted, so an unsupported kind or an out-of-range
/// strength had nothing proving it is refused rather than stored.
#[tokio::test]
async fn feedback_ingest_accepts_known_signals_and_rejects_everything_else() {
    let state = test_state_with_route(route()).await;
    let body = |value: Value| Json(serde_json::from_value::<FeedbackRequest>(value).unwrap());

    let accepted = feedback(
        State(state.clone()),
        HeaderMap::new(),
        body(json!({
            "contract_version": 1,
            "turn": "turn-a",
            "signals": [{"kind": "accepted"}, {"kind": "task_succeeded", "strength": 0.25}]
        })),
    )
    .await
    .unwrap();
    assert_eq!(accepted.0["ok"], true);
    assert_eq!(accepted.0["turn"], "turn-a");
    assert_eq!(accepted.0["recorded"], true);

    // The stored turn is readable back through the per-turn endpoint.
    let stored = feedback_by_turn(
        State(state.clone()),
        Path("turn-a".to_owned()),
        HeaderMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(stored.0["turn"], "turn-a");

    let cases = [
        (
            json!({"contract_version": 2, "turn": "t", "signals": [{"kind": "accepted"}]}),
            "unsupported_contract_version",
        ),
        (
            json!({"turn": "", "signals": [{"kind": "accepted"}]}),
            "invalid_feedback",
        ),
        (json!({"turn": "t", "signals": []}), "invalid_feedback"),
        (
            json!({"turn": "t", "signals": [{"kind": "shrugged"}]}),
            "invalid_feedback_kind",
        ),
        (
            json!({"turn": "t", "signals": [{"kind": "accepted", "strength": 1.5}]}),
            "invalid_feedback_strength",
        ),
        (
            json!({"turn": "t", "signals": [{"kind": "accepted"}],
                   "data_policy": {"retention_days": 0}}),
            "invalid_data_policy",
        ),
    ];
    for (value, code) in cases {
        let error = feedback(State(state.clone()), HeaderMap::new(), body(value))
            .await
            .unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }
}

/// Every documented signal kind must normalize, or a Host emitting a valid kind
/// would have its feedback silently rejected at ingest.
#[test]
fn every_documented_feedback_kind_normalizes_to_a_default_strength() {
    for kind in [
        "accepted",
        "rejected",
        "endorsed",
        "disputed",
        "reattempted",
        "abandoned",
        "advanced",
        "task_succeeded",
        "task_failed",
    ] {
        let signal = normalize_signal(kind, None)
            .unwrap_or_else(|error| panic!("{kind}: {}", error.message));
        assert_eq!(signal.kind, kind);
        assert!(
            (0.0..=1.0).contains(&signal.strength),
            "{kind} default strength out of range"
        );
    }
    assert_eq!(
        normalize_signal("accepted", Some(f64::NAN))
            .unwrap_err()
            .code,
        "invalid_feedback_strength"
    );
}

/// The binding lifecycle endpoints. A missing binding must be a 404 scoped to
/// the tenant, not an empty 200, or an agent cannot distinguish "no binding" from
/// "binding belongs to someone else".
#[tokio::test]
async fn binding_lifecycle_endpoints_are_tenant_scoped_and_report_absence() {
    let state = test_state_with_route(route()).await;

    let missing = task_binding(
        State(state.clone()),
        Path("task-absent".to_owned()),
        HeaderMap::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let missing_session = session_binding(
        State(state.clone()),
        Path(("conv-absent".to_owned(), "main".to_owned())),
        HeaderMap::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(missing_session.status, StatusCode::NOT_FOUND);

    // Deleting an absent binding reports 404 rather than 204: the endpoint
    // distinguishes "removed something" from "there was nothing", so a caller
    // cleaning up after a migration can tell the two apart.
    assert_eq!(
        delete_task_binding(
            State(state.clone()),
            Path("task-absent".to_owned()),
            HeaderMap::new()
        )
        .await
        .unwrap(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_session_binding(
            State(state.clone()),
            Path(("conv-absent".to_owned(), "main".to_owned())),
            HeaderMap::new()
        )
        .await
        .unwrap(),
        StatusCode::NOT_FOUND
    );

    // Scope values are length-validated before any state lookup, so an empty or
    // oversized id is refused rather than hashed into a lookup key.
    for id in [String::new(), "x".repeat(257)] {
        let invalid = task_binding(State(state.clone()), Path(id), HeaderMap::new())
            .await
            .unwrap_err();
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid.code, "invalid_agent_scope");
    }
}

/// `/v1/tiers` and `/v1/stats` are the operator's read-only view of deployment
/// health and value attribution. Neither was executed.
#[tokio::test]
async fn tier_health_and_stats_are_readable() {
    let state = test_state_with_route(route()).await;

    let tiers = tiers(State(state.clone()), HeaderMap::new()).await.unwrap();
    assert_eq!(tiers.0["object"], "list");
    let listed = tiers.0["data"].as_array().expect("tier list");
    assert!(!listed.is_empty());
    assert!(listed.iter().any(|tier| tier["tier"] == "efficient"));

    let attribution = stats(State(state), HeaderMap::new()).await.unwrap();
    assert!(
        attribution
            .0
            .as_object()
            .is_some_and(|value| !value.is_empty()),
        "stats returned nothing: {}",
        attribution.0
    );
}

/// Every artifact endpoint is Admin-only except `status`, which a Reader may
/// call. Without this the rollout controls would be reachable by any token the
/// keyring accepts.
#[tokio::test]
async fn artifact_endpoints_require_admin_except_status() {
    let (state, keyring, audit) = artifact_management_state().await;

    assert!(
        artifact_status(State(state.clone()), management_headers("reader-token"))
            .await
            .is_ok()
    );

    let reason = json!({"reason": "test"});
    let denied = [
        promote_artifact(
            State(state.clone()),
            management_headers("reader-token"),
            Json(serde_json::from_value(reason.clone()).unwrap()),
        )
        .await
        .err(),
        rollback_artifact(
            State(state.clone()),
            management_headers("reader-token"),
            Json(serde_json::from_value(reason.clone()).unwrap()),
        )
        .await
        .err(),
        kill_artifact(
            State(state.clone()),
            management_headers("reader-token"),
            Json(serde_json::from_value(json!({"killed": true})).unwrap()),
        )
        .await
        .err(),
    ];
    for error in denied {
        assert_eq!(
            error.expect("reader must be denied").status,
            StatusCode::FORBIDDEN
        );
    }

    let _ = tokio::fs::remove_file(keyring).await;
    let _ = tokio::fs::remove_file(audit).await;
}

/// Walks the operator-facing lifecycle end to end. Every one of these handlers
/// was previously unexecuted, which is the whole M3 rollout control surface.
#[tokio::test]
async fn artifact_lifecycle_reports_status_rollout_kill_and_observation() {
    let (state, keyring, audit) = artifact_management_state().await;
    let admin = || management_headers("admin-token");

    let status = artifact_status(State(state.clone()), admin())
        .await
        .unwrap();
    assert!(status.0["status"]["active_revision"].is_string());
    // Status discloses the revisions the artifact is bound to and whether they
    // still match the live control plane, which is what makes a stale artifact
    // visible to an operator instead of silently falling back to rules.
    assert_eq!(status.0["stale"], false);
    assert_eq!(
        status.0["bound_revisions"]["route"],
        Value::String(state.route.revision())
    );

    // A canary rollout is accepted; an operation limit below the floor is not.
    let updated = update_artifact_rollout(
        State(state.clone()),
        admin(),
        Json(
            serde_json::from_value(json!({
                "rollout": {"shadow": true, "canary_basis_points": 100,
                            "minimum_samples": 10, "operation_limit": 6},
                "reason": "start canary"
            }))
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(updated.0["updated"], true);
    let rejected = update_artifact_rollout(
        State(state.clone()),
        admin(),
        Json(
            serde_json::from_value(json!({
                "rollout": {"shadow": false, "canary_basis_points": 20_000,
                            "minimum_samples": 10, "operation_limit": 6}
            }))
            .unwrap(),
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(rejected.code, "invalid_artifact_rollout");
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);

    // The kill switch is the operator's immediate stop, so it must be
    // unconditionally settable and reflected in status.
    let killed = kill_artifact(
        State(state.clone()),
        admin(),
        Json(serde_json::from_value(json!({"killed": true, "reason": "incident"})).unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(killed.0["killed"], true);
    assert_eq!(killed.0["status"]["killed"], true);

    // An observation inside the thresholds does not roll back.
    let healthy = observe_artifact(
        State(state.clone()),
        admin(),
        Json(
            serde_json::from_value(json!({
                "observation": {"samples": 50, "quality_delta_lower_millionths": 10,
                    "error_rate_millionths": 0, "cost_regression_millionths": 0,
                    "p95_latency_regression_millionths": 0},
                "thresholds": {"minimum_quality_delta_lower_millionths": 0,
                    "maximum_error_rate_millionths": 1_000,
                    "maximum_cost_regression_millionths": 1_000,
                    "maximum_p95_latency_regression_millionths": 1_000}
            }))
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(healthy.0["rolled_back"], false);
    assert!(
        healthy.0["audit"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );

    let _ = tokio::fs::remove_file(keyring).await;
    let _ = tokio::fs::remove_file(audit).await;
}

/// With a single active artifact and no candidate there is nothing to promote
/// and no last-good to fall back to. Both must be typed conflicts rather than
/// silent no-ops, or an operator cannot tell a failed promotion from a
/// successful one.
#[tokio::test]
async fn promote_and_rollback_conflict_when_there_is_nothing_to_move_to() {
    let (state, keyring, audit) = artifact_management_state().await;
    let reason = || Json(serde_json::from_value(json!({"reason": "test"})).unwrap());

    let promote = promote_artifact(
        State(state.clone()),
        management_headers("admin-token"),
        reason(),
    )
    .await
    .unwrap_err();
    assert_eq!(promote.status, StatusCode::CONFLICT);
    assert_eq!(promote.code, "artifact_candidate_unavailable");

    let rollback = rollback_artifact(
        State(state.clone()),
        management_headers("admin-token"),
        reason(),
    )
    .await
    .unwrap_err();
    assert_eq!(rollback.status, StatusCode::CONFLICT);
    assert_eq!(rollback.code, "artifact_rollback_unavailable");

    let _ = tokio::fs::remove_file(keyring).await;
    let _ = tokio::fs::remove_file(audit).await;
}

/// A Gateway started without `--artifact-active` must say so, rather than
/// reporting an empty status that reads like a healthy artifact.
#[tokio::test]
async fn artifact_endpoints_report_when_no_artifact_is_configured() {
    let state = test_state_with_route(route()).await;
    let error = artifact_status(State(state.clone()), HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
    assert_eq!(error.code, "artifact_runtime_not_configured");

    let error = kill_artifact(
        State(state),
        HeaderMap::new(),
        Json(serde_json::from_value(json!({"killed": false})).unwrap()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "artifact_runtime_not_configured");
}

#[tokio::test]
async fn artifact_policy_overrides_auto_but_falls_back_when_control_revision_changes() {
    use urouter_artifact::{ArtifactSupportDomain, ExportGates, FeatureWeights, LinearPolicy};

    let mut state = test_state_with_route(route()).await;
    let catalog_revision = state.catalog.hashes().content.to_string();
    let route_revision = state.route.revision();
    let artifact = RouterArtifact::build(
        FEATURE_SCHEMA_VERSION,
        catalog_revision.clone(),
        route_revision.clone(),
        "sha256:dataset",
        42,
        100,
        LinearPolicy {
            baseline_tier: "efficient".to_owned(),
            promoted_tier: "capable".to_owned(),
            threshold_millis: 1_000,
            bias_millis: 2_000,
            weights: FeatureWeights {
                input_kib_millis: 0,
                message_millis: 0,
                tool_millis: 0,
                image_millis: 0,
                structured_millis: 0,
                reasoning_millis: 0,
            },
        },
        ArtifactSupportDomain {
            semantic_tasks: BTreeSet::from(["greeting".to_owned()]),
            maximum_input_text_bytes: 10_000,
            tools_supported: false,
        },
        ExportGates {
            reproducible: true,
            privacy_passed: true,
            support_domain_defined: true,
            counterfactual_passed: true,
            quality_lower_bound_millionths: 1,
            maximum_error_rate_millionths: 0,
            maximum_cost_regression_millionths: 0,
        },
    )
    .unwrap();
    let controller = ArtifactController::new(
        Some(artifact),
        RolloutPolicy {
            shadow: false,
            canary_basis_points: 0,
            minimum_samples: 100,
            operation_limit: 6,
        },
    );
    state.artifact = Some(ArtifactRuntime {
        controller,
        catalog_revision,
        route_revision,
    });
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let mut decision = state.route.decide(&state.catalog, &request).unwrap();
    let applied = apply_artifact_policy(&state, &request, "tenant-a", "task-a", &mut decision)
        .unwrap()
        .unwrap();
    assert_eq!(applied.source, DecisionSource::ActiveArtifact);
    assert_eq!(decision.tier, "capable");
    assert_eq!(decision.reason, "artifact_active");

    state.artifact.as_mut().unwrap().route_revision = "stale".to_owned();
    let mut fallback = state.route.decide(&state.catalog, &request).unwrap();
    let evidence = apply_artifact_policy(&state, &request, "tenant-a", "task-a", &mut fallback)
        .unwrap()
        .unwrap();
    assert_eq!(evidence.source, DecisionSource::Rule);
    assert_eq!(
        evidence.fallback_reason.as_deref(),
        Some("control_revision_changed")
    );
    assert_eq!(fallback.tier, "efficient");
}

#[tokio::test]
async fn controlled_exploration_requires_explicit_consent_and_records_propensity() {
    let mut state = test_state_with_route(route()).await;
    state.exploration = ExplorationPolicy {
        epsilon_millionths: 1_000_000,
        maximum_budget_nano_usd: 10_000,
    };
    let mut request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}],
        "urouter": {
            "contract_version": 2,
            "task": {"id": "task-explore"},
            "agent": {
                "harness": "test",
                "prompt_profile_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "toolset_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
            },
            "call": {"role": "primary"},
            "trace": {
                "turn": "turn-1",
                "conversation": "conversation-1",
                "branch": "main"
            },
            "data_policy": {
                "recording": "metadata_only",
                "allow_training": true,
                "allow_exploration": true,
                "exploration_budget_nano_usd": 5000
            }
        }
    });
    let mut decision = state.route.decide(&state.catalog, &request).unwrap();
    let evidence = apply_controlled_exploration(
        &state,
        &request,
        "tenant-a",
        "task-explore",
        false,
        &mut decision,
    )
    .unwrap()
    .unwrap();
    assert_eq!(evidence.epsilon_millionths, 1_000_000);
    assert!(evidence.propensity_millionths > 0);
    assert!(evidence.eligible_set.len() >= 2);
    assert!(evidence.selected_by_exploration);

    request["urouter"]["data_policy"]["allow_exploration"] = json!(false);
    let mut denied = state.route.decide(&state.catalog, &request).unwrap();
    assert!(
        apply_controlled_exploration(
            &state,
            &request,
            "tenant-a",
            "task-explore",
            false,
            &mut denied,
        )
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn cache_affinity_prefers_the_last_successful_deployment_and_is_bounded() {
    let mut configured = route();
    let model = configured.tiers[0].model.clone();
    configured.tiers[0].deployments = vec![
        RouteDeployment {
            id: "cold".to_owned(),
            model: model.clone(),
            base_url: None,
            weight: 1,
            order: 0,
            provider_scope: None,
            credential_scope: None,
            enabled: true,
            credential_available: true,
            region: None,
            residency: Vec::new(),
            tenant_allowlist: Vec::new(),
            quota_usage_millis: None,
            accept_new_requests: true,
            binding_grace_until_unix: None,
        },
        RouteDeployment {
            id: "warm".to_owned(),
            model,
            base_url: None,
            weight: 1,
            order: 10,
            provider_scope: None,
            credential_scope: None,
            enabled: true,
            credential_available: true,
            region: None,
            residency: Vec::new(),
            tenant_allowlist: Vec::new(),
            quota_usage_millis: None,
            accept_new_requests: true,
            binding_grace_until_unix: None,
        },
    ];
    let mut state = test_state_with_route(configured).await;
    state.cache_affinity = CacheAffinityStore::new(1);
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let mut decision = state.route.decide(&state.catalog, &request).unwrap();
    decision.prompt_profile_hash = Some("sha256:profile-a".to_owned());
    state
        .cache_affinity
        .remember(decision.prompt_profile_hash.as_deref(), "warm");
    let tier = execution_tier(&state, &decision, "efficient").unwrap();
    let candidates = filter_deployments(&state, &tier, &decision, "tenant-a", &mut Vec::new());
    assert_eq!(
        candidates
            .iter()
            .find(|item| item.id == "warm")
            .unwrap()
            .order,
        0
    );
    assert!(
        candidates
            .iter()
            .find(|item| item.id == "cold")
            .unwrap()
            .order
            > 0
    );

    state
        .cache_affinity
        .remember(Some("sha256:profile-b"), "cold");
    assert!(
        state
            .cache_affinity
            .preferred(Some("sha256:profile-a"))
            .is_none()
    );
}

#[tokio::test]
async fn baseline_http_contract_routes_through_axum() {
    let app = app_router(test_state_with_route(route()).await);

    let health_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health_response.status(), StatusCode::OK);

    let contract_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(contract_response.status(), StatusCode::OK);
    assert_eq!(
        contract_response.headers()[header::CONTENT_TYPE],
        "application/json"
    );

    let models_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(models_response.status(), StatusCode::OK);
    let models_body = to_bytes(models_response.into_body(), 1_048_576)
        .await
        .unwrap();
    let models_json: Value = serde_json::from_slice(&models_body).unwrap();
    assert_eq!(models_json["object"], "list");
    assert!(models_json["data"].as_array().unwrap().iter().any(|model| {
        model["id"] == "urouter/auto" && model["urouter"]["contract_version"] == 2
    }));

    let explain_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/explain")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(explain_response.status(), StatusCode::OK);
    let explain_body = to_bytes(explain_response.into_body(), 1_048_576)
        .await
        .unwrap();
    let explain_json: Value = serde_json::from_slice(&explain_body).unwrap();
    assert_eq!(explain_json["schema_version"], 1);
    assert_eq!(explain_json["feature_frame"]["schema_version"], 1);
    assert_eq!(explain_json["routing_trace"]["completeness"], "summary");
    assert_nonempty_cascade_trace(&explain_json);
    assert_eq!(explain_json["route_id"], "urouter/auto");
    assert!(explain_json["tier"].is_string());
    assert!(explain_json["model"].is_string());

    let error_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/explain")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "not-a-route",
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(error_response.status(), StatusCode::BAD_REQUEST);
    let error_body = to_bytes(error_response.into_body(), 1_048_576)
        .await
        .unwrap();
    let error_json: Value = serde_json::from_slice(&error_body).unwrap();
    assert_eq!(error_json["error"]["type"], "urouter_error");
    assert_eq!(error_json["error"]["code"], "route_rejected");
}

#[tokio::test]
async fn readiness_rejects_traffic_while_draining() {
    let state = test_state_with_route(route()).await;
    let accepting = Arc::clone(&state.accepting);
    let app = app_router(state);

    let ready = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);

    accepting.store(false, Ordering::SeqCst);
    let draining = app
        .oneshot(
            Request::builder()
                .uri("/health/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(draining.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn metrics_v2_exposes_histograms_without_high_cardinality_labels() {
    let state = test_state_with_route(route()).await;
    observe_execution_metrics(&state.metrics, 25, 20, 1, None, Some("decision-test"));
    state.metrics.ttft_ms.observe(10);
    let response = metrics(State(state), HeaderMap::new()).await.unwrap();
    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    for metric in [
        "urouter_llm_calls_total",
        "urouter_upstream_attempts_total",
        "urouter_usage_unavailable_total",
        "urouter_cache_affinity_hit_total",
        "urouter_vector_write_total",
        "urouter_vector_write_errors_total",
        "urouter_request_duration_milliseconds_bucket",
        "urouter_upstream_duration_milliseconds_bucket",
        "urouter_time_to_first_token_milliseconds_bucket",
        "urouter_request_cost_nano_usd_bucket",
        "urouter_fallback_depth_bucket",
    ] {
        assert!(body.contains(metric), "missing {metric}");
    }
    for forbidden in ["tenant=", "task=", "request_id="] {
        assert!(
            !body.contains(forbidden),
            "forbidden metric label {forbidden}"
        );
    }
    assert!(body.contains("# {trace_id=\"decision-test\"} 25"));
    assert!(body.ends_with("# EOF\n"));
}

#[tokio::test]
async fn graceful_shutdown_drains_an_in_flight_request() {
    let started = Arc::new(tokio::sync::Notify::new());
    let handler_started = Arc::clone(&started);
    let app = Router::new().route(
        "/slow",
        get(move || {
            let handler_started = Arc::clone(&handler_started);
            async move {
                handler_started.notify_one();
                sleep(Duration::from_millis(50)).await;
                "complete"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepting = Arc::new(AtomicBool::new(true));
    let server_accepting = Arc::clone(&accepting);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        serve_with_shutdown(listener, app, server_accepting, 1, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    let response = tokio::spawn(async move {
        // `reqwest::get` uses a default client that honours an ambient
        // `http_proxy`, which would send this loopback request to the proxy
        // and hang the drain assertion below.
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/slow"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    started.notified().await;
    shutdown_tx.send(()).unwrap();
    for _ in 0..20 {
        if !accepting.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(!accepting.load(Ordering::SeqCst));
    assert_eq!(response.await.unwrap(), "complete");
    server.await.unwrap();
}

#[test]
fn upstream_proxy_defaults_to_disabled_and_ignores_the_ambient_environment() {
    let args = Args::try_parse_from(["urouter-gateway"]).unwrap();
    assert!(args.upstream_proxy.is_none());
    assert!(args.upstream_no_proxy.is_none());
    validate_args(&args).unwrap();
    // Builds even with an ambient proxy in the environment; `no_proxy()`
    // clears the auto-detected system proxy.
    upstream_client(&args).unwrap();
}

#[test]
fn explicit_upstream_proxy_is_accepted_with_exceptions() {
    let args = Args::try_parse_from([
        "urouter-gateway",
        "--upstream-proxy",
        "http://127.0.0.1:3128",
        "--upstream-no-proxy",
        "127.0.0.1,localhost",
    ])
    .unwrap();
    validate_args(&args).unwrap();
    upstream_client(&args).unwrap();
}

#[test]
fn upstream_no_proxy_without_a_proxy_is_rejected() {
    let args =
        Args::try_parse_from(["urouter-gateway", "--upstream-no-proxy", "127.0.0.1"]).unwrap();
    let error = validate_args(&args).unwrap_err();
    assert!(error.to_string().contains("require --upstream-proxy"));
}

#[test]
fn an_invalid_upstream_proxy_is_rejected_at_startup() {
    let args = Args::try_parse_from(["urouter-gateway", "--upstream-proxy", "not a url"]).unwrap();
    let error = validate_args(&args).unwrap_err();
    assert!(error.to_string().contains("invalid upstream proxy"));
}

#[test]
fn zero_shutdown_grace_is_rejected() {
    let mut args = Args::try_parse_from(["urouter-gateway"]).unwrap();
    args.shutdown_grace_seconds = 0;
    let error = validate_args(&args).unwrap_err();
    assert!(error.to_string().contains("shutdown grace period"));
}

#[test]
fn redis_authoritative_state_rejects_local_persistence_files() {
    let args = Args::try_parse_from([
        "urouter-gateway",
        "--redis-url",
        "redis://127.0.0.1:6379/",
        "--records",
        "/tmp/records.jsonl",
    ])
    .unwrap();
    let error = validate_args(&args).unwrap_err();
    assert!(error.to_string().contains("Redis authoritative state"));
}

fn record_for(request: &Value, decision_id: &str) -> DecisionRecord {
    let catalog =
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
    let route = route();
    let decision = route.decide(&catalog, request).unwrap();
    let model = catalog.model(&decision.model).unwrap();
    let record = RecordSeed {
        request_id: "req-test".to_owned(),
        messages_hash: messages_hash(request).unwrap(),
        features: FeatureFrame::from_openai_chat(request),
        route_revision: route.revision(),
        artifact: None,
        exploration: None,
        vector_ref: None,
    };
    build_record(
        &catalog,
        decision_id.to_owned(),
        decision,
        model,
        &test_governance(),
        &record,
        successful_execution(),
    )
    .unwrap()
}

#[test]
fn value_stats_separate_downgrade_and_cache_savings() {
    let mut document: Value =
        serde_json::from_str(include_str!("../../../catalog/catalog.json")).unwrap();
    for model in document["models"].as_array_mut().unwrap() {
        match model["id"].as_str() {
            Some("local-vllm/qwen3.5-4b") => {
                model["cost"]["base"] =
                    json!({"input": "2", "output": "4", "cache_read": "0.2", "cache_write": "2.5"});
            }
            Some("local-vllm-qwen38/qwen3.8-27b") => {
                model["cost"]["base"] = json!({"input": "10", "output": "20", "cache_read": "1", "cache_write": "12.5"});
            }
            _ => {}
        }
    }
    let catalog = CatalogSnapshot::from_document(
        serde_json::from_value(document).expect("valid fixture catalog"),
    )
    .unwrap();
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let mut record = record_for(&request, "value-stats");
    record.evidence = CatalogEvidence::from_catalog(
        &catalog,
        &ModelId::new("local-vllm/qwen3.5-4b").unwrap(),
        PriceSource::Catalog,
    )
    .unwrap();
    record.execution.usage = Some(Usage {
        input: 100,
        output: 100,
        cache_read: 900,
        ..Usage::default()
    });
    record.outcome_signals = vec![FeedbackSignal {
        kind: "quality".to_owned(),
        strength: 0.8,
    }];
    let capable = catalog
        .model(&ModelId::new("local-vllm-qwen38/qwen3.8-27b").unwrap())
        .unwrap();

    let stats = calculate_value_stats(&[record], &catalog, Some(capable));

    assert_eq!(stats.actual_cost_nano_usd, 780_000);
    assert_eq!(stats.cache_savings_nano_usd, 1_620_000);
    assert_eq!(stats.downgrade_savings_nano_usd, 9_600_000);
    assert_eq!(stats.total_savings_nano_usd, 11_220_000);
    assert_eq!(stats.quality_mean, Some(0.8));
    assert!((stats.quality_loss_vs_perfect.unwrap() - 0.2).abs() < f64::EPSILON);
}

#[test]
fn decision_record_v2_pins_features_candidates_and_revisions() {
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let record = record_for(&request, "decision-v2");
    let context = record.context.as_ref().unwrap();
    assert_eq!(context.schema_version, 2);
    assert_eq!(context.request_id, "req-test");
    assert_eq!(context.revisions.feature_schema, FEATURE_SCHEMA_VERSION);
    assert!(context.revisions.catalog.starts_with("sha256:"));
    assert!(context.revisions.route.starts_with("sha256:"));
    assert!(!context.eligible_candidates.is_empty());
    assert!(context.training_complete());
    assert_eq!(
        context.trace.completeness,
        urouter_contracts::TraceCompleteness::Full
    );
    assert_eq!(context.trace.policy_filters.len(), 3);
}

#[tokio::test]
async fn authorized_semantic_vector_is_side_stored_and_only_referenced() {
    let mut state = test_state_with_route(route()).await;
    let root = std::env::temp_dir().join(format!(
        "urouter-gateway-vectors-{}-{}",
        std::process::id(),
        unix_seconds()
    ));
    let store = VectorSideStore::open(&root, 1_024).await.unwrap();
    state.vector_store = Some(Arc::clone(&store));
    let mut governance = test_governance();
    governance.policy.allow_training = true;
    governance.policy.recording = RecordingMode::MetadataOnly;
    governance.compatibility_mode = false;
    let request = json!({
        "urouter": {"semantic_vector": [0.25, -0.5, 1.0]}
    });
    let reference = persist_semantic_vector(&state, &request, &governance)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reference.dimensions, 3);
    assert_eq!(store.read(&reference).await.unwrap(), vec![0.25, -0.5, 1.0]);
    assert!(!serde_json::to_string(&reference).unwrap().contains("0.25"));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn semantic_vector_requires_configured_store_when_training_is_authorized() {
    let state = test_state_with_route(route()).await;
    let mut governance = test_governance();
    governance.policy.allow_training = true;
    governance.policy.recording = RecordingMode::MetadataOnly;
    governance.compatibility_mode = false;
    let error = persist_semantic_vector(
        &state,
        &json!({"urouter": {"semantic_vector": [1.0]}}),
        &governance,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "vector_store_unavailable");
}

#[test]
fn legacy_record_remains_readable_but_is_not_training_eligible() {
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "legacy"}],
        "urouter": {"data_policy": {"allow_training": true}}
    });
    let mut value = serde_json::to_value(record_for(&request, "decision-v1")).unwrap();
    value.as_object_mut().unwrap().remove("context");
    value["training_eligible"] = Value::Bool(true);
    let record = serde_json::from_value::<DecisionRecord>(value)
        .unwrap()
        .normalize_after_load();
    assert!(record.context.is_none());
    assert!(!record.training_eligible);
}

async fn test_state_with_route(route: RouteConfig) -> AppState {
    let catalog = Arc::new(
        CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap(),
    );
    route.validate(&catalog).unwrap();
    let capacity = CapacityManager::new(CooldownPolicy::default());
    for tier in &route.tiers {
        capacity.register(&tier.effective_deployments());
    }
    let records = RecordStore::open(None, 10, 1, 0).await.unwrap();
    let route = Arc::new(route);
    let control = ControlPlane::new(
        ControlSnapshot::from_validated(Arc::clone(&catalog), Arc::clone(&route)),
        ControlFailurePolicy::LastGood,
        None,
    );
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    AppState {
        catalog,
        route,
        client: client.clone(),
        credentials: CredentialManager::new(client),
        records: records.clone(),
        record_repository: MemoryDecisionRecordRepository::new(records),
        feedback: FeedbackStore::default(),
        metrics: GatewayMetrics::default(),
        request_timeout: Duration::from_secs(2),
        retry_policy: RetryPolicy {
            max_retries: 1,
            base_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        capacity,
        shared_circuits: Arc::new(LocalCircuitRepository::default()),
        max_fallback_depth: 5,
        bindings: Arc::new(MemoryTaskBindingRepository::new(100)),
        idempotency: MemoryIdempotencyRepository::new(100),
        idempotency_ttl_seconds: 86_400,
        quota: MemoryQuotaRepository::new(0, 0, 0),
        quota_default_max_output_tokens: 4_096,
        budget: MemoryBudgetRepository::new(0, 2_592_000),
        shared_state: None,
        require_tenant_header: false,
        management_auth: ManagementAuth::disabled(),
        accepting: Arc::new(AtomicBool::new(true)),
        control,
        control_source: None,
        artifact: None,
        exploration: ExplorationPolicy {
            epsilon_millionths: 0,
            maximum_budget_nano_usd: 0,
        },
        cache_affinity: CacheAffinityStore::new(100),
        vector_store: None,
    }
}

async fn test_budget_accounting(
    state: &AppState,
    tenant_key: &str,
    request_id: &str,
    decision: &RouteDecision,
    request: &Value,
) -> BudgetAccounting {
    let (lease, input_tokens, output_tokens) =
        acquire_request_budget(state, tenant_key, request_id, decision, request)
            .await
            .unwrap();
    BudgetAccounting {
        lease,
        input_tokens,
        output_tokens,
    }
}

async fn execute_test_request(
    state: &AppState,
    decision: &RouteDecision,
    request: &Value,
    request_id: &str,
) -> Result<UpstreamExecution, RoutedFailure> {
    execute_routed_upstream(state, decision, request, request_id, &tenant_key("local")).await
}

fn deployment(id: &str, model: &str, base_url: String, order: u16) -> RouteDeployment {
    RouteDeployment {
        id: id.to_owned(),
        model: ModelId::new(model).unwrap(),
        base_url: Some(base_url),
        weight: 1,
        order,
        provider_scope: None,
        credential_scope: None,
        enabled: true,
        credential_available: true,
        region: None,
        residency: Vec::new(),
        tenant_allowlist: Vec::new(),
        quota_usage_millis: None,
        accept_new_requests: true,
        binding_grace_until_unix: None,
    }
}

fn set_tier_deployments(
    route: &mut RouteConfig,
    tier_name: &str,
    deployments: Vec<RouteDeployment>,
) {
    route
        .tiers
        .iter_mut()
        .find(|tier| tier.tier == tier_name)
        .unwrap()
        .deployments = deployments;
}

fn primary_request(task: &str, turn: &str, difficulty: Option<&str>) -> Value {
    let mut request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "continue the task"}],
        "urouter": {
            "contract_version": 1,
            "task": {"id": task},
            "agent": {"harness": "aionui"},
            "call": {"role": "primary"},
            "trace": {"turn": turn},
            "data_policy": {
                "recording": "metadata_only",
                "allow_training": false,
                "allow_remote_judge": false,
                "retention_days": 7
            }
        }
    });
    if let Some(difficulty) = difficulty {
        request["urouter"]["hint"]["difficulty"] = Value::String(difficulty.to_owned());
    }
    request
}

fn session_request(
    task: &str,
    conversation: &str,
    branch: &str,
    turn: &str,
    difficulty: Option<&str>,
) -> Value {
    let mut request = primary_request(task, turn, difficulty);
    request["urouter"]["contract_version"] = json!(2);
    request["urouter"]["trace"]["conversation"] = json!(conversation);
    request["urouter"]["trace"]["branch"] = json!(branch);
    request["urouter"]["agent"]["prompt_profile_hash"] =
        json!("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    request["urouter"]["agent"]["toolset_hash"] =
        json!("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    request
}

fn adapter_headers(conversation: &str, turn: &str, call_kind: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-urouter-conversation-id", conversation.parse().unwrap());
    headers.insert("x-urouter-turn-id", turn.parse().unwrap());
    headers.insert("x-urouter-call-kind", call_kind.parse().unwrap());
    headers
}

async fn commit_decision_binding(
    state: &AppState,
    governance: &RequestGovernance,
    decision: &RouteDecision,
) {
    commit_task_binding(state, governance, decision, &decision.tier, &decision.model)
        .await
        .unwrap();
}

#[test]
fn rewrites_model_contract_role_and_token_field() {
    let mut request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "developer", "content": "rules"}],
        "max_output_tokens": 12,
        "urouter": {"contract_version": 1}
    });
    rewrite_request(&mut request, &model());
    assert_eq!(request["model"], "Qwen3.5-4B");
    assert_eq!(request["messages"][0]["role"], "system");
    assert_eq!(request["max_tokens"], 12);
    assert!(request.get("urouter").is_none());
}

#[tokio::test]
async fn primary_task_keeps_exact_model_across_later_calls() {
    let state = test_state_with_route(route()).await;
    let governance = test_governance();
    let first = primary_request("task-stable", "turn-1", Some("hard"));
    let first_decision = state.route.decide(&state.catalog, &first).unwrap();
    assert_eq!(first_decision.tier, "capable");
    commit_decision_binding(&state, &governance, &first_decision).await;

    let later = primary_request("task-stable", "turn-2", None);
    let mut later_decision = state.route.decide(&state.catalog, &later).unwrap();
    assert_eq!(later_decision.tier, "efficient");
    apply_task_binding(&state, &governance, &mut later_decision)
        .await
        .unwrap();
    assert_eq!(later_decision.tier, "capable");
    assert_eq!(later_decision.model, first_decision.model);
    assert_eq!(later_decision.reason, "task_binding");
    assert_eq!(state.metrics.bindings_applied.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn session_binding_is_branch_scoped_and_requires_safe_identity_migration() {
    let state = test_state_with_route(route()).await;
    let governance = test_governance();
    let first = session_request(
        "task-session",
        "conversation-a",
        "main",
        "turn-1",
        Some("hard"),
    );
    let first_decision = state.route.decide(&state.catalog, &first).unwrap();
    commit_task_binding(
        &state,
        &governance,
        &first_decision,
        &first_decision.tier,
        &first_decision.model,
    )
    .await
    .unwrap();

    let later = session_request("task-session", "conversation-a", "main", "turn-2", None);
    let mut later_decision = state.route.decide(&state.catalog, &later).unwrap();
    apply_task_binding(&state, &governance, &mut later_decision)
        .await
        .unwrap();
    assert_eq!(later_decision.model, first_decision.model);
    assert_eq!(later_decision.reason, "session_binding");

    let other_branch = session_request(
        "task-session",
        "conversation-a",
        "experiment",
        "turn-3",
        None,
    );
    let mut other_decision = state.route.decide(&state.catalog, &other_branch).unwrap();
    apply_task_binding(&state, &governance, &mut other_decision)
        .await
        .unwrap();
    assert_eq!(other_decision.reason, "default_efficient");

    let mut changed = session_request("task-session", "conversation-a", "main", "turn-4", None);
    changed["urouter"]["agent"]["toolset_hash"] =
        json!("sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
    let mut changed_decision = state.route.decide(&state.catalog, &changed).unwrap();
    let error = apply_task_binding(&state, &governance, &mut changed_decision)
        .await
        .unwrap_err();
    assert_eq!(error.code, "unsafe_session_migration");

    changed["urouter"]["call"]["migration_boundary"] = json!("after_compaction");
    let mut migrated = state.route.decide(&state.catalog, &changed).unwrap();
    apply_task_binding(&state, &governance, &mut migrated)
        .await
        .unwrap();
    assert_eq!(migrated.reason, "session_migration");
    commit_task_binding(
        &state,
        &governance,
        &migrated,
        &migrated.tier,
        &migrated.model,
    )
    .await
    .unwrap();
    let key = session_scope_key(&governance.tenant_key, "conversation-a", "main");
    let binding = state.bindings.get(&key).await.unwrap().unwrap();
    assert_eq!(binding.generation, 2);
    assert_eq!(binding.toolset_hash, migrated.toolset_hash);
    assert_eq!(
        state
            .bindings
            .remove_task(
                &governance.tenant_key,
                &task_scope_key(&governance.tenant_key, "task-session"),
            )
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn aionui_adapter_preserves_primary_session_and_bypasses_title_call() {
    let state = test_state_with_route(route()).await;
    let governance = test_governance();
    let mut first = json!({
        "model": "urouter/auto",
        "messages": [
            {"role": "system", "content": "stable agent rules"},
            {"role": "user", "content": "plan a hard migration"}
        ],
        "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}]
    });
    adapt_agent_request(
        "aionui",
        &adapter_headers("conversation-adapter", "turn-1", "plan"),
        &mut first,
    )
    .unwrap();
    first["urouter"]["hint"]["difficulty"] = json!("hard");
    let first_decision = state.route.decide(&state.catalog, &first).unwrap();
    assert_eq!(first_decision.tier, "capable");
    commit_decision_binding(&state, &governance, &first_decision).await;

    let mut continuation = json!({
        "model": "urouter/auto",
        "messages": [
            {"role": "system", "content": "stable agent rules"},
            {"role": "user", "content": "continue"}
        ],
        "tools": [{"function": {"parameters": {"type": "object"}, "name": "read"}, "type": "function"}]
    });
    adapt_agent_request(
        "aionui",
        &adapter_headers("conversation-adapter", "turn-2", "primary"),
        &mut continuation,
    )
    .unwrap();
    let mut continued = state.route.decide(&state.catalog, &continuation).unwrap();
    apply_task_binding(&state, &governance, &mut continued)
        .await
        .unwrap();
    assert_eq!(continued.model, first_decision.model);
    assert_eq!(continued.reason, "session_binding");
    commit_decision_binding(&state, &governance, &continued).await;

    let mut title = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "make a short title"}]
    });
    adapt_agent_request(
        "aionui",
        &adapter_headers("conversation-adapter", "turn-title", "title"),
        &mut title,
    )
    .unwrap();
    let mut title_decision = state.route.decide(&state.catalog, &title).unwrap();
    apply_task_binding(&state, &governance, &mut title_decision)
        .await
        .unwrap();
    assert_eq!(title_decision.tier, "efficient");
    assert_eq!(title_decision.call_role, Some(CallRole::Auxiliary));
    commit_decision_binding(&state, &governance, &title_decision).await;

    let binding = state
        .bindings
        .get(&session_scope_key(
            &governance.tenant_key,
            "conversation-adapter",
            "main",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.model, first_decision.model);
    assert_eq!(binding.last_seen_turn.as_deref(), Some("turn-2"));

    continuation["tools"] = json!([]);
    continuation.as_object_mut().unwrap().remove("urouter");
    let mut changed_headers = adapter_headers("conversation-adapter", "turn-3", "primary");
    changed_headers.insert("x-urouter-task-id", "conversation-adapter".parse().unwrap());
    adapt_agent_request("aionui", &changed_headers, &mut continuation).unwrap();
    let mut changed = state.route.decide(&state.catalog, &continuation).unwrap();
    assert_eq!(
        apply_task_binding(&state, &governance, &mut changed)
            .await
            .unwrap_err()
            .code,
        "unsafe_session_migration"
    );
}

#[tokio::test]
async fn explicit_retry_boundary_upgrades_bound_task_once() {
    let state = test_state_with_route(route()).await;
    let governance = test_governance();
    let first = primary_request("task-retry", "turn-1", None);
    let first_decision = state.route.decide(&state.catalog, &first).unwrap();
    commit_task_binding(
        &state,
        &governance,
        &first_decision,
        &first_decision.tier,
        &first_decision.model,
    )
    .await
    .unwrap();

    let mut retry = primary_request("task-retry", "turn-2", None);
    retry["urouter"]["call"]["migration_boundary"] =
        Value::String("explicit_user_retry".to_owned());
    let mut retry_decision = state.route.decide(&state.catalog, &retry).unwrap();
    apply_task_binding(&state, &governance, &mut retry_decision)
        .await
        .unwrap();
    assert_eq!(retry_decision.tier, "capable");
    assert_eq!(retry_decision.reason, "task_migration");
    commit_task_binding(
        &state,
        &governance,
        &retry_decision,
        &retry_decision.tier,
        &retry_decision.model,
    )
    .await
    .unwrap();
    assert_eq!(state.metrics.binding_migrations.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn stale_concurrent_decision_cannot_overwrite_first_binding() {
    let state = test_state_with_route(route()).await;
    let governance = test_governance();
    let hard = primary_request("task-race", "turn-hard", Some("hard"));
    let normal = primary_request("task-race", "turn-normal", None);
    let hard_decision = state.route.decide(&state.catalog, &hard).unwrap();
    let stale_normal = state.route.decide(&state.catalog, &normal).unwrap();

    commit_task_binding(
        &state,
        &governance,
        &hard_decision,
        &hard_decision.tier,
        &hard_decision.model,
    )
    .await
    .unwrap();
    commit_task_binding(
        &state,
        &governance,
        &stale_normal,
        &stale_normal.tier,
        &stale_normal.model,
    )
    .await
    .unwrap();

    let binding = state
        .bindings
        .get(&task_scope_key(&governance.tenant_key, "task-race"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.model, hard_decision.model);
    assert_eq!(binding.bound_at_turn.as_deref(), Some("turn-hard"));
    assert_eq!(state.metrics.binding_conflicts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn auxiliary_and_compatibility_calls_do_not_change_binding() {
    let state = test_state_with_route(route()).await;
    let governance = test_governance();
    let primary = primary_request("task-bypass", "turn-1", Some("hard"));
    let primary_decision = state.route.decide(&state.catalog, &primary).unwrap();
    commit_task_binding(
        &state,
        &governance,
        &primary_decision,
        &primary_decision.tier,
        &primary_decision.model,
    )
    .await
    .unwrap();
    let key = task_scope_key(&governance.tenant_key, "task-bypass");
    let original = state.bindings.get(&key).await.unwrap().unwrap();

    let auxiliary = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "title"}],
        "urouter": {
            "task": {"id": "task-bypass"},
            "agent": {"harness": "aionui"},
            "call": {"role": "auxiliary"},
            "trace": {"turn": "turn-title"}
        }
    });
    let mut auxiliary_decision = state.route.decide(&state.catalog, &auxiliary).unwrap();
    apply_task_binding(&state, &governance, &mut auxiliary_decision)
        .await
        .unwrap();
    assert_eq!(auxiliary_decision.tier, "efficient");
    commit_task_binding(
        &state,
        &governance,
        &auxiliary_decision,
        &auxiliary_decision.tier,
        &auxiliary_decision.model,
    )
    .await
    .unwrap();
    assert_eq!(
        state.bindings.get(&key).await.unwrap().unwrap().model,
        original.model
    );

    let legacy = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "legacy"}]
    });
    let legacy_decision = state.route.decide(&state.catalog, &legacy).unwrap();
    assert!(legacy_decision.compatibility_mode);
    commit_task_binding(
        &state,
        &governance,
        &legacy_decision,
        &legacy_decision.tier,
        &legacy_decision.model,
    )
    .await
    .unwrap();
    assert_eq!(
        state.bindings.get(&key).await.unwrap().unwrap().model,
        original.model
    );
}

#[tokio::test]
async fn binding_store_is_bounded_and_does_not_retain_raw_task_ids() {
    let store = MemoryTaskBindingRepository::new(1);
    let tenant = tenant_key("test-tenant");
    for task in ["sensitive-task-one", "sensitive-task-two"] {
        store
            .put(
                TaskBinding {
                    tenant_key: tenant.clone(),
                    binding_key: task_scope_key(&tenant, task),
                    task_key: task_scope_key(&tenant, task),
                    conversation_key: None,
                    branch_key: None,
                    tier: "efficient".to_owned(),
                    model: ModelId::new("local-vllm/qwen3.5-4b").unwrap(),
                    provider: "local-vllm".to_owned(),
                    api: "open_ai_chat".to_owned(),
                    agent_harness: Some("aionui".to_owned()),
                    prompt_profile_hash: None,
                    toolset_hash: None,
                    bound_at_turn: Some("turn".to_owned()),
                    last_seen_turn: Some("turn".to_owned()),
                    generation: 1,
                    tenant_generation: 0,
                    task_generation: 0,
                },
                false,
            )
            .await
            .unwrap();
    }
    assert!(
        store
            .get(&task_scope_key(&tenant, "sensitive-task-one"))
            .await
            .unwrap()
            .is_none()
    );
    let retained = store
        .get(&task_scope_key(&tenant, "sensitive-task-two"))
        .await
        .unwrap()
        .unwrap();
    assert!(!retained.task_key.contains("sensitive-task-two"));
    assert!(store.remove(&retained.task_key).await.unwrap());
}

#[test]
fn parses_terminal_stream_usage() {
    let usage = parse_stream_usage(
        b"data: {\"choices\":[]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\ndata: [DONE]\n",
    )
    .unwrap();
    assert_eq!(usage.input, 12);
    assert_eq!(usage.output, 3);
}

#[test]
fn removes_done_marker_from_stream_tail() {
    let tail = remove_sse_done(b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n".to_vec());
    assert_eq!(tail, b"data: {\"choices\":[]}\n\n\n\n");
}

#[test]
fn override_pair_requires_same_trace_and_messages() {
    let parent_request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "solve"}],
        "urouter": {"trace": {"id": "trace-1", "turn": "turn-1"}}
    });
    let parent = record_for(&parent_request, "decision-1");
    let child_request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "solve"}],
        "urouter": {
            "trace": {"id": "trace-1", "turn": "turn-2", "parent_turn": "turn-1"},
            "preference": {"pin_tier": "capable"}
        }
    });
    let child = record_for(&child_request, "decision-2");
    let records = VecDeque::from([parent]);
    let paired = evaluate_override(&route(), &child, &records);
    assert!(paired.paired);
    assert_eq!(paired.kind.as_deref(), Some("escalate"));

    let changed_request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "solve a different problem"}],
        "urouter": {
            "trace": {"id": "trace-1", "turn": "turn-3", "parent_turn": "turn-1"},
            "preference": {"pin_tier": "capable"}
        }
    });
    let changed = record_for(&changed_request, "decision-3");
    let rejected = evaluate_override(&route(), &changed, &records);
    assert!(!rejected.paired);
    assert_eq!(
        rejected.rejected_reason.as_deref(),
        Some("messages_mismatch")
    );
}

#[tokio::test]
async fn feedback_is_idempotent_per_turn_and_kind() {
    let store = FeedbackStore::default();
    let tenant = tenant_key("test-tenant");
    store
        .upsert(
            &tenant,
            "turn-1",
            vec![FeedbackSignal {
                kind: "accepted".to_owned(),
                strength: 0.5,
            }],
        )
        .await;
    let signals = store
        .upsert(
            &tenant,
            "turn-1",
            vec![FeedbackSignal {
                kind: "accepted".to_owned(),
                strength: 1.0,
            }],
        )
        .await;
    assert_eq!(signals.len(), 1);
    assert!((signals[0].strength - 1.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn feedback_events_recover_after_restart() {
    let path = std::env::temp_dir().join(format!(
        "urouter-feedback-test-{}.jsonl",
        next_decision_id()
    ));
    let store = FeedbackStore::open(Some(path.clone()), 4, 1_000_000)
        .await
        .unwrap();
    let tenant = tenant_key("test-tenant");
    store
        .upsert(
            &tenant,
            "turn-recover",
            vec![FeedbackSignal {
                kind: "task_succeeded".to_owned(),
                strength: 0.9,
            }],
        )
        .await;
    for _ in 0..50 {
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|metadata| metadata.len() > 0)
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    drop(store);
    let recovered = FeedbackStore::open(Some(path.clone()), 4, 1_000_000)
        .await
        .unwrap();
    let values = recovered.values.read().await;
    assert!(values[&feedback_scope_key(&tenant, "turn-recover")].contains_key("task_succeeded"));
    drop(values);
    let records = RecordStore::open(None, 2, 1, 0).await.unwrap();
    records
        .append(record_for(
            &json!({
                "model": "urouter/auto",
                "messages": [{"role": "user", "content": "recover"}],
                "urouter": {"trace": {"turn": "turn-recover"}}
            }),
            "decision-recover",
        ))
        .await;
    reconcile_feedback(&records, &recovered).await;
    assert_eq!(
        records.records.read().await[0].outcome_signals[0].kind,
        "task_succeeded"
    );
    drop(recovered);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn tenant_scope_isolates_feedback_and_task_bindings() {
    let state = test_state_with_route(route()).await;
    let tenant_a = RequestGovernance {
        tenant_key: tenant_key("tenant-a"),
        ..test_governance()
    };
    let tenant_b = RequestGovernance {
        tenant_key: tenant_key("tenant-b"),
        ..test_governance()
    };
    let request = primary_request("shared-task", "shared-turn", Some("hard"));
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    commit_task_binding(
        &state,
        &tenant_a,
        &decision,
        &decision.tier,
        &decision.model,
    )
    .await
    .unwrap();
    assert!(
        state
            .bindings
            .get(&task_scope_key(&tenant_a.tenant_key, "shared-task"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        state
            .bindings
            .get(&task_scope_key(&tenant_b.tenant_key, "shared-task"))
            .await
            .unwrap()
            .is_none()
    );

    state
        .feedback
        .upsert(
            &tenant_a.tenant_key,
            "shared-turn",
            vec![FeedbackSignal {
                kind: "accepted".to_owned(),
                strength: 1.0,
            }],
        )
        .await;
    let values = state.feedback.values.read().await;
    assert!(values.contains_key(&feedback_scope_key(&tenant_a.tenant_key, "shared-turn")));
    assert!(!values.contains_key(&feedback_scope_key(&tenant_b.tenant_key, "shared-turn")));
}

#[tokio::test]
#[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
#[allow(clippy::too_many_lines)]
async fn redis_records_and_feedback_are_cross_instance_consistent() {
    let url = std::env::var("UROUTER_TEST_REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:16380/".to_owned());
    let prefix = format!("urouter-shared-state-test-{}", next_decision_id());
    let mut state_a = test_state_with_route(route()).await;
    state_a.shared_state = Some(
        RedisSharedState::connect(&url, prefix.clone())
            .await
            .unwrap(),
    );
    state_a.idempotency =
        RedisIdempotencyRepository::new(state_a.shared_state.as_ref().unwrap().clone());
    state_a.record_repository = RedisDecisionRecordRepository::new(
        state_a.shared_state.as_ref().unwrap().clone(),
        state_a.records.clone(),
        state_a.records.capacity,
    );
    let mut state_b = test_state_with_route(route()).await;
    state_b.shared_state = Some(RedisSharedState::connect(&url, prefix).await.unwrap());
    state_b.idempotency =
        RedisIdempotencyRepository::new(state_b.shared_state.as_ref().unwrap().clone());
    state_b.record_repository = RedisDecisionRecordRepository::new(
        state_b.shared_state.as_ref().unwrap().clone(),
        state_b.records.clone(),
        state_b.records.capacity,
    );
    let tenant = tenant_key("shared-state-tenant");
    let idempotency_request = json!({"model": "urouter/auto", "messages": []});
    let headers = HeaderMap::from_iter([(
        HeaderName::from_static("idempotency-key"),
        HeaderValue::from_static("shared-operation"),
    )]);
    let first_request_id = resolve_request_id(
        &state_a,
        &headers,
        &tenant,
        &idempotency_request,
        "req_shared_first".to_owned(),
    )
    .await
    .unwrap();
    let second_request_id = resolve_request_id(
        &state_b,
        &headers,
        &tenant,
        &idempotency_request,
        "req_shared_second".to_owned(),
    )
    .await
    .unwrap();
    assert_eq!(first_request_id, second_request_id);
    let mut record = record_for(
        &primary_request("shared-state-task", "shared-state-turn", None),
        "shared-state-decision",
    );
    record.tenant_key.clone_from(&tenant);
    store_record(&state_a, record).await.unwrap();

    let records = records_for_tenant(&state_b, &tenant).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision_id, "shared-state-decision");
    ingest_feedback(
        &state_b,
        &tenant,
        "shared-state-turn",
        vec![FeedbackSignal {
            kind: "accepted".to_owned(),
            strength: 1.0,
        }],
        7,
    )
    .await
    .unwrap();
    let recovered = record_for_id(&state_a, &tenant, "shared-state-decision")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.outcome_signals[0].kind, "accepted");
    assert!(
        feedback_for_turn(&state_a, &tenant, "shared-state-turn")
            .await
            .unwrap()
            .is_some()
    );
    let (left, right) = tokio::join!(
        ingest_feedback(
            &state_a,
            &tenant,
            "shared-state-turn",
            vec![FeedbackSignal {
                kind: "disputed".to_owned(),
                strength: 0.8,
            }],
            7,
        ),
        ingest_feedback(
            &state_b,
            &tenant,
            "shared-state-turn",
            vec![FeedbackSignal {
                kind: "task_succeeded".to_owned(),
                strength: 0.9,
            }],
            7,
        )
    );
    left.unwrap();
    right.unwrap();
    let merged = feedback_for_turn(&state_a, &tenant, "shared-state-turn")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.len(), 3);
    let hydrated = record_for_id(&state_b, &tenant, "shared-state-decision")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hydrated.outcome_signals.len(), 3);

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-urouter-tenant-id",
        HeaderValue::from_static("shared-state-tenant"),
    );
    assert_eq!(
        delete_decision(
            State(state_b.clone()),
            Path("shared-state-decision".to_owned()),
            headers,
        )
        .await
        .unwrap(),
        StatusCode::NO_CONTENT
    );
    assert!(
        record_for_id(&state_a, &tenant, "shared-state-decision")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        feedback_for_turn(&state_a, &tenant, "shared-state-turn")
            .await
            .unwrap()
            .is_none()
    );

    let mut expiring = record_for(
        &primary_request("expiring-task", "expiring-turn", None),
        "expiring-decision",
    );
    expiring.tenant_key.clone_from(&tenant);
    expiring.expires_at_unix_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_add(1);
    let shared = state_a.shared_state.as_ref().unwrap();
    shared.put_record(&expiring, 10).await.unwrap();
    shared
        .upsert_feedback(
            &tenant,
            "expiring-turn",
            &[FeedbackSignal {
                kind: "accepted".to_owned(),
                strength: 1.0,
            }],
            1,
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(1_100)).await;
    assert!(
        shared
            .record(&tenant, "expiring-decision")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        shared
            .feedback(&tenant, "expiring-turn")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn tenant_header_can_be_mandatory() {
    let mut state = test_state_with_route(route()).await;
    state.require_tenant_header = true;
    let error = resolve_tenant(&state, &HeaderMap::new()).unwrap_err();
    assert_eq!(error.code, "tenant_required");

    let mut headers = HeaderMap::new();
    headers.insert("x-urouter-tenant-id", HeaderValue::from_static("tenant-a"));
    let (resolved, compatibility) = resolve_tenant(&state, &headers).unwrap();
    assert_eq!(resolved, tenant_key("tenant-a"));
    assert!(!compatibility);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn management_endpoints_enforce_role_and_tenant_scope() {
    let suffix = next_decision_id();
    let keyring_path = std::env::temp_dir().join(format!("urouter-keyring-{suffix}.json"));
    let audit_path = std::env::temp_dir().join(format!("urouter-audit-{suffix}.jsonl"));
    let tenant_a = tenant_key("tenant-a");
    let key = |id: &str, token: &str, role: &str, tenants: Vec<String>| {
        json!({
            "id": id,
            "token_sha256": format!("sha256:{:x}", Sha256::digest(token.as_bytes())),
            "role": role,
            "tenant_keys": tenants
        })
    };
    tokio::fs::write(
        &keyring_path,
        serde_json::to_vec(&json!({
            "version": 1,
            "keys": [
                key("reader", "reader-token", "reader", vec![tenant_a.clone()]),
                key("operator", "operator-token", "operator", vec![tenant_a.clone()]),
                key("admin", "admin-token", "admin", vec!["*".to_owned()])
            ]
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    let mut state = test_state_with_route(route()).await;
    state.management_auth = ManagementAuth::open(
        Some(keyring_path.clone()),
        Some(audit_path.clone()),
        16,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    let mut record = record_for(&primary_request("task-a", "turn-a", None), "decision-a");
    record.tenant_key.clone_from(&tenant_a);
    state.records.append(record).await;
    let request_headers = |tenant: &'static str, token: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert("x-urouter-tenant-id", HeaderValue::from_static(tenant));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    };

    assert!(
        decisions(
            State(state.clone()),
            request_headers("tenant-a", "reader-token"),
            Query(DecisionQuery::default()),
        )
        .await
        .is_ok()
    );
    assert_eq!(
        delete_decision(
            State(state.clone()),
            Path("decision-a".to_owned()),
            request_headers("tenant-a", "reader-token"),
        )
        .await
        .unwrap_err()
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        metrics(
            State(state.clone()),
            request_headers("tenant-a", "operator-token"),
        )
        .await
        .unwrap_err()
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        decisions(
            State(state.clone()),
            request_headers("tenant-b", "reader-token"),
            Query(DecisionQuery::default()),
        )
        .await
        .unwrap_err()
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        decisions(
            State(state.clone()),
            request_headers("tenant-a", "wrong-token"),
            Query(DecisionQuery::default()),
        )
        .await
        .unwrap_err()
        .status,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        metrics(
            State(state.clone()),
            request_headers("tenant-a", "admin-token"),
        )
        .await
        .is_ok()
    );
    assert_eq!(
        delete_decision(
            State(state.clone()),
            Path("decision-a".to_owned()),
            request_headers("tenant-a", "operator-token"),
        )
        .await
        .unwrap(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        resolve_tenant(&state, &HeaderMap::new()).unwrap_err().code,
        "tenant_required"
    );
    let audit = tokio::fs::read_to_string(&audit_path).await.unwrap();
    assert_eq!(audit.lines().count(), 7);
    assert!(!audit.contains("reader-token"));
    let _ = tokio::fs::remove_file(keyring_path).await;
    let _ = tokio::fs::remove_file(audit_path).await;
}

#[tokio::test]
async fn catalog_management_reports_status_and_stable_conflicts() {
    let state = test_state_with_route(route()).await;
    let status = catalog_status(State(state.clone()), HeaderMap::new())
        .await
        .unwrap();
    assert!(status.0.contains_key(header::ETAG));
    assert_eq!(status.1.0["hot_reload"], false);
    assert_eq!(status.1.0["control"]["ready"], true);

    let refresh = refresh_catalog(State(state.clone()), HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(refresh.status, StatusCode::CONFLICT);
    assert_eq!(refresh.code, "control_reload_not_configured");

    let rollback = rollback_catalog(State(state), HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(rollback.status, StatusCode::CONFLICT);
    assert_eq!(rollback.code, "control_rollback_unavailable");
}

#[tokio::test]
async fn recording_none_retains_no_decision_or_piggyback_feedback() {
    let state = test_state_with_route(route()).await;
    let mut governance = test_governance();
    governance.policy.recording = RecordingMode::None;
    governance.policy.allow_training = true;
    governance.policy.allow_remote_judge = true;
    let request = primary_request("private-task", "private-turn", None);
    let catalog = &state.catalog;
    let decision = state.route.decide(catalog, &request).unwrap();
    let model = catalog.model(&decision.model).unwrap();
    let record_seed = RecordSeed {
        request_id: "req-private".to_owned(),
        messages_hash: messages_hash(&request).unwrap(),
        features: FeatureFrame::from_openai_chat(&request),
        route_revision: state.route.revision(),
        artifact: None,
        exploration: None,
        vector_ref: None,
    };
    let record = build_record(
        catalog,
        "decision-private".to_owned(),
        decision,
        model,
        &governance,
        &record_seed,
        successful_execution(),
    )
    .unwrap();
    assert!(!record.training_eligible);
    assert!(!record.remote_judge_eligible);
    store_record(&state, record).await.unwrap();
    ingest_piggyback(
        &state,
        &governance,
        &[SignalContract {
            turn: Some("private-turn".to_owned()),
            kind: Some("accepted".to_owned()),
            strength: None,
        }],
    )
    .await
    .unwrap();
    assert!(state.records.records.read().await.is_empty());
    assert!(state.feedback.values.read().await.is_empty());
}

#[tokio::test]
async fn expired_records_are_pruned() {
    let store = RecordStore::open(None, 4, 1, 0).await.unwrap();
    let mut expired = record_for(
        &primary_request("old-task", "old-turn", None),
        "decision-old",
    );
    expired.expires_at_unix_s = 1;
    store.append(expired).await;
    assert_eq!(store.prune_expired().await.unwrap(), 1);
    assert!(store.records.read().await.is_empty());
}

#[tokio::test]
async fn persistent_delete_survives_restart_and_removes_rotated_copy() {
    let path = std::env::temp_dir().join(format!(
        "urouter-record-delete-test-{}.jsonl",
        next_decision_id()
    ));
    let rotated = PathBuf::from(format!("{}.1", path.display()));
    let store = RecordStore::open(Some(path.clone()), 8, 8, 1_000_000)
        .await
        .unwrap();
    let mut tenant_a = record_for(&primary_request("task-a", "turn-a", None), "decision-a");
    tenant_a.tenant_key = tenant_key("tenant-a");
    let mut tenant_b = record_for(&primary_request("task-b", "turn-b", None), "decision-b");
    tenant_b.tenant_key = tenant_key("tenant-b");
    store.append(tenant_a).await;
    store.append(tenant_b).await;
    for _ in 0..50 {
        if tokio::fs::read_to_string(&path)
            .await
            .is_ok_and(|contents| contents.contains("decision-b"))
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    tokio::fs::copy(&path, &rotated).await.unwrap();
    assert_eq!(
        store
            .delete_matching(|record| record.tenant_key == tenant_key("tenant-a"))
            .await
            .unwrap(),
        1
    );
    assert!(!tokio::fs::try_exists(&rotated).await.unwrap());
    drop(store);

    let recovered = RecordStore::open(Some(path.clone()), 8, 8, 1_000_000)
        .await
        .unwrap();
    let records = recovered.records.read().await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision_id, "decision-b");
    drop(records);
    drop(recovered);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn record_store_retains_configured_capacity() {
    let store = RecordStore::open(None, 2, 1, 0).await.unwrap();
    for index in 0..3 {
        let request = json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": format!("request {index}")}]
        });
        store
            .append(record_for(&request, &format!("decision-{index}")))
            .await;
    }
    let records = store.records.read().await;
    assert_eq!(records.len(), 2);
    assert_eq!(records.front().unwrap().decision_id, "decision-1");
}

async fn assert_record_repository_contract(repository: Arc<dyn DecisionRecordRepository>) {
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "record contract"}]
    });
    let mut record = record_for(&request, "decision-contract");
    let tenant = record.tenant_key.clone();
    record.task_key = Some("task-contract".to_owned());
    repository.put(record).await.unwrap();
    let other_tenant = tenant_key("record-contract-other");
    let mut other = record_for(&request, "decision-other");
    other.tenant_key.clone_from(&other_tenant);
    repository.put(other).await.unwrap();

    assert_eq!(repository.list(&tenant).await.unwrap().len(), 1);
    assert_eq!(repository.list(&other_tenant).await.unwrap().len(), 1);
    assert!(
        repository
            .get(&tenant, "decision-contract")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repository
            .get(&other_tenant, "decision-contract")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repository
            .delete(
                &tenant,
                RecordDelete::Task {
                    decision_ids: vec!["decision-contract".to_owned()],
                    task_key: "task-contract".to_owned(),
                    before_generation: 1,
                },
            )
            .await
            .unwrap(),
        1
    );
    assert!(repository.list(&tenant).await.unwrap().is_empty());
    assert_eq!(repository.list(&other_tenant).await.unwrap().len(), 1);
    assert_eq!(
        repository
            .delete(
                &other_tenant,
                RecordDelete::Decisions(vec!["decision-other".to_owned()]),
            )
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn memory_record_repository_passes_shared_contract() {
    let store = RecordStore::open(None, 4, 1, 0).await.unwrap();
    assert_record_repository_contract(MemoryDecisionRecordRepository::new(store)).await;
}

#[tokio::test]
async fn memory_record_repository_honors_generation_boundary() {
    let store = RecordStore::open(None, 4, 1, 0).await.unwrap();
    let repository = MemoryDecisionRecordRepository::new(store);
    let request = json!({"model": "urouter/auto", "messages": []});
    let mut record = record_for(&request, "decision-generation");
    let tenant = record.tenant_key.clone();
    record.task_key = Some("task-generation".to_owned());
    repository.put(record).await.unwrap();
    assert_eq!(
        repository
            .delete(
                &tenant,
                RecordDelete::Task {
                    decision_ids: vec!["decision-generation".to_owned()],
                    task_key: "task-generation".to_owned(),
                    before_generation: 0,
                },
            )
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn tenant_quota_rejects_at_limit_and_recovers_after_release() {
    let mut state = test_state_with_route(route()).await;
    state.quota = MemoryQuotaRepository::new(1, 0, 0);
    let lease = acquire_tenant_quota(&state, "quota-tenant", 0)
        .await
        .unwrap();
    let Err(error) = acquire_tenant_quota(&state, "quota-tenant", 0).await else {
        panic!("quota reservation should be rejected at the configured limit");
    };
    assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error.code, "tenant_concurrency_exhausted");
    assert_eq!(state.metrics.quota_rejections.load(Ordering::Relaxed), 1);
    lease.release().await.unwrap();
    acquire_tenant_quota(&state, "quota-tenant", 0)
        .await
        .unwrap()
        .release()
        .await
        .unwrap();
}

#[tokio::test]
async fn chat_completions_returns_stable_error_when_tenant_quota_is_exhausted() {
    let mut state = test_state_with_route(route()).await;
    state.quota = MemoryQuotaRepository::new(1, 0, 0);
    let held_lease = acquire_tenant_quota(&state, &tenant_key("local"), 0)
        .await
        .unwrap();
    let app = app_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["type"], "urouter_error");
    assert_eq!(error["error"]["code"], "tenant_concurrency_exhausted");
    held_lease.release().await.unwrap();
}

#[tokio::test]
async fn chat_completions_returns_stable_error_when_budget_is_exhausted() {
    let paid_route: RouteConfig = serde_json::from_value(json!({
        "id": "urouter/auto",
        "tiers": [{
            "tier": "efficient",
            "model": "anthropic/claude-sonnet-4-6"
        }]
    }))
    .unwrap();
    let mut state = test_state_with_route(paid_route).await;
    state.budget = MemoryBudgetRepository::new(1, 2_592_000);
    let app = app_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "hello"}],
                        "max_tokens": 1
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["type"], "urouter_error");
    assert_eq!(error["error"]["code"], "tenant_budget_exhausted");
}

#[tokio::test]
async fn chat_completions_returns_stable_error_when_tenant_rpm_is_exhausted() {
    let mut state = test_state_with_route(route()).await;
    state.quota = MemoryQuotaRepository::new(0, 1, 0);
    acquire_tenant_quota(&state, &tenant_key("local"), 0)
        .await
        .unwrap()
        .release()
        .await
        .unwrap();
    let app = app_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["type"], "urouter_error");
    assert_eq!(error["error"]["code"], "tenant_rate_limit_exhausted");
}

#[test]
fn quota_estimate_reserves_retry_input_and_requested_output() {
    let request = json!({
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 12
    });
    let serialized = u64::try_from(serde_json::to_vec(&request).unwrap().len()).unwrap();
    let expected_input = serialized.div_ceil(4);
    assert_eq!(
        estimate_quota_tokens(&request, 4_096, 1),
        (expected_input, expected_input * 2 + 12)
    );
}

#[tokio::test]
async fn chat_completions_returns_stable_error_when_tenant_tpm_is_exhausted() {
    let mut state = test_state_with_route(route()).await;
    state.quota = MemoryQuotaRepository::new(0, 0, 10);
    acquire_tenant_quota(&state, &tenant_key("local"), 10)
        .await
        .unwrap()
        .release()
        .await
        .unwrap();
    let app = app_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["code"], "tenant_token_limit_exhausted");
}

#[tokio::test]
async fn non_stream_usage_settles_tpm_reservation() {
    async fn completion() -> Json<Value> {
        Json(json!({
            "id": "chatcmpl-quota",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }))
    }

    let upstream = Router::new().route("/chat/completions", post(completion));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let mut route = route();
    set_tier_deployments(
        &mut route,
        "efficient",
        vec![deployment(
            "quota-upstream",
            "local-vllm/qwen3.5-4b",
            format!("http://{address}"),
            0,
        )],
    );
    let quota = MemoryQuotaRepository::new(0, 0, 100);
    let mut state = test_state_with_route(route).await;
    state.retry_policy.max_retries = 0;
    state.quota_default_max_output_tokens = 5;
    state.quota = quota.clone();
    let app = app_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "hello"}],
                        "max_tokens": 5
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let reservation = quota.reserve(&tenant_key("local"), 98).await.unwrap();
    let quota::QuotaReservation::Granted(permit) = reservation else {
        panic!("actual usage should replace the larger TPM reservation");
    };
    quota.release(permit).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn stream_terminal_usage_settles_tpm_reservation() {
    async fn completion() -> Response {
        Response::new(Body::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
             data: [DONE]\n\n",
        ))
    }

    let upstream = Router::new().route("/chat/completions", post(completion));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let mut route = route();
    set_tier_deployments(
        &mut route,
        "efficient",
        vec![deployment(
            "quota-stream-upstream",
            "local-vllm/qwen3.5-4b",
            format!("http://{address}"),
            0,
        )],
    );
    let quota = MemoryQuotaRepository::new(0, 0, 100);
    let mut state = test_state_with_route(route).await;
    state.retry_policy.max_retries = 0;
    state.quota_default_max_output_tokens = 5;
    state.quota = quota.clone();
    let app = app_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "hello"}],
                        "max_tokens": 5,
                        "stream": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 1_048_576).await.unwrap();

    let reservation = quota.reserve(&tenant_key("local"), 98).await.unwrap();
    let quota::QuotaReservation::Granted(permit) = reservation else {
        panic!("terminal stream usage should replace the larger TPM reservation");
    };
    quota.release(permit).await.unwrap();
    server.abort();
}

#[tokio::test]
#[ignore = "requires a Redis service at UROUTER_TEST_REDIS_URL"]
async fn redis_record_repository_passes_shared_contract() {
    let url = std::env::var("UROUTER_TEST_REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:16380/".to_owned());
    let prefix = format!("urouter-record-contract-{}", next_decision_id());
    let shared = RedisSharedState::connect(&url, prefix).await.unwrap();
    let local = RecordStore::open(None, 4, 1, 0).await.unwrap();
    assert_record_repository_contract(RedisDecisionRecordRepository::new(shared, local, 4)).await;
}

#[tokio::test]
async fn retries_on_a_different_deployment_and_opens_failed_primary() {
    async fn primary() -> Response {
        (StatusCode::SERVICE_UNAVAILABLE, "temporary").into_response()
    }
    async fn backup() -> Response {
        Json(json!({"ok": true})).into_response()
    }

    let app = Router::new()
        .route("/primary/chat/completions", post(primary))
        .route("/backup/chat/completions", post(backup));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut route = route();
    set_tier_deployments(
        &mut route,
        "efficient",
        vec![
            deployment(
                "efficient-primary",
                "local-vllm/qwen3.5-4b",
                format!("http://{address}/primary"),
                0,
            ),
            deployment(
                "efficient-backup",
                "local-vllm/qwen3.5-4b",
                format!("http://{address}/backup"),
                1,
            ),
        ],
    );
    let mut state = test_state_with_route(route).await;
    state.quota = MemoryQuotaRepository::new(1, 0, 0);
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    let execution = execute_test_request(&state, &decision, &request, "req-retry")
        .await
        .unwrap();
    assert_eq!(execution.response.status(), StatusCode::OK);
    assert_eq!(execution.attempts.len(), 2);
    assert_eq!(execution.attempts[0].deployment, "efficient-primary");
    assert_eq!(execution.attempts[1].deployment, "efficient-backup");
    assert!(
        execution.attempts[0]
            .selection_trace
            .iter()
            .any(|candidate| {
                candidate.deployment == "efficient-backup"
                    && candidate.reasons == ["lower_priority_order"]
            })
    );
    assert!(
        execution.attempts[1]
            .selection_trace
            .iter()
            .any(|candidate| {
                candidate.deployment == "efficient-primary"
                    && candidate.disposition == DeploymentDisposition::Excluded
                    && candidate
                        .reasons
                        .iter()
                        .any(|reason| reason == "retry_excluded")
            })
    );
    assert_eq!(
        execution.attempts[0].attempt_id.as_deref(),
        Some("req-retry:attempt:1")
    );
    assert_eq!(
        execution.attempts[1].attempt_id.as_deref(),
        Some("req-retry:attempt:2")
    );
    assert!(execution.attempts[0].retry);
    assert_eq!(state.metrics.retries.load(Ordering::Relaxed), 1);
    let health = state.capacity.snapshot();
    assert_eq!(
        health
            .iter()
            .find(|item| item.deployment == "efficient-primary")
            .unwrap()
            .state,
        urouter_gateway::capacity::CircuitState::Open
    );
    server.abort();
}

#[tokio::test]
async fn exhausted_tier_without_attempt_retains_top_level_filter_trace() {
    let mut route = route();
    route.tiers[0].fallbacks.clear();
    route.tiers[0].deployments = vec![
        deployment(
            "cooling-a",
            "local-vllm/qwen3.5-4b",
            "http://127.0.0.1:1/a".to_owned(),
            0,
        ),
        deployment(
            "cooling-b",
            "local-vllm/qwen3.5-4b",
            "http://127.0.0.1:1/b".to_owned(),
            0,
        ),
    ];
    let state = test_state_with_route(route).await;
    let deployments = state.route.tiers[0].effective_deployments();
    state
        .capacity
        .select(&deployments, &BTreeSet::new())
        .unwrap()
        .complete(Err(UpstreamErrorKind::ServerError));
    state
        .capacity
        .select(&deployments, &BTreeSet::new())
        .unwrap()
        .complete(Err(UpstreamErrorKind::ServerError));

    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    let result = execute_test_request(&state, &decision, &request, "req-exhausted").await;
    let Err(failure) = result else {
        panic!("cooling deployments must be exhausted before an upstream attempt");
    };
    assert!(failure.attempts.is_empty());
    assert_eq!(failure.runtime_filter_trace.len(), 2);
    assert!(failure.runtime_filter_trace.iter().all(|candidate| {
        candidate.disposition == DeploymentDisposition::Excluded
            && candidate
                .reasons
                .iter()
                .any(|reason| reason == "local_circuit_unavailable")
    }));
}

#[tokio::test]
async fn deployment_policy_filters_emit_complete_machine_reasons() {
    let mut route = route();
    route.tiers.truncate(1);
    route.tiers[0].fallbacks.clear();
    let mut restricted = deployment(
        "restricted",
        "local-vllm/qwen3.5-4b",
        "http://127.0.0.1:1".to_owned(),
        0,
    );
    restricted.enabled = false;
    restricted.credential_available = false;
    restricted.region = Some("cn-east".to_owned());
    restricted.residency = vec!["cn".to_owned()];
    restricted.tenant_allowlist = vec!["tenant-a".to_owned()];
    route.tiers[0].deployments = vec![restricted];
    let state = test_state_with_route(route).await;
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}],
        "urouter": {"policy": {"region": "us-west", "residency": "eu"}}
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    let result = execute_routed_upstream(
        &state,
        &decision,
        &request,
        "req-policy",
        &tenant_key("tenant-b"),
    )
    .await;
    let Err(failure) = result else {
        panic!("restricted deployment must be filtered before execution");
    };
    assert!(failure.attempts.is_empty());
    assert_eq!(failure.runtime_filter_trace.len(), 1);
    assert_eq!(
        failure.runtime_filter_trace[0].reasons,
        [
            "deployment_disabled",
            "credential_unavailable",
            "region_mismatch",
            "residency_mismatch",
            "tenant_not_allowed"
        ]
    );
    let counters = state.metrics.filter_rejections.lock().unwrap();
    assert_eq!(counters.get("region_mismatch"), Some(&1));
    assert_eq!(counters.get("tenant_not_allowed"), Some(&1));
}

#[tokio::test]
async fn retired_deployment_only_serves_bound_requests_during_grace() {
    let state = test_state_with_route(route()).await;
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "summarize this document"}]
    });
    let mut decision = state.route.decide(&state.catalog, &request).unwrap();
    let mut retired = deployment(
        "retired",
        "local-vllm/qwen3.5-4b",
        "http://127.0.0.1:1".to_owned(),
        0,
    );
    retired.accept_new_requests = false;
    retired.binding_grace_until_unix = Some(unix_seconds() + 60);

    assert!(
        deployment_filter_reasons(&state, &retired, &decision, "local")
            .contains(&"deployment_retired".to_owned())
    );

    decision.reason = "task_binding".to_owned();
    assert!(
        !deployment_filter_reasons(&state, &retired, &decision, "local")
            .contains(&"deployment_retired".to_owned())
    );

    retired.binding_grace_until_unix = Some(unix_seconds().saturating_sub(1));
    assert!(
        deployment_filter_reasons(&state, &retired, &decision, "local")
            .contains(&"deployment_retired".to_owned())
    );
}

#[tokio::test]
async fn falls_back_to_capable_after_efficient_is_exhausted() {
    async fn unavailable() -> Response {
        (StatusCode::SERVICE_UNAVAILABLE, "temporary").into_response()
    }
    async fn capable() -> Response {
        Json(json!({"ok": true})).into_response()
    }

    let app = Router::new()
        .route("/efficient/chat/completions", post(unavailable))
        .route("/capable/chat/completions", post(capable));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut route = route();
    set_tier_deployments(
        &mut route,
        "efficient",
        vec![deployment(
            "efficient-only",
            "local-vllm/qwen3.5-4b",
            format!("http://{address}/efficient"),
            0,
        )],
    );
    set_tier_deployments(
        &mut route,
        "capable",
        vec![deployment(
            "capable-only",
            "local-vllm-qwen38/qwen3.8-27b",
            format!("http://{address}/capable"),
            0,
        )],
    );
    let mut state = test_state_with_route(route).await;
    state.retry_policy.max_retries = 0;
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    let execution = execute_test_request(&state, &decision, &request, "req-fallback")
        .await
        .unwrap();
    assert_eq!(execution.tier, "capable");
    assert_eq!(execution.fallback_depth, 1);
    assert_eq!(execution.attempts.len(), 2);
    assert_eq!(execution.attempts[1].deployment, "capable-only");
    assert_eq!(state.metrics.fallbacks.load(Ordering::Relaxed), 1);
    server.abort();
}

#[tokio::test]
async fn bad_request_does_not_fallback() {
    async fn bad_request() -> Response {
        (StatusCode::BAD_REQUEST, "invalid input").into_response()
    }
    let app = Router::new().route("/efficient/chat/completions", post(bad_request));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut route = route();
    set_tier_deployments(
        &mut route,
        "efficient",
        vec![deployment(
            "bad-request",
            "local-vllm/qwen3.5-4b",
            format!("http://{address}/efficient"),
            0,
        )],
    );
    let state = test_state_with_route(route).await;
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    let failure = execute_test_request(&state, &decision, &request, "req-bad-request")
        .await
        .err()
        .unwrap();
    assert_eq!(failure.error.code, "upstream_bad_request");
    assert_eq!(failure.attempts.len(), 1);
    assert_eq!(state.metrics.fallbacks.load(Ordering::Relaxed), 0);
    server.abort();
}

#[tokio::test]
async fn timeout_uses_its_typed_fallback_chain() {
    async fn slow() -> Response {
        sleep(Duration::from_millis(100)).await;
        Json(json!({"unexpected": true})).into_response()
    }
    async fn generic() -> Response {
        Json(json!({"path": "generic"})).into_response()
    }
    async fn timeout_backup() -> Response {
        Json(json!({"path": "timeout"})).into_response()
    }

    let slow_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_address = slow_listener.local_addr().unwrap();
    let slow_server = tokio::spawn(async move {
        axum::serve(
            slow_listener,
            Router::new().route("/chat/completions", post(slow)),
        )
        .await
        .unwrap();
    });
    let generic_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let generic_address = generic_listener.local_addr().unwrap();
    let generic_server = tokio::spawn(async move {
        axum::serve(
            generic_listener,
            Router::new().route("/chat/completions", post(generic)),
        )
        .await
        .unwrap();
    });
    let timeout_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let timeout_address = timeout_listener.local_addr().unwrap();
    let timeout_server = tokio::spawn(async move {
        axum::serve(
            timeout_listener,
            Router::new().route("/chat/completions", post(timeout_backup)),
        )
        .await
        .unwrap();
    });

    let mut route = route();
    route.tiers[0].deployments = vec![deployment(
        "slow",
        "local-vllm/qwen3.5-4b",
        format!("http://{slow_address}"),
        0,
    )];
    route.tiers[1].deployments = vec![deployment(
        "generic",
        "local-vllm-qwen38/qwen3.8-27b",
        format!("http://{generic_address}"),
        0,
    )];
    let mut timeout_tier = route.tiers[1].clone();
    timeout_tier.tier = "timeout-backup".to_owned();
    timeout_tier.fallbacks.clear();
    timeout_tier.deployments = vec![deployment(
        "timeout-backup",
        "local-vllm-qwen38/qwen3.8-27b",
        format!("http://{timeout_address}"),
        0,
    )];
    route.tiers.push(timeout_tier);
    route.tiers[0]
        .fallbacks_by_error
        .insert(FallbackCause::Timeout, vec!["timeout-backup".to_owned()]);
    let mut state = test_state_with_route(route).await;
    state.request_timeout = Duration::from_millis(25);
    state.retry_policy.max_retries = 0;
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    let execution = execute_test_request(&state, &decision, &request, "req-timeout")
        .await
        .unwrap();
    assert_eq!(execution.tier, "timeout-backup");
    assert_eq!(execution.attempts.len(), 2);
    assert_eq!(
        execution.attempts[0].error_kind,
        Some(UpstreamErrorKind::Timeout)
    );
    assert_eq!(execution.attempts[1].deployment, "timeout-backup");

    slow_server.abort();
    generic_server.abort();
    timeout_server.abort();
}

#[tokio::test]
async fn fallback_depth_counts_edges_from_selected_tier() {
    let mut state = test_state_with_route(route()).await;
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    state.max_fallback_depth = 0;
    let tiers = execution_tiers(&state, &decision).unwrap();
    assert_eq!(tiers.len(), 1);
    assert_eq!(tiers[0].tier, "efficient");

    state.max_fallback_depth = 1;
    let tiers = execution_tiers(&state, &decision).unwrap();
    assert_eq!(tiers.len(), 2);
    assert_eq!(tiers[1].tier, "capable");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dropping_gateway_stream_cancels_upstream_body() {
    struct DropFlag(Arc<AtomicU64>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(1, Ordering::Relaxed);
        }
    }

    async fn slow_stream(State(dropped): State<Arc<AtomicU64>>) -> Response {
        let stream = stream::unfold((DropFlag(dropped), 0_u64), |(guard, index)| async move {
            sleep(Duration::from_millis(10)).await;
            Some((
                Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!(
                    "data: {{\"index\":{index}}}\n\n"
                ))),
                (guard, index.saturating_add(1)),
            ))
        });
        Response::new(Body::from_stream(stream))
    }

    let dropped = Arc::new(AtomicU64::new(0));
    let app = Router::new()
        .route("/chat/completions", post(slow_stream))
        .with_state(Arc::clone(&dropped));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut route = route();
    set_tier_deployments(
        &mut route,
        "efficient",
        vec![deployment(
            "slow-stream",
            "local-vllm/qwen3.5-4b",
            format!("http://{address}"),
            0,
        )],
    );
    let mut state = test_state_with_route(route).await;
    state.quota = MemoryQuotaRepository::new(1, 0, 0);
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "stream"}],
        "stream": true
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    let execution = execute_test_request(&state, &decision, &request, "req-stream")
        .await
        .unwrap();
    let QuotaAdmission::Granted(quota) =
        QuotaLease::acquire(Arc::clone(&state.quota), "stream-tenant", 0)
            .await
            .unwrap()
    else {
        panic!("initial stream quota should be granted");
    };
    let budget =
        test_budget_accounting(&state, "stream-tenant", "req-stream", &decision, &request).await;
    let response = stream_response(
        state.clone(),
        execution,
        ResponseContext {
            decision_id: "decision-cancel".to_owned(),
            decision,
            governance: test_governance(),
            record: RecordSeed {
                request_id: "req-stream".to_owned(),
                messages_hash: messages_hash(&request).unwrap(),
                features: FeatureFrame::from_openai_chat(&request),
                route_revision: state.route.revision(),
                artifact: None,
                exploration: None,
                vector_ref: None,
            },
            headers: HeaderMap::new(),
            quota,
            quota_input_tokens: 0,
            budget,
        },
    );
    drop(response);
    for _ in 0..50 {
        if dropped.load(Ordering::Relaxed) == 1 {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
    let mut replacement = None;
    for _ in 0..50 {
        replacement = match QuotaLease::acquire(Arc::clone(&state.quota), "stream-tenant", 0)
            .await
            .unwrap()
        {
            QuotaAdmission::Granted(lease) => Some(lease),
            QuotaAdmission::Rejected(_) => None,
        };
        if replacement.is_some() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    replacement.unwrap().release().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn partial_stream_failure_never_runs_success_finalization() {
    async fn broken_stream() -> Response {
        let chunks = stream::unfold(0_u8, |step| async move {
            match step {
                0 => Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: {\"delta\":1}\n\n")),
                    1,
                )),
                1 => {
                    sleep(Duration::from_millis(25)).await;
                    Some((Err(std::io::Error::other("stream interrupted")), 2))
                }
                _ => None,
            }
        });
        Response::new(Body::from_stream(chunks))
    }

    let app = Router::new().route("/chat/completions", post(broken_stream));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut route = route();
    route.tiers[0].fallbacks.clear();
    set_tier_deployments(
        &mut route,
        "efficient",
        vec![deployment(
            "broken-stream",
            "local-vllm/qwen3.5-4b",
            format!("http://{address}"),
            0,
        )],
    );
    let mut state = test_state_with_route(route).await;
    state.retry_policy.max_retries = 0;
    state.quota = MemoryQuotaRepository::new(1, 0, 0);
    let request = json!({
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "stream"}],
        "stream": true
    });
    let decision = state.route.decide(&state.catalog, &request).unwrap();
    let execution = execute_test_request(&state, &decision, &request, "req-broken")
        .await
        .unwrap();
    let QuotaAdmission::Granted(quota) =
        QuotaLease::acquire(Arc::clone(&state.quota), "broken-tenant", 0)
            .await
            .unwrap()
    else {
        panic!("stream quota should be granted");
    };
    let budget =
        test_budget_accounting(&state, "broken-tenant", "req-broken", &decision, &request).await;
    let response = stream_response(
        state.clone(),
        execution,
        ResponseContext {
            decision_id: "decision-broken".to_owned(),
            decision,
            governance: test_governance(),
            record: RecordSeed {
                request_id: "req-broken".to_owned(),
                messages_hash: messages_hash(&request).unwrap(),
                features: FeatureFrame::from_openai_chat(&request),
                route_revision: state.route.revision(),
                artifact: None,
                exploration: None,
                vector_ref: None,
            },
            headers: HeaderMap::new(),
            quota,
            quota_input_tokens: 0,
            budget,
        },
    );
    let mut body = response.into_body().into_data_stream();
    while body.next().await.is_some() {}
    assert_eq!(state.metrics.stream_failures.load(Ordering::Relaxed), 1);
    assert_eq!(state.metrics.streams_completed.load(Ordering::Relaxed), 0);
    assert!(state.records.records.read().await.is_empty());
    let replacement = QuotaLease::acquire(Arc::clone(&state.quota), "broken-tenant", 0)
        .await
        .unwrap();
    assert!(matches!(replacement, QuotaAdmission::Granted(_)));
    server.abort();
}
