use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

const NANO_PER_USD: i128 = 1_000_000_000;
const TOKENS_PER_MILLION: i128 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MoneyError {
    #[error("money value must be non-negative")]
    Negative,
    #[error("invalid USD decimal value: {0}")]
    InvalidDecimal(String),
    #[error("money arithmetic overflow")]
    Overflow,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoneyNanoUsd(i128);

impl MoneyNanoUsd {
    pub const ZERO: Self = Self(0);

    pub fn new(value: i128) -> Result<Self, MoneyError> {
        if value < 0 {
            return Err(MoneyError::Negative);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn as_nano_usd(self) -> i128 {
        self.0
    }

    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        self.0
            .checked_add(other.0)
            .ok_or(MoneyError::Overflow)
            .and_then(Self::new)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RateNanoUsdPerMillion(i128);

impl RateNanoUsdPerMillion {
    pub fn new(value: i128) -> Result<Self, MoneyError> {
        if value < 0 {
            return Err(MoneyError::Negative);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn as_nano_usd_per_million(self) -> i128 {
        self.0
    }

    pub fn cost_for_tokens(self, tokens: u64) -> Result<MoneyNanoUsd, MoneyError> {
        let product = self
            .0
            .checked_mul(i128::from(tokens))
            .ok_or(MoneyError::Overflow)?;
        let rounded = product
            .checked_add(TOKENS_PER_MILLION / 2)
            .ok_or(MoneyError::Overflow)?
            / TOKENS_PER_MILLION;
        MoneyNanoUsd::new(rounded)
    }

    pub fn checked_mul_millis(self, multiplier_millis: u32) -> Result<Self, MoneyError> {
        let product = self
            .0
            .checked_mul(i128::from(multiplier_millis))
            .ok_or(MoneyError::Overflow)?;
        Self::new(product / 1_000)
    }
}

fn parse_usd_decimal(value: &str) -> Result<i128, MoneyError> {
    if value.starts_with('-') {
        return Err(MoneyError::Negative);
    }
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
        || fractional.len() > 9
    {
        return Err(MoneyError::InvalidDecimal(value.to_owned()));
    }
    let whole = whole
        .parse::<i128>()
        .map_err(|_| MoneyError::InvalidDecimal(value.to_owned()))?;
    let mut fraction = fractional.to_owned();
    fraction.push_str(&"0".repeat(9 - fraction.len()));
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i128>()
            .map_err(|_| MoneyError::InvalidDecimal(value.to_owned()))?
    };
    whole
        .checked_mul(NANO_PER_USD)
        .and_then(|base| base.checked_add(fraction))
        .ok_or(MoneyError::Overflow)
}

fn format_usd_decimal(nano_usd: i128) -> String {
    let whole = nano_usd / NANO_PER_USD;
    let fraction = nano_usd % NANO_PER_USD;
    if fraction == 0 {
        return whole.to_string();
    }
    let mut result = format!("{whole}.{fraction:09}");
    while result.ends_with('0') {
        result.pop();
    }
    result
}

macro_rules! impl_money_serde {
    ($type:ty, $getter:expr, $constructor:expr) => {
        impl FromStr for $type {
            type Err = MoneyError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                $constructor(parse_usd_decimal(value)?)
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&format_usd_decimal($getter(*self)))
            }
        }

        impl Serialize for $type {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(D::Error::custom)
            }
        }
    };
}

impl_money_serde!(
    MoneyNanoUsd,
    |value: MoneyNanoUsd| value.0,
    MoneyNanoUsd::new
);
impl_money_serde!(
    RateNanoUsdPerMillion,
    |value: RateNanoUsdPerMillion| value.0,
    RateNanoUsdPerMillion::new
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_round_trip_is_exact() {
        let rate: RateNanoUsdPerMillion = "3.125000001".parse().unwrap();
        assert_eq!(rate.to_string(), "3.125000001");
        assert_eq!(serde_json::to_string(&rate).unwrap(), "\"3.125000001\"");
    }

    #[test]
    fn token_cost_rounds_half_up() {
        let rate: RateNanoUsdPerMillion = "1".parse().unwrap();
        assert_eq!(rate.cost_for_tokens(1).unwrap().as_nano_usd(), 1_000);
    }

    #[test]
    fn rejects_negative_and_excess_precision() {
        assert!("-1".parse::<MoneyNanoUsd>().is_err());
        assert!("0.0000000001".parse::<MoneyNanoUsd>().is_err());
    }
}
