use std::{
    collections::VecDeque,
    fs,
    path::Path,
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use urouter_ai::catalog::CatalogSnapshot;

use crate::RouteConfig;

const HMAC_BLOCK_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ControlFailurePolicy {
    LastGood,
    FailClosed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlManifest {
    pub schema_version: u16,
    pub revision: String,
    pub catalog_sha256: String,
    pub route_sha256: String,
    #[serde(default)]
    pub signature: Option<String>,
}

impl ControlManifest {
    #[cfg(test)]
    #[must_use]
    pub(crate) fn unsigned(catalog: &[u8], route: &[u8]) -> Self {
        let catalog_sha256 = sha256(catalog);
        let route_sha256 = sha256(route);
        let revision = combined_revision(&catalog_sha256, &route_sha256);
        Self {
            schema_version: 1,
            revision,
            catalog_sha256,
            route_sha256,
            signature: None,
        }
    }

    fn signing_payload(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}",
            self.schema_version, self.revision, self.catalog_sha256, self.route_sha256
        )
    }

    #[cfg(test)]
    pub(crate) fn sign(&mut self, key: &[u8]) {
        self.signature = Some(hmac_sha256(key, self.signing_payload().as_bytes()));
    }

    fn verify(&self, catalog: &[u8], route: &[u8], key: Option<&[u8]>) -> Result<(), ControlError> {
        if self.schema_version != 1 {
            return Err(ControlError::UnsupportedSchema);
        }
        let catalog_sha256 = sha256(catalog);
        let route_sha256 = sha256(route);
        if self.catalog_sha256 != catalog_sha256 || self.route_sha256 != route_sha256 {
            return Err(ControlError::HashMismatch);
        }
        if self.revision != combined_revision(&catalog_sha256, &route_sha256) {
            return Err(ControlError::RevisionMismatch);
        }
        if let Some(key) = key {
            let expected = hmac_sha256(key, self.signing_payload().as_bytes());
            let signature = self
                .signature
                .as_deref()
                .ok_or(ControlError::MissingSignature)?;
            if signature.len() != expected.len()
                || !bool::from(signature.as_bytes().ct_eq(expected.as_bytes()))
            {
                return Err(ControlError::InvalidSignature);
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ControlSnapshot {
    pub catalog: Arc<CatalogSnapshot>,
    pub route: Arc<RouteConfig>,
    pub revision: String,
}

impl ControlSnapshot {
    pub(crate) fn load(
        catalog_path: &Path,
        route_path: &Path,
        manifest_path: &Path,
        signing_key: Option<&[u8]>,
    ) -> Result<Self, ControlError> {
        let catalog_source = fs::read(catalog_path)?;
        let route_source = fs::read(route_path)?;
        let manifest: ControlManifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
        manifest.verify(&catalog_source, &route_source, signing_key)?;
        let catalog = Arc::new(CatalogSnapshot::from_json_str(std::str::from_utf8(
            &catalog_source,
        )?)?);
        let route: RouteConfig = serde_json::from_slice(&route_source)?;
        route.validate(&catalog)?;
        Ok(Self {
            catalog,
            route: Arc::new(route),
            revision: manifest.revision,
        })
    }

    pub(crate) fn from_validated(catalog: Arc<CatalogSnapshot>, route: Arc<RouteConfig>) -> Self {
        let revision = combined_revision(&catalog.hashes().content.to_string(), &route.revision());
        Self {
            catalog,
            route,
            revision,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ControlPlane {
    inner: Arc<RwLock<ControlState>>,
    failure_policy: ControlFailurePolicy,
    required_revision: Option<String>,
}

struct ControlState {
    active: ControlSnapshot,
    history: VecDeque<ControlSnapshot>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ControlStatus {
    pub revision: String,
    pub required_revision: Option<String>,
    pub ready: bool,
    pub degraded: bool,
    pub last_error: Option<String>,
}

impl ControlPlane {
    pub(crate) fn new(
        active: ControlSnapshot,
        failure_policy: ControlFailurePolicy,
        required_revision: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(ControlState {
                active,
                history: VecDeque::new(),
                last_error: None,
            })),
            failure_policy,
            required_revision,
        }
    }

    pub(crate) fn snapshot(&self) -> ControlSnapshot {
        self.inner
            .read()
            .expect("control plane lock poisoned")
            .active
            .clone()
    }

    pub(crate) fn publish(&self, candidate: ControlSnapshot) -> bool {
        let mut state = self.inner.write().expect("control plane lock poisoned");
        if state.active.revision == candidate.revision {
            state.last_error = None;
            return false;
        }
        let previous = std::mem::replace(&mut state.active, candidate);
        state.history.push_front(previous);
        state.history.truncate(2);
        state.last_error = None;
        true
    }

    pub(crate) fn reject(&self, error: impl std::fmt::Display) {
        self.inner
            .write()
            .expect("control plane lock poisoned")
            .last_error = Some(error.to_string());
    }

    pub(crate) fn rollback(&self) -> bool {
        let mut state = self.inner.write().expect("control plane lock poisoned");
        let Some(previous) = state.history.pop_front() else {
            return false;
        };
        let replaced = std::mem::replace(&mut state.active, previous);
        state.history.push_front(replaced);
        state.last_error = None;
        true
    }

    pub(crate) fn status(&self) -> ControlStatus {
        let state = self.inner.read().expect("control plane lock poisoned");
        let revision_matches = self
            .required_revision
            .as_ref()
            .is_none_or(|required| required == &state.active.revision);
        let backend_healthy =
            state.last_error.is_none() || self.failure_policy == ControlFailurePolicy::LastGood;
        ControlStatus {
            revision: state.active.revision.clone(),
            required_revision: self.required_revision.clone(),
            ready: revision_matches && backend_healthy,
            degraded: state.last_error.is_some(),
            last_error: state.last_error.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum ControlError {
    #[error("control manifest schema_version must be 1")]
    UnsupportedSchema,
    #[error("control manifest file hash mismatch")]
    HashMismatch,
    #[error("control manifest revision mismatch")]
    RevisionMismatch,
    #[error("control manifest signature is required")]
    MissingSignature,
    #[error("control manifest signature is invalid")]
    InvalidSignature,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Catalog(#[from] urouter_ai::catalog::CatalogLoadError),
    #[error(transparent)]
    Route(#[from] crate::RouteError),
}

fn combined_revision(catalog_hash: &str, route_hash: &str) -> String {
    sha256(format!("{catalog_hash}\n{route_hash}").as_bytes())
}

fn sha256(value: impl AsRef<[u8]>) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_ref()))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> String {
    let mut normalized = [0_u8; HMAC_BLOCK_BYTES];
    if key.len() > HMAC_BLOCK_BYTES {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; HMAC_BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; HMAC_BLOCK_BYTES];
    for index in 0..HMAC_BLOCK_BYTES {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let inner = Sha256::new()
        .chain_update(inner_pad)
        .chain_update(message)
        .finalize();
    let output = Sha256::new()
        .chain_update(outer_pad)
        .chain_update(inner)
        .finalize();
    format!("hmac-sha256:{output:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_manifest_rejects_tampering() {
        let catalog = b"catalog";
        let route = b"route";
        let mut manifest = ControlManifest::unsigned(catalog, route);
        manifest.sign(b"test-key");
        assert!(manifest.verify(catalog, route, Some(b"test-key")).is_ok());
        assert!(matches!(
            manifest.verify(b"changed", route, Some(b"test-key")),
            Err(ControlError::HashMismatch)
        ));
        assert!(matches!(
            manifest.verify(catalog, route, Some(b"wrong-key")),
            Err(ControlError::InvalidSignature)
        ));
    }

    #[test]
    fn failure_policy_and_required_revision_control_readiness() {
        let catalog = Arc::new(
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap(),
        );
        let route = Arc::new(
            serde_json::from_str::<RouteConfig>(include_str!("../../../gateway/route.json"))
                .unwrap(),
        );
        let snapshot = ControlSnapshot::from_validated(catalog, route);
        let required = snapshot.revision.clone();
        let last_good = ControlPlane::new(
            snapshot.clone(),
            ControlFailurePolicy::LastGood,
            Some(required.clone()),
        );
        last_good.reject("distribution unavailable");
        assert!(last_good.status().ready);
        assert!(last_good.status().degraded);

        let fail_closed =
            ControlPlane::new(snapshot, ControlFailurePolicy::FailClosed, Some(required));
        fail_closed.reject("distribution unavailable");
        assert!(!fail_closed.status().ready);
    }

    #[test]
    fn publish_is_atomic_for_in_flight_snapshot_and_rollback() {
        let catalog = Arc::new(
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap(),
        );
        let route = Arc::new(
            serde_json::from_str::<RouteConfig>(include_str!("../../../gateway/route.json"))
                .unwrap(),
        );
        let first = ControlSnapshot::from_validated(Arc::clone(&catalog), Arc::clone(&route));
        let in_flight = first.clone();
        let mut next_route = (*route).clone();
        next_route.id.push_str("-next");
        let second = ControlSnapshot::from_validated(catalog, Arc::new(next_route));
        assert_ne!(first.revision, second.revision);

        let plane = ControlPlane::new(first, ControlFailurePolicy::LastGood, None);
        assert!(plane.publish(second.clone()));
        let active = plane.snapshot();
        assert_ne!(in_flight.revision, active.revision);
        assert_eq!(active.revision, second.revision);
        assert!(plane.rollback());
        assert_eq!(plane.snapshot().revision, in_flight.revision);
    }
}
