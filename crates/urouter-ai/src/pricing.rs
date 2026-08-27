use serde::{Deserialize, Serialize};
use thiserror::Error;
use urouter_types::{MoneyError, MoneyNanoUsd, RateNanoUsdPerMillion, Usage};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostRates {
    pub input: RateNanoUsdPerMillion,
    pub output: RateNanoUsdPerMillion,
    pub cache_read: RateNanoUsdPerMillion,
    pub cache_write: RateNanoUsdPerMillion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostTier {
    pub input_tokens_above: u64,
    pub rates: CostRates,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongCacheWriteRule {
    pub input_multiplier_millis: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCost {
    pub base: CostRates,
    #[serde(default)]
    pub tiers: Vec<CostTier>,
    pub long_cache_write: Option<LongCacheWriteRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PriceSource {
    Catalog,
    Override { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostBreakdown {
    pub input: MoneyNanoUsd,
    pub output: MoneyNanoUsd,
    pub cache_read: MoneyNanoUsd,
    pub cache_write: MoneyNanoUsd,
    pub total: MoneyNanoUsd,
    pub matched_tier_above: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CounterfactualMethod {
    SameUsageReprice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterfactualCostEstimate {
    pub estimate: MoneyNanoUsd,
    pub method: CounterfactualMethod,
    pub assumptions: Vec<String>,
    pub ci95: Option<(MoneyNanoUsd, MoneyNanoUsd)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PricingError {
    #[error("usage total overflow")]
    UsageOverflow,
    #[error("long cache write tokens exceed all cache write tokens")]
    LongCacheWriteExceedsTotal,
    #[error("cost tiers must be strictly increasing")]
    TiersNotStrictlyIncreasing,
    #[error("long cache multiplier must be greater than zero")]
    InvalidLongCacheMultiplier,
    #[error(transparent)]
    Money(#[from] MoneyError),
}

impl ModelCost {
    pub fn validate(&self) -> Result<(), PricingError> {
        if self
            .tiers
            .windows(2)
            .any(|pair| pair[0].input_tokens_above >= pair[1].input_tokens_above)
        {
            return Err(PricingError::TiersNotStrictlyIncreasing);
        }
        if self
            .long_cache_write
            .as_ref()
            .is_some_and(|rule| rule.input_multiplier_millis == 0)
        {
            return Err(PricingError::InvalidLongCacheMultiplier);
        }
        Ok(())
    }
}

pub fn calculate_actual_cost(
    cost: &ModelCost,
    usage: Usage,
) -> Result<CostBreakdown, PricingError> {
    cost.validate()?;
    if usage.cache_write_long > usage.cache_write {
        return Err(PricingError::LongCacheWriteExceedsTotal);
    }
    let total_input = usage.total_input().ok_or(PricingError::UsageOverflow)?;
    let matched = cost
        .tiers
        .iter()
        .rev()
        .find(|tier| total_input > tier.input_tokens_above);
    let rates = matched.map_or(&cost.base, |tier| &tier.rates);

    let input = rates.input.cost_for_tokens(usage.input)?;
    let output = rates.output.cost_for_tokens(usage.output)?;
    let cache_read = rates.cache_read.cost_for_tokens(usage.cache_read)?;
    let short_write = usage.cache_write - usage.cache_write_long;
    let short_write_cost = rates.cache_write.cost_for_tokens(short_write)?;
    let long_write_rate = match &cost.long_cache_write {
        Some(rule) => rates
            .input
            .checked_mul_millis(rule.input_multiplier_millis)?,
        None => rates.cache_write,
    };
    let long_write_cost = long_write_rate.cost_for_tokens(usage.cache_write_long)?;
    let cache_write = short_write_cost.checked_add(long_write_cost)?;
    let total = input
        .checked_add(output)?
        .checked_add(cache_read)?
        .checked_add(cache_write)?;

    Ok(CostBreakdown {
        input,
        output,
        cache_read,
        cache_write,
        total,
        matched_tier_above: matched.map(|tier| tier.input_tokens_above),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates(input: &str, output: &str, read: &str, write: &str) -> CostRates {
        CostRates {
            input: input.parse().unwrap(),
            output: output.parse().unwrap(),
            cache_read: read.parse().unwrap(),
            cache_write: write.parse().unwrap(),
        }
    }

    #[test]
    fn applies_tier_only_above_threshold() {
        let cost = ModelCost {
            base: rates("1", "2", "0.1", "1.25"),
            tiers: vec![CostTier {
                input_tokens_above: 100,
                rates: rates("4", "8", "0.4", "5"),
            }],
            long_cache_write: None,
        };
        let at = calculate_actual_cost(
            &cost,
            Usage {
                input: 100,
                ..Usage::default()
            },
        )
        .unwrap();
        let above = calculate_actual_cost(
            &cost,
            Usage {
                input: 101,
                ..Usage::default()
            },
        )
        .unwrap();
        assert_eq!(at.matched_tier_above, None);
        assert_eq!(above.matched_tier_above, Some(100));
    }

    #[test]
    fn separates_long_cache_write() {
        let cost = ModelCost {
            base: rates("2", "4", "0.2", "2.5"),
            tiers: vec![],
            long_cache_write: Some(LongCacheWriteRule {
                input_multiplier_millis: 2_000,
            }),
        };
        let result = calculate_actual_cost(
            &cost,
            Usage {
                cache_write: 1_000_000,
                cache_write_long: 500_000,
                ..Usage::default()
            },
        )
        .unwrap();
        assert_eq!(result.cache_write.to_string(), "3.25");
    }
}
