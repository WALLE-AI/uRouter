use std::sync::Arc;

use serde_json::{Map, Value, json};
use thiserror::Error;
use urouter_ai::CatalogSnapshot;
use urouter_artifact::{ArtifactController, ArtifactDecision, DecisionSource};
use urouter_contracts::{CapacitySnapshot, FeatureFrame};
use urouter_core::{RouteConfig, RouteDecision, RouteError};

#[derive(Clone)]
pub struct EmbedRouter {
    catalog: Arc<CatalogSnapshot>,
    route: Arc<RouteConfig>,
    artifact: Option<ArtifactController>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedContext {
    pub tenant_key: String,
    pub task_key: String,
    pub capacity: CapacitySnapshot,
}

#[derive(Debug, Clone)]
pub struct EmbedDecision {
    pub route: RouteDecision,
    pub artifact: Option<ArtifactDecision>,
}

pub struct ModelCall<'a> {
    pub request: &'a Value,
    pub decision: &'a EmbedDecision,
    pub capacity: &'a CapacitySnapshot,
}

pub trait Driver {
    type Error: std::error::Error + Send + Sync + 'static;

    fn call_model(&mut self, call: ModelCall<'_>) -> Result<Value, Self::Error>;
}

#[derive(Debug, Clone)]
pub struct EmbedRun {
    pub decision: EmbedDecision,
    pub response: Value,
}

impl EmbedRouter {
    pub fn new(catalog: CatalogSnapshot, route: RouteConfig) -> Result<Self, EmbedError> {
        route.validate(&catalog)?;
        Ok(Self {
            catalog: Arc::new(catalog),
            route: Arc::new(route),
            artifact: None,
        })
    }

    #[must_use]
    pub fn with_artifact(mut self, artifact: ArtifactController) -> Self {
        self.artifact = Some(artifact);
        self
    }

    pub fn decide(
        &self,
        request: &Value,
        context: &EmbedContext,
    ) -> Result<EmbedDecision, EmbedError> {
        if !context.capacity.is_compatible() {
            return Err(EmbedError::UnsupportedCapacitySnapshot(
                context.capacity.schema_version,
            ));
        }
        let baseline = self.route.decide(&self.catalog, request)?;
        let Some(controller) = &self.artifact else {
            return Ok(EmbedDecision {
                route: baseline,
                artifact: None,
            });
        };
        let artifact = controller.decide(
            &FeatureFrame::from_openai_chat(request),
            semantic_task_name(&baseline),
            &context.tenant_key,
            &context.task_key,
            &baseline.tier,
        );
        let route =
            if artifact.source == DecisionSource::Rule || artifact.applied_tier == baseline.tier {
                baseline
            } else {
                let pinned = pin_tier(request, &artifact.applied_tier)?;
                self.route.decide(&self.catalog, &pinned)?
            };
        Ok(EmbedDecision {
            route,
            artifact: Some(artifact),
        })
    }

    pub fn run<D: Driver>(
        &self,
        request: &Value,
        context: &EmbedContext,
        driver: &mut D,
    ) -> Result<EmbedRun, EmbedRunError<D::Error>> {
        let decision = self.decide(request, context)?;
        let response = driver
            .call_model(ModelCall {
                request,
                decision: &decision,
                capacity: &context.capacity,
            })
            .map_err(EmbedRunError::Driver)?;
        Ok(EmbedRun { decision, response })
    }

    #[must_use]
    pub fn catalog(&self) -> &CatalogSnapshot {
        &self.catalog
    }

    #[must_use]
    pub fn route(&self) -> &RouteConfig {
        &self.route
    }
}

fn pin_tier(request: &Value, tier: &str) -> Result<Value, EmbedError> {
    let mut request = request.clone();
    let object = request
        .as_object_mut()
        .ok_or(EmbedError::RequestMustBeObject)?;
    let urouter = object
        .entry("urouter")
        .or_insert_with(|| Value::Object(Map::new()));
    let urouter = urouter
        .as_object_mut()
        .ok_or(EmbedError::UrouterMustBeObject)?;
    let preference = urouter
        .entry("preference")
        .or_insert_with(|| Value::Object(Map::new()));
    let preference = preference
        .as_object_mut()
        .ok_or(EmbedError::PreferenceMustBeObject)?;
    preference.insert("pin_tier".to_owned(), json!(tier));
    Ok(request)
}

fn semantic_task_name(decision: &RouteDecision) -> &'static str {
    use urouter_core::SemanticTask;
    match decision.semantic.task {
        SemanticTask::Greeting => "greeting",
        SemanticTask::RealtimeWeather => "realtime_weather",
        SemanticTask::EquationSolving => "equation_solving",
        SemanticTask::General => "general",
    }
}

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error(transparent)]
    Route(#[from] RouteError),
    #[error("request must be a JSON object")]
    RequestMustBeObject,
    #[error("urouter must be a JSON object")]
    UrouterMustBeObject,
    #[error("urouter.preference must be a JSON object")]
    PreferenceMustBeObject,
    #[error("unsupported capacity snapshot schema version: {0}")]
    UnsupportedCapacitySnapshot(u16),
}

