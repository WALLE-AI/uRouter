//! Scheduled updates to the model catalog from a signed remote feed.
//!
//! Adapted from `FreeLLMAPI` `server/src/services/catalog-sync.ts`, MIT License,
//! Copyright (c) 2026 Tashfeen Ahmed. The four hard constraints it demonstrates
//! — asymmetric signature over the exact bytes received, a monotonic
//! anti-rollback floor, a support gate before anything is stored, and a
//! last-good fallback — are kept. What is added is the part uRouter cannot give
//! up: a catalog swap changes the control REVISION, and decisions must stay
//! attributable across it.
//!
//! ## Why Ed25519 here and HMAC for the local manifest
//!
//! `control.rs` verifies the operator's own manifest with HMAC-SHA256, which is
//! right for a secret both ends already share. It is wrong for a distribution
//! channel: every install would have to hold the signing key, and any install
//! could therefore forge a feed. The remote feed is verified against a PUBLIC
//! key, so an install can check authenticity without being able to produce it.
//!
//! ## Why this is off by default
//!
//! A feed that lands automatically rewrites which models a tenant's traffic can
//! reach. That is a routing policy change, and taking it silently is not a
//! default anyone should get without asking.

use std::time::Duration;

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use urouter_ai::catalog::{CatalogDocument, CatalogSnapshot};
use urouter_transport::TransportRegistry;

/// The envelope a feed is published in.
///
/// The signature covers `document` as RECEIVED, so it is kept as raw text and
/// only parsed after verification. Re-serialising before checking would let a
/// canonicalisation difference silently break, or worse, silently pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignedCatalogFeed {
    pub schema_version: u16,
    /// The catalog document, verbatim, as a JSON string.
    pub document: String,
    /// Hex-encoded Ed25519 signature over `document`'s bytes.
    pub signature: String,
}

/// The verified payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogFeedDocument {
    pub schema_version: u16,
    /// Human-facing version, for logs and status. Never used for ordering.
    pub catalog_version: String,
    /// Publication time. **This** is the anti-rollback ordering key: a string
    /// version invites a comparison bug, an integer cannot be misread.
    pub published_at_unix: u64,
    pub catalog: CatalogDocument,
}

pub(crate) const FEED_SCHEMA_VERSION: u16 = 1;

/// The floor a feed must beat, in addition to whatever is already live.
///
/// Bump this whenever a newer catalog is bundled into the binary, so a stale
/// snapshot from the feed can never roll back models the release shipped with.
pub(crate) const BUNDLED_CATALOG_PUBLISHED_AT: u64 = 1_756_857_600; // 2025-09-03T00:00:00Z

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum FeedError {
    #[error("catalog feed schema_version must be {FEED_SCHEMA_VERSION}")]
    UnsupportedSchema,
    #[error("catalog feed public key is not a valid Ed25519 key")]
    InvalidPublicKey,
    #[error("catalog feed signature is malformed")]
    MalformedSignature,
    #[error("catalog feed signature does not verify against the pinned key")]
    InvalidSignature,
    #[error("catalog feed document is not valid JSON: {0}")]
    MalformedDocument(String),
    #[error(
        "catalog feed published at {candidate} is not newer than the current {floor}; refusing to roll back"
    )]
    Rollback { candidate: u64, floor: u64 },
    #[error("catalog feed references {0} model(s) with no installed transport")]
    UnsupportedTransports(usize),
    #[error("catalog feed document is not a valid catalog: {0}")]
    InvalidCatalog(String),
}

