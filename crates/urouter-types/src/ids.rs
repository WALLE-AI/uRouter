use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    #[error("identifier must not be empty")]
    Empty,
    #[error("identifier must not have leading or trailing whitespace")]
    SurroundingWhitespace,
    #[error("identifier contains a control character")]
    ControlCharacter,
    #[error("catalog hash must contain exactly 64 hexadecimal characters")]
    InvalidHash,
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(IdError::Empty);
                }
                if value.trim() != value {
                    return Err(IdError::SurroundingWhitespace);
                }
                if value.chars().any(char::is_control) {
                    return Err(IdError::ControlCharacter);
                }
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

string_id!(ProviderId);
string_id!(ModelId);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CatalogHash(String);

impl CatalogHash {
    pub fn sha256(hex: impl Into<String>) -> Result<Self, IdError> {
        let hex = hex.into();
        if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(IdError::InvalidHash);
        }
        Ok(Self(format!("sha256:{}", hex.to_ascii_lowercase())))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        let hex = value.strip_prefix("sha256:").ok_or(IdError::InvalidHash)?;
        Self::sha256(hex)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CatalogHash {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<CatalogHash> for String {
    fn from(value: CatalogHash) -> Self {
        value.0
    }
}

impl fmt::Display for CatalogHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ambiguous_ids() {
        assert!(ProviderId::new("").is_err());
        assert!(ProviderId::new(" openai").is_err());
        assert!(ModelId::new("openai/gpt\n").is_err());
    }

    #[test]
    fn id_serde_round_trip() {
        let id = ModelId::new("openai/gpt-test").unwrap();
        let encoded = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<ModelId>(&encoded).unwrap(), id);
    }

    #[test]
    fn catalog_hash_serde_validates_shape() {
        let valid = format!("sha256:{}", "a".repeat(64));
        assert!(serde_json::from_str::<CatalogHash>(&format!("\"{valid}\"")).is_ok());
        assert!(serde_json::from_str::<CatalogHash>("\"sha256:nope\"").is_err());
    }
}
