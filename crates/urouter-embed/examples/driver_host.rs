use serde_json::{Value, json};
use thiserror::Error;
use urouter_ai::CatalogSnapshot;
use urouter_contracts::CapacitySnapshot;
use urouter_core::RouteConfig;
use urouter_embed::{Driver, EmbedContext, EmbedRouter, ModelCall};

#[derive(Debug, Error)]
#[error("local example driver failed")]
struct LocalDriverError;

struct LocalDriver;

impl Driver for LocalDriver {
    type Error = LocalDriverError;

    fn call_model(&mut self, call: ModelCall<'_>) -> Result<Value, Self::Error> {
        Ok(json!({
            "object": "chat.completion",
            "model": call.decision.route.model,
            "tier": call.decision.route.tier,
            "capacity_schema": call.capacity.schema_version,
            "echoed_request": call.request
        }))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let catalog = CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json"))?;
    let route: RouteConfig = serde_json::from_str(include_str!("../../../gateway/route.json"))?;
    let router = EmbedRouter::new(catalog, route)?;
    let context = EmbedContext {
        tenant_key: "example-tenant".to_owned(),
        task_key: "example-task".to_owned(),
        capacity: CapacitySnapshot::empty(),
    };
    let run = router.run(
        &json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "求解 x+y=5, x-y=1"}]
        }),
        &context,
        &mut LocalDriver,
    )?;
    println!("{}", serde_json::to_string_pretty(&run.response)?);
    Ok(())
}
