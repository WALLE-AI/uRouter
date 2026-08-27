use serde::{Deserialize, Serialize};
use urouter_types::{ModelId, ProviderId, WireApi};

use crate::{capabilities::Capabilities, catalog::CatalogSnapshot};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProjection {
    pub id: ModelId,
    pub provider: ProviderId,
    pub api: WireApi,
    pub capabilities: Capabilities,
}

#[must_use]
pub fn model_projections(catalog: &CatalogSnapshot) -> Vec<ModelProjection> {
    catalog
        .models()
        .map(|model| ModelProjection {
            id: model.id.clone(),
            provider: model.provider.clone(),
            api: model.api.clone(),
            capabilities: model.capabilities.clone(),
        })
        .collect()
}