#[derive(Debug, Error)]
pub enum EmbedRunError<E: std::error::Error + 'static> {
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error("model driver failed: {0}")]
    Driver(E),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use urouter_artifact::{
        ArtifactSupportDomain, ExportGates, FeatureWeights, LinearPolicy, RolloutPolicy,
        RouterArtifact,
    };

    fn inputs() -> Vec<Value> {
        vec![
            json!({"model": "urouter/auto", "messages": [{"role": "user", "content": "hello"}]}),
            json!({"model": "urouter/auto", "messages": [{"role": "user", "content": "x+y=4; x-y=2"}]}),
            json!({"model": "urouter/auto", "messages": [{"role": "user", "content": "summarize this"}]}),
            json!({
                "model": "urouter/auto",
                "messages": [{"role": "user", "content": "plan a migration"}],
                "urouter": {"hint": {"difficulty": "hard"}}
            }),
            // An overstated output bound. Admission caps what this reserves
            // against the context window, and the cap has to apply identically
            // on both sides or the two routers disagree about which models are
            // eligible for the same request.
            json!({
                "model": "urouter/auto",
                "max_tokens": 32_000,
                "messages": [{"role": "user", "content": "hello"}]
            }),
        ]
    }

    #[test]
    fn embed_and_gateway_core_have_full_parity_on_shared_corpus() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let route: RouteConfig =
            serde_json::from_str(include_str!("../../../gateway/route.json")).unwrap();
        let embed = EmbedRouter::new(catalog, route.clone()).unwrap();
        let context = EmbedContext {
            tenant_key: "tenant-a".to_owned(),
            task_key: "task-a".to_owned(),
            capacity: CapacitySnapshot::empty(),
        };
        for request in inputs() {
            let gateway = route.decide(embed.catalog(), &request).unwrap();
            let embedded = embed.decide(&request, &context).unwrap();
            assert_eq!(embedded.route.tier, gateway.tier);
            assert_eq!(embedded.route.model, gateway.model);
            assert_eq!(embedded.route.reason, gateway.reason);
            assert_eq!(embedded.route.admission, gateway.admission);
        }
    }

    #[test]
    fn embed_rejects_an_unknown_capacity_snapshot_schema() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let route: RouteConfig =
            serde_json::from_str(include_str!("../../../gateway/route.json")).unwrap();
        let embed = EmbedRouter::new(catalog, route).unwrap();
        let context = EmbedContext {
            tenant_key: "tenant-a".to_owned(),
            task_key: "task-a".to_owned(),
            capacity: CapacitySnapshot {
                schema_version: 999,
                candidates: Vec::new(),
            },
        };
        let error = embed
            .decide(&json!({"model": "urouter/auto", "messages": []}), &context)
            .unwrap_err();
        assert!(matches!(
            error,
            EmbedError::UnsupportedCapacitySnapshot(999)
        ));
    }

    #[derive(Debug, Error)]
    #[error("test driver error")]
    struct TestDriverError;

    struct TestDriver;

    impl Driver for TestDriver {
        type Error = TestDriverError;

        fn call_model(&mut self, call: ModelCall<'_>) -> Result<Value, Self::Error> {
            Ok(json!({
                "tier": call.decision.route.tier,
                "model": call.decision.route.model,
                "capacity_schema": call.capacity.schema_version
            }))
        }
    }

    #[test]
    fn host_driver_receives_the_decision_and_same_capacity_snapshot() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let route: RouteConfig =
            serde_json::from_str(include_str!("../../../gateway/route.json")).unwrap();
        let embed = EmbedRouter::new(catalog, route).unwrap();
        let context = EmbedContext {
            tenant_key: "tenant-a".to_owned(),
            task_key: "task-a".to_owned(),
            capacity: CapacitySnapshot::empty(),
        };
        let run = embed
            .run(
                &json!({"model": "urouter/auto", "messages": [{"role": "user", "content": "hello"}]}),
                &context,
                &mut TestDriver,
            )
            .unwrap();
        assert_eq!(run.decision.route.tier, "efficient");
        assert_eq!(run.response["tier"], "efficient");
        assert_eq!(run.response["capacity_schema"], 1);
    }

    #[test]
    fn embed_matches_gateway_core_with_the_same_router_artifact() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let route: RouteConfig =
            serde_json::from_str(include_str!("../../../gateway/route.json")).unwrap();
        let artifact = RouterArtifact::build(
            urouter_contracts::FEATURE_SCHEMA_VERSION,
            catalog.hashes().content.to_string(),
            route.revision(),
            "dataset:test",
            42,
            100,
            LinearPolicy {
                baseline_tier: "efficient".to_owned(),
                promoted_tier: "capable".to_owned(),
                threshold_millis: 0,
                bias_millis: 1,
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
                maximum_input_text_bytes: 1_024,
                tools_supported: false,
            },
            ExportGates {
                reproducible: true,
                privacy_passed: true,
                support_domain_defined: true,
                counterfactual_passed: true,
                quality_lower_bound_millionths: 0,
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
                minimum_samples: 0,
                operation_limit: 6,
            },
        );
        let request =
            json!({"model": "urouter/auto", "messages": [{"role": "user", "content": "你好"}]});
        let baseline = route.decide(&catalog, &request).unwrap();
        let gateway_artifact = controller.decide(
            &FeatureFrame::from_openai_chat(&request),
            "greeting",
            "tenant-a",
            "task-a",
            &baseline.tier,
        );
        let mut gateway_request = request.clone();
        gateway_request["urouter"] =
            json!({"preference": {"pin_tier": gateway_artifact.applied_tier}});
        let gateway = route.decide(&catalog, &gateway_request).unwrap();

        let embed = EmbedRouter::new(catalog, route)
            .unwrap()
            .with_artifact(controller);
        let embedded = embed
            .decide(
                &request,
                &EmbedContext {
                    tenant_key: "tenant-a".to_owned(),
                    task_key: "task-a".to_owned(),
                    capacity: CapacitySnapshot::empty(),
                },
            )
            .unwrap();
        assert_eq!(embedded.route.tier, gateway.tier);
        assert_eq!(embedded.route.model, gateway.model);
        assert_eq!(embedded.artifact.unwrap(), gateway_artifact);
    }
}
