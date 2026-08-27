use std::collections::BTreeMap;

use urouter_ai::{
    catalog::CatalogSnapshot,
    endpoint::EndpointTemplate,
    pricing::{CostRates, CostTier, ModelCost, calculate_actual_cost},
};
use urouter_types::{RateNanoUsdPerMillion, Usage};

struct Generator(u64);

impl Generator {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn range(&mut self, upper: u64) -> u64 {
        self.next() % upper
    }
}

fn rate(value: u64) -> RateNanoUsdPerMillion {
    RateNanoUsdPerMillion::new(i128::from(value)).unwrap()
}

fn rates(input: u64, output: u64, cache_read: u64, cache_write: u64) -> CostRates {
    CostRates {
        input: rate(input),
        output: rate(output),
        cache_read: rate(cache_read),
        cache_write: rate(cache_write),
    }
}

#[test]
fn generated_catalog_inputs_never_panic() {
    let mut generator = Generator(0xCA7A_10C0_F00D_BAAD);
    for _ in 0..5_000 {
        let length = usize::try_from(generator.range(512)).unwrap();
        let input = (0..length)
            .map(|_| char::from(u8::try_from(generator.range(128)).unwrap()))
            .collect::<String>();
        let _ = CatalogSnapshot::from_json_str(&input);
    }
}

#[test]
fn generated_endpoint_templates_never_leave_placeholders() {
    let mut generator = Generator(0xE0D0_9017_7E57_0001);
    let alphabet = b"{}:/._-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    for _ in 0..10_000 {
        let length = usize::try_from(generator.range(96)).unwrap();
        let template = (0..length)
            .map(|_| {
                let index = usize::try_from(generator.range(alphabet.len() as u64)).unwrap();
                char::from(alphabet[index])
            })
            .collect::<String>();
        if let Ok(compiled) = EndpointTemplate::compile(template) {
            let values = compiled
                .variables()
                .iter()
                .map(|variable| (variable.clone(), "value".to_owned()))
                .collect::<BTreeMap<_, _>>();
            if let Ok(url) = compiled.materialize(&values) {
                assert!(!url.as_str().contains('{'));
                assert!(!url.as_str().contains('}'));
            }
        }
    }
}

#[test]
fn generated_costs_are_exact_and_monotonic() {
    let mut generator = Generator(0xC057_B0A0_DA7A_0001);
    for _ in 0..10_000 {
        let base = rates(
            generator.range(10_000_000),
            generator.range(10_000_000),
            generator.range(10_000_000),
            generator.range(10_000_000),
        );
        let tier = CostRates {
            input: rate(
                u64::try_from(base.input.as_nano_usd_per_million()).unwrap()
                    + generator.range(1_000_000),
            ),
            output: rate(
                u64::try_from(base.output.as_nano_usd_per_million()).unwrap()
                    + generator.range(1_000_000),
            ),
            cache_read: rate(
                u64::try_from(base.cache_read.as_nano_usd_per_million()).unwrap()
                    + generator.range(1_000_000),
            ),
            cache_write: rate(
                u64::try_from(base.cache_write.as_nano_usd_per_million()).unwrap()
                    + generator.range(1_000_000),
            ),
        };
        let threshold = generator.range(1_000_000);
        let cost = ModelCost {
            base,
            tiers: vec![CostTier {
                input_tokens_above: threshold,
                rates: tier,
            }],
            long_cache_write: None,
        };
        let input = generator.range(2_000_000);
        let usage = Usage {
            input,
            output: generator.range(100_000),
            cache_read: generator.range(100_000),
            cache_write: generator.range(100_000),
            ..Usage::default()
        };
        let result = calculate_actual_cost(&cost, usage).unwrap();
        let component_total = result
            .input
            .checked_add(result.output)
            .unwrap()
            .checked_add(result.cache_read)
            .unwrap()
            .checked_add(result.cache_write)
            .unwrap();
        assert_eq!(result.total, component_total);

        let next = calculate_actual_cost(
            &cost,
            Usage {
                input: input + 1,
                ..usage
            },
        )
        .unwrap();
        assert!(next.total >= result.total);
    }
}