/// Verify a feed against the pinned public key and decode it.
///
/// Verification happens on the exact bytes received, before any parsing, so a
/// tampered payload is rejected without ever being interpreted.
pub(crate) fn verify_feed(
    feed: &SignedCatalogFeed,
    public_key_hex: &str,
) -> Result<CatalogFeedDocument, FeedError> {
    if feed.schema_version != FEED_SCHEMA_VERSION {
        return Err(FeedError::UnsupportedSchema);
    }
    let key_bytes: [u8; 32] = hex::decode(public_key_hex.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(FeedError::InvalidPublicKey)?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| FeedError::InvalidPublicKey)?;
    let signature_bytes: [u8; 64] = hex::decode(feed.signature.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(FeedError::MalformedSignature)?;
    let signature = Signature::from_bytes(&signature_bytes);
    key.verify_strict(feed.document.as_bytes(), &signature)
        .map_err(|_| FeedError::InvalidSignature)?;

    let document: CatalogFeedDocument = serde_json::from_str(&feed.document)
        .map_err(|error| FeedError::MalformedDocument(error.to_string()))?;
    if document.schema_version != FEED_SCHEMA_VERSION {
        return Err(FeedError::UnsupportedSchema);
    }
    Ok(document)
}

/// Refuse anything not strictly newer than both the bundled floor and what is
/// already live.
///
/// Both bounds are needed. The bundled floor stops a stale feed from undoing a
/// release; the live version stops a replayed older feed from undoing a
/// successful update.
pub(crate) const fn check_rollback(
    candidate_published_at: u64,
    bundled_floor: u64,
    active_published_at: Option<u64>,
) -> Result<(), FeedError> {
    let floor = match active_published_at {
        Some(active) if active > bundled_floor => active,
        _ => bundled_floor,
    };
    if candidate_published_at > floor {
        Ok(())
    } else {
        Err(FeedError::Rollback {
            candidate: candidate_published_at,
            floor,
        })
    }
}

/// Reject a catalog naming a wire format this binary cannot speak.
///
/// The alternative — accepting it and failing on the first request that routes
/// there — turns a publishing mistake into a runtime outage for whoever happens
/// to hit that model first.
pub(crate) fn check_transport_support(
    catalog: &CatalogSnapshot,
    registry: &TransportRegistry,
) -> Result<(), FeedError> {
    let unsupported = registry.unsupported_models(catalog);
    if unsupported.is_empty() {
        Ok(())
    } else {
        Err(FeedError::UnsupportedTransports(unsupported.len()))
    }
}

/// Full acceptance: verify, refuse a rollback, parse, and gate on transports.
///
/// Returns the snapshot to stage. The caller still has to validate the active
/// Route against it and swap atomically — this function deliberately stops
/// short of mutating anything.
pub(crate) fn accept_feed(
    feed: &SignedCatalogFeed,
    public_key_hex: &str,
    bundled_floor: u64,
    active_published_at: Option<u64>,
    registry: &TransportRegistry,
) -> Result<(CatalogFeedDocument, CatalogSnapshot), FeedError> {
    let document = verify_feed(feed, public_key_hex)?;
    check_rollback(document.published_at_unix, bundled_floor, active_published_at)?;
    let snapshot = CatalogSnapshot::from_document(document.catalog.clone())
        .map_err(|error| FeedError::InvalidCatalog(error.to_string()))?;
    check_transport_support(&snapshot, registry)?;
    Ok((document, snapshot))
}

/// How long to wait before the next poll.
///
/// The jitter is not politeness. Without it every install on a given release
/// polls on the same wall-clock boundary, and the feed host sees the whole
/// fleet arrive in one second. The offset is derived from the install's own
/// identity so it is stable across restarts rather than re-randomising into a
/// new thundering herd each boot.
#[must_use]
pub(crate) fn next_poll_delay(interval: Duration, install_id: &str) -> Duration {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(install_id.as_bytes());
    let spread = u64::from(u16::from_be_bytes([digest[0], digest[1]]));
    let interval_secs = interval.as_secs().max(1);
    // Up to 10% of the interval, never more than an hour.
    let max_jitter = (interval_secs / 10).clamp(1, 3_600);
    interval + Duration::from_secs(spread % max_jitter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn catalog_document_json() -> String {
        let catalog = include_str!("../../../catalog/catalog.json");
        let value: serde_json::Value = serde_json::from_str(catalog).unwrap();
        serde_json::to_string(&serde_json::json!({
            "schema_version": FEED_SCHEMA_VERSION,
            "catalog_version": "2026.09.03",
            "published_at_unix": 1_788_393_600_u64,
            "catalog": value
        }))
        .unwrap()
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7_u8; 32])
    }

    fn signed(document: String) -> (SignedCatalogFeed, String) {
        let key = signing_key();
        let signature = key.sign(document.as_bytes());
        (
            SignedCatalogFeed {
                schema_version: FEED_SCHEMA_VERSION,
                document,
                signature: hex::encode(signature.to_bytes()),
            },
            hex::encode(key.verifying_key().to_bytes()),
        )
    }

    #[test]
    fn a_correctly_signed_feed_verifies() {
        let (feed, public_key) = signed(catalog_document_json());
        let document = verify_feed(&feed, &public_key).unwrap();
        assert_eq!(document.catalog_version, "2026.09.03");
    }

    /// The point of signing: a byte changed in transit must be rejected, and
    /// rejected BEFORE the document is interpreted.
    #[test]
    fn a_tampered_document_is_rejected() {
        let (mut feed, public_key) = signed(catalog_document_json());
        feed.document = feed.document.replace("2026.09.03", "2026.09.04");
        assert_eq!(
            verify_feed(&feed, &public_key),
            Err(FeedError::InvalidSignature)
        );
    }

    #[test]
    fn a_feed_signed_by_the_wrong_key_is_rejected() {
        let (feed, _) = signed(catalog_document_json());
        let other = SigningKey::from_bytes(&[9_u8; 32]);
        assert_eq!(
            verify_feed(&feed, &hex::encode(other.verifying_key().to_bytes())),
            Err(FeedError::InvalidSignature)
        );
    }

    #[test]
    fn a_malformed_key_or_signature_is_a_distinct_error() {
        let (mut feed, public_key) = signed(catalog_document_json());
        assert_eq!(
            verify_feed(&feed, "not-hex"),
            Err(FeedError::InvalidPublicKey)
        );
        feed.signature = "abcd".to_owned();
        assert_eq!(
            verify_feed(&feed, &public_key),
            Err(FeedError::MalformedSignature)
        );
    }

    #[test]
    fn an_unknown_envelope_schema_is_refused_before_any_crypto() {
        let (mut feed, public_key) = signed(catalog_document_json());
        feed.schema_version = 99;
        assert_eq!(
            verify_feed(&feed, &public_key),
            Err(FeedError::UnsupportedSchema)
        );
    }

    #[test]
    fn rollback_is_refused_against_both_the_floor_and_the_live_version() {
        // Older than the bundled floor.
        assert!(matches!(
            check_rollback(100, 200, None),
            Err(FeedError::Rollback { .. })
        ));
        // Newer than the floor but not newer than what is live.
        assert!(matches!(
            check_rollback(250, 200, Some(300)),
            Err(FeedError::Rollback { .. })
        ));
        // Equal is not newer: republishing the same version is a no-op, and
        // treating it as an update would churn the revision for nothing.
        assert!(matches!(
            check_rollback(300, 200, Some(300)),
            Err(FeedError::Rollback { .. })
        ));
        // Strictly newer than both.
        assert!(check_rollback(301, 200, Some(300)).is_ok());
        assert!(check_rollback(201, 200, None).is_ok());
    }

    #[test]
    fn the_shipped_catalog_passes_the_transport_gate() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        assert!(check_transport_support(&catalog, &TransportRegistry::with_builtins()).is_ok());
    }

    /// A feed naming a wire format this binary cannot speak must be refused at
    /// load, not discovered by the first request that routes to it.
    #[test]
    fn a_catalog_with_no_installed_transport_is_refused() {
        let catalog =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        // An empty registry stands in for "this binary speaks nothing".
        let error = check_transport_support(&catalog, &TransportRegistry::empty()).unwrap_err();
        assert!(matches!(error, FeedError::UnsupportedTransports(n) if n > 0));
    }

    #[test]
    fn acceptance_runs_every_gate_in_order() {
        let (feed, public_key) = signed(catalog_document_json());
        let registry = TransportRegistry::with_builtins();
        let (document, snapshot) =
            accept_feed(&feed, &public_key, 1_000, None, &registry).unwrap();
        assert_eq!(document.published_at_unix, 1_788_393_600);
        assert!(snapshot.models().count() > 0);

        // The rollback gate fires even though the signature is good.
        assert!(matches!(
            accept_feed(&feed, &public_key, u64::MAX, None, &registry),
            Err(FeedError::Rollback { .. })
        ));
        // The transport gate fires even though signature and version are good.
        assert!(matches!(
            accept_feed(&feed, &public_key, 1_000, None, &TransportRegistry::empty()),
            Err(FeedError::UnsupportedTransports(_))
        ));
    }

    #[test]
    fn poll_jitter_is_bounded_stable_and_spread() {
        let interval = Duration::from_secs(12 * 3_600);
        let a = next_poll_delay(interval, "install-a");
        let b = next_poll_delay(interval, "install-b");
        // Stable for a given install: restarting must not re-roll into a new herd.
        assert_eq!(a, next_poll_delay(interval, "install-a"));
        // Never shorter than the configured interval, never more than 10% over.
        for delay in [a, b] {
            assert!(delay >= interval);
            assert!(delay <= interval + Duration::from_secs(interval.as_secs() / 10));
        }
        // Different installs land at different times.
        assert_ne!(a, b);
    }

    #[test]
    fn a_tiny_interval_still_produces_a_valid_delay() {
        // The jitter divisor must not become zero for short intervals.
        let delay = next_poll_delay(Duration::from_secs(1), "install-a");
        assert!(delay >= Duration::from_secs(1));
    }
}
