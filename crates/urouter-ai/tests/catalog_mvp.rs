use std::{collections::BTreeSet, sync::Arc, thread};

use serde::Deserialize;
use urouter_ai::{
    admission::eligible_models,
    capabilities::{CapabilityRequirement, Modality},
    catalog::{CatalogDocument, CatalogManifest, CatalogSnapshot},
    compat::Compat,
    deployment::{DeploymentSpec, validate_deployments},
    endpoint::EndpointPlan,
    evidence::CatalogEvidence,
    pricing::{PriceSource, calculate_actual_cost},
    projection::model_projections,
};
use urouter_types::{ModelId, ProviderId, Usage, WireApi};

fn catalog() -> CatalogSnapshot {
    CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap()
}

#[test]
fn capability_consumer_filters_without_global_intersection() {
    let catalog = catalog();
    let requirement = CapabilityRequirement {
        input_modalities: BTreeSet::from([Modality::Image]),
        tool_calling: true,
        ..CapabilityRequirement::default()
    };
    let result = eligible_models(&catalog, &requirement);
    assert_eq!(result.eligible.len(), 3);
    assert_eq!(result.excluded.len(), 2);
    assert!(
        result
            .excluded
            .iter()
            .any(|item| item.model.as_str() == "local-fixture/efficient")
    );
    assert!(
        result
            .excluded
            .iter()
            .any(|item| item.model.as_str() == "local-vllm/qwen3.5-4b")
    );
}

#[test]
fn deployment_consumer_reuses_catalog_facts() {
    let catalog = catalog();
    let deployments = vec![
        DeploymentSpec {
            id: "balanced-east".to_owned(),
            model: ModelId::new("openai/gpt-5.6-terra").unwrap(),
            provider_override: None,
            rpm: Some(100),
            tpm: Some(1_000_000),
            region: Some("test-east".to_owned()),
            weight: 1,
            cost_override: None,
        },
        DeploymentSpec {
            id: "balanced-west".to_owned(),
            model: ModelId::new("openai/gpt-5.6-terra").unwrap(),
            provider_override: None,
            rpm: Some(100),
            tpm: Some(1_000_000),
            region: Some("test-west".to_owned()),
            weight: 1,
            cost_override: None,
        },
    ];
    assert!(validate_deployments(&catalog, &deployments).is_empty());
}

#[test]
fn decision_record_evidence_pins_catalog_and_price() {
    let catalog = catalog();
    let id = ModelId::new("gpt-5.6-terra").unwrap();
    let evidence = CatalogEvidence::from_catalog(&catalog, &id, PriceSource::Catalog).unwrap();
    assert_eq!(evidence.model_id.as_str(), "openai/gpt-5.6-terra");
    assert_eq!(evidence.content_hash, catalog.hashes().content);
    assert_eq!(evidence.pricing_hash, catalog.hashes().pricing);
}

#[test]
fn endpoint_plan_resolves_custom_provider_variables() {
    let catalog = catalog();
    let id = ModelId::new("local-fixture/efficient").unwrap();
    let model = catalog.model(&id).unwrap();
    let provider = catalog.provider(&model.provider).unwrap();
    let endpoint = EndpointPlan::for_model(provider, model).unwrap();
    assert_eq!(endpoint.url.as_str(), "http://127.0.0.1:8000/v1");
}

#[test]
fn endpoint_plan_resolves_verified_local_vllm() {
    let catalog = catalog();
    let id = ModelId::new("local-vllm/qwen3.5-4b").unwrap();
    let model = catalog.model(&id).unwrap();
    let provider = catalog.provider(&model.provider).unwrap();
    let endpoint = EndpointPlan::for_model(provider, model).unwrap();
    assert_eq!(endpoint.url.as_str(), "http://127.0.0.1:8087/v1");
}

#[test]
fn endpoint_plan_resolves_verified_qwen38_vllm() {
    let catalog = catalog();
    let id = ModelId::new("local-vllm-qwen38/qwen3.8-27b").unwrap();
    let model = catalog.model(&id).unwrap();
    let provider = catalog.provider(&model.provider).unwrap();
    let endpoint = EndpointPlan::for_model(provider, model).unwrap();
    assert_eq!(endpoint.url.as_str(), "http://127.0.0.1:19121/starvlm/v1");
}

#[test]
fn projection_is_stable_and_policy_free() {
    let projections = model_projections(&catalog());
    assert_eq!(projections.len(), 5);
    assert_eq!(projections[0].id.as_str(), "anthropic/claude-sonnet-4-6");
}

