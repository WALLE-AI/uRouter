use serde_json::json;
use urouter_ai::CatalogSnapshot;
use urouter_contracts::CapacitySnapshot;
use urouter_core::RouteConfig;
use urouter_embed::{EmbedContext, EmbedRouter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let catalog = CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json"))?;
    let route: RouteConfig = serde_json::from_str(include_str!("../../../gateway/route.json"))?;
    let router = EmbedRouter::new(catalog, route)?;
    let context = EmbedContext {
        tenant_key: "example-tenant".to_owned(),
        task_key: "example-task".to_owned(),
        capacity: CapacitySnapshot::empty(),
    };
    let decision = router.decide(
        &json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "你好"}]
        }),
        &context,
    )?;
    println!(
        "tier={} model={}",
        decision.route.tier, decision.route.model
    );
    Ok(())
}
