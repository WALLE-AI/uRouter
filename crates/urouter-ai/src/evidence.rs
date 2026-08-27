use serde::{Deserialize, Serialize};
use urouter_types::{CatalogHash, ModelId, ProviderId, WireApi};

use crate::{catalog::CatalogSnapshot, pricing::PriceSource};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEvidence {
    pub schema_version: u16,
    pub content_hash: CatalogHash,
    pub pricing_hash: CatalogHash,
    pub model_id: ModelId,
    pub provider_id: ProviderId,
    pub api: WireApi,
    pub price_source: PriceSource,
}

impl CatalogEvidence {
    #[must_use]
    pub fn from_catalog(
        catalog: &CatalogSnapshot,
        model_id: &ModelId,
        price_source: PriceSource,
    ) -> Option<Self> {
        let model = catalog.model(model_id)?;
        Some(Self {
            schema_version: catalog.schema_version(),
            content_hash: catalog.hashes().content.clone(),
            pricing_hash: catalog.hashes().pricing.clone(),
            model_id: model.id.clone(),
            provider_id: model.provider.clone(),
            api: model.api.clone(),
            price_source,
        })
    }
}