#[derive(Debug, Deserialize)]
struct GoldenCase {
    model: ModelId,
    usage: Usage,
    expected_total_nano_usd: i128,
    expected_tier_above: Option<u64>,
}

#[test]
fn pricing_matches_cross_language_golden_vectors() {
    let catalog = catalog();
    let cases: Vec<GoldenCase> = serde_json::from_str(include_str!(
        "../../../catalog/fixtures/pricing-golden.json"
    ))
    .unwrap();
    for case in cases {
        let model = catalog.model(&case.model).unwrap();
        let result = calculate_actual_cost(&model.cost, case.usage).unwrap();
        assert_eq!(
            result.total.as_nano_usd(),
            case.expected_total_nano_usd,
            "{}",
            case.model
        );
        assert_eq!(result.matched_tier_above, case.expected_tier_above);
    }
}

#[test]
fn catalog_hash_is_reproducible_across_input_order() {
    let catalog = catalog();
    let mut document = catalog.document();
    document.providers.reverse();
    document.models.reverse();
    let reordered = CatalogSnapshot::from_document(document).unwrap();
    assert_eq!(catalog.hashes(), reordered.hashes());
}

#[test]
fn resolves_alias_with_provenance_and_variant_key() {
    let catalog = catalog();
    let alias = ModelId::new("qwen3.8-27b").unwrap();
    let resolution = catalog.resolve_model(&alias).unwrap();
    assert_eq!(
        resolution.canonical_id.as_str(),
        "local-vllm-qwen38/qwen3.8-27b"
    );
    assert_eq!(resolution.matched_alias, Some(&alias));

    let provider = ProviderId::new("local-vllm-qwen38").unwrap();
    let variant = catalog
        .model_variant(&provider, "Qwen3.8-27B", &WireApi::OpenAiChat)
        .unwrap();
    assert_eq!(variant.id, *resolution.canonical_id);
}

#[test]
fn duplicate_provider_upstream_api_variant_is_rejected() {
    let catalog = catalog();
    let mut document = catalog.document();
    let mut duplicate = document
        .models
        .iter()
        .find(|model| model.id.as_str() == "local-vllm-qwen38/qwen3.8-27b")
        .unwrap()
        .clone();
    duplicate.id = ModelId::new("local-vllm-qwen38/qwen3.8-27b-copy").unwrap();
    duplicate.aliases.clear();
    document.models.push(duplicate);
    let report = CatalogSnapshot::from_document(document).unwrap_err();
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.code == "duplicate_model_variant")
    );
}

#[test]
fn manifest_detects_source_or_semantic_drift() {
    let source = include_bytes!("../../../catalog/catalog.json");
    let catalog = catalog();
    let manifest = CatalogManifest::from_source(source, &catalog);
    assert!(manifest.matches(source, &catalog));
    assert!(!manifest.matches(b"{}", &catalog));
}

#[test]
fn custom_provider_missing_compat_reports_all_fields() {
    let mut document: CatalogDocument = catalog().document();
    let local = document
        .models
        .iter_mut()
        .find(|model| model.id.as_str() == "local-fixture/efficient")
        .unwrap();
    local.compat = Compat::default();
    let report = CatalogSnapshot::from_document(document).unwrap_err();
    assert_eq!(
        report
            .issues
            .iter()
            .filter(|issue| issue.code == "missing_custom_compat")
            .count(),
        7
    );
}

#[test]
fn cost_is_monotonic_across_long_context_boundary() {
    let catalog = catalog();
    let model = catalog
        .model(&ModelId::new("openai/gpt-5.6-terra").unwrap())
        .unwrap();
    let mut previous = 0;
    for input in [0, 1, 271_999, 272_000, 272_001, 1_000_000] {
        let total = calculate_actual_cost(
            &model.cost,
            Usage {
                input,
                output: 1_000,
                ..Usage::default()
            },
        )
        .unwrap()
        .total
        .as_nano_usd();
        assert!(total >= previous, "cost decreased at {input} tokens");
        previous = total;
    }
}

#[test]
fn snapshot_is_safe_for_concurrent_readers() {
    let catalog = Arc::new(catalog());
    let workers = (0..8)
        .map(|_| {
            let catalog = Arc::clone(&catalog);
            thread::spawn(move || {
                let id = ModelId::new("gpt-5.6-terra").unwrap();
                for _ in 0..10_000 {
                    assert_eq!(
                        catalog.model(&id).unwrap().id.as_str(),
                        "openai/gpt-5.6-terra"
                    );
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
}
