use std::{hint::black_box, time::Instant};

use urouter_ai::{catalog::CatalogSnapshot, pricing::calculate_actual_cost};
use urouter_types::{ModelId, Usage};

const MODEL_COUNT: usize = 10_000;
const OPERATION_COUNT: usize = 1_000_000;

fn main() {
    let seed = CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json"))
        .expect("seed catalog must be valid");
    let mut document = seed.document();
    let template = document
        .models
        .iter()
        .find(|model| model.id.as_str() == "openai/gpt-5.6-terra")
        .expect("priced fixture must exist")
        .clone();
    document.models = (0..MODEL_COUNT)
        .map(|index| {
            let mut model = template.clone();
            model.id = ModelId::new(format!("openai/bench-{index:05}"))
                .expect("generated model id must be valid");
            model.upstream_id = format!("bench-{index:05}");
            model.name = format!("Benchmark model {index:05}");
            model.aliases.clear();
            model
        })
        .collect();

    let source = serde_json::to_string(&document).expect("benchmark catalog must serialize");
    let load_started = Instant::now();
    let catalog = CatalogSnapshot::from_json_str(&source).expect("benchmark catalog must load");
    let load_elapsed = load_started.elapsed();

    let id = ModelId::new("openai/bench-05000").expect("lookup id must be valid");
    let usage = Usage {
        input: 2_000,
        output: 500,
        cache_read: 1_000,
        ..Usage::default()
    };
    let operations_started = Instant::now();
    let mut checksum = 0_i128;
    for _ in 0..OPERATION_COUNT {
        let model = black_box(catalog.model(black_box(&id)).expect("model must exist"));
        checksum = checksum.wrapping_add(
            calculate_actual_cost(&model.cost, black_box(usage))
                .expect("cost must calculate")
                .total
                .as_nano_usd(),
        );
    }
    let operations_elapsed = operations_started.elapsed();

    println!(
        "models={MODEL_COUNT} load_ms={} operations={OPERATION_COUNT} operations_ms={} checksum={checksum}",
        load_elapsed.as_millis(),
        operations_elapsed.as_millis()
    );
}
