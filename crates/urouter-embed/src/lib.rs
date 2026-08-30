use std::sync::Arc;

use serde_json::{Map, Value, json};
use thiserror::Error;
use urouter_ai::CatalogSnapshot;
use urouter_artifact::{ArtifactController, ArtifactDecision, DecisionSource};
use urouter_contracts::FeatureFrame;
use urouter_gateway::{RouteConfig, RouteDecision, RouteError};

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
}

#[derive(Debug, Clone)]
pub struct EmbedDecision {
    pub route: RouteDecision,
    pub artifact: Option<ArtifactDecision>,
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
    use urouter_gateway::SemanticTask;
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
