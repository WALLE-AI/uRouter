//! Model facts, endpoint planning, capability admission, and exact pricing for uRouter.
//!
//! The crate is intentionally free of network and credential I/O. Runtime consumers load a
//! snapshot once, then use its immutable indexes on request paths.
//!
//! ```no_run
//! use urouter_ai::{CatalogSnapshot, admission::eligible_models};
//! use urouter_ai::capabilities::CapabilityRequirement;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let source = std::fs::read_to_string("catalog/catalog.json")?;
//! let catalog = CatalogSnapshot::from_json_str(&source)?;
//! let admitted = eligible_models(&catalog, &CapabilityRequirement::default());
//! assert!(!admitted.eligible.is_empty());
//! # Ok(())
//! # }
//! ```

pub mod admission;
pub mod auth;
pub mod capabilities;
pub mod catalog;
pub mod compat;
pub mod deployment;
pub mod endpoint;
pub mod evidence;
pub mod pricing;
pub mod projection;

pub use catalog::{
    CatalogDocument, CatalogSnapshot, ModelResolution, ModelSpec, ModelVariantKey, ProviderSpec,
};
pub use compat::{
    Compat, MaxTokensField, ParamPolicy, StructuredOutputFormat, ThinkingFormat, ToolCallFormat,
};
