//! Turning a pile of per-candidate exclusion reasons into something a client
//! can act on.
//!
//! Adapted from `FreeLLMAPI` `server/src/services/router.ts` (`summarizeExhaustion`),
//! MIT License, Copyright (c) 2026 Tashfeen Ahmed.
//!
//! ## Why the reasons stay strings
//!
//! Exclusion reasons are `Vec<String>` throughout the workspace, and they are
//! serialised into `RoutingTrace.candidates[].reasons` inside a persisted
//! `DecisionRecord`. Migrating them to a closed enum would be a wire-format
//! break for every stored record, and several reasons are parameterised by
//! construction (`missing_modality:image`, `shared_provider_circuit_unavailable`)
//! so they could not be represented by a closed enum anyway.
//!
//! Classifying `&str` into a typed bucket gets the benefits — exhaustive
//! matching, a compile-checked status mapping — at zero wire cost. The strings
//! remain the wire format; the enum is the interpretation.

use core::fmt::Write as _;

use serde::{Deserialize, Serialize};

pub const EXHAUSTION_SCHEMA_VERSION: u16 = 1;

/// Why a candidate could not serve, coarse enough to be worth counting.
///
/// **Declaration order is actionability order.** It decides which bucket a
/// candidate with several reasons is attributed to, how ties are broken when
/// ranking, and which HTTP status the client sees. Reordering these variants
/// changes client-visible behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionBucket {
    /// The request asks for something no candidate can do. Only the caller can
    /// fix this, so it ranks first: retrying will never help.
    Capability,
    /// No usable credential is configured. An operator fix, not a client one,
    /// but still not something waiting will resolve.
    CredentialMissing,
    /// A tenant, region or residency policy excluded the candidate.
    PolicyExcluded,
    /// Turned off, retired, or not in the catalog as an active model.
    Disabled,
    /// Rate-limited, cooling down, or out of quota. The one bucket where
    /// waiting is the correct response.
    RateLimitedOrCooling,
    /// Available, but a picker or a priority order preferred someone else.
    /// Never a reason a request failed on its own.
    Deprioritized,
    /// Unrecognised. A reason landing here means the classifier is out of date;
    /// `classifier_covers_every_workspace_reason` is the guard.
    Unknown,
}

/// Bucket a single reason string.
///
/// Matching is on prefixes and substrings rather than equality because several
/// reasons are built with `format!` and carry a payload.
#[must_use]
pub fn classify_exclusion(reason: &str) -> ExclusionBucket {
    // Longest / most specific tests first: `credential_unavailable` must not be
    // caught by the generic `*_unavailable` circuit test below.
    if reason.starts_with("missing_modality")
        || reason.ends_with("_unsupported")
        || reason == "context_window_too_small"
    {
        return ExclusionBucket::Capability;
    }
    if reason == "credential_unavailable" || reason == "endpoint_unavailable" {
        return ExclusionBucket::CredentialMissing;
    }
    if reason == "region_mismatch"
        || reason == "residency_mismatch"
        || reason == "tenant_not_allowed"
    {
        return ExclusionBucket::PolicyExcluded;
    }
    if reason == "deployment_disabled"
        || reason == "deployment_retired"
        || reason == "model_not_active"
        || reason == "model_ineligible"
        || reason == "model_unavailable"
        || reason == "not_accepting_requests"
    {
        return ExclusionBucket::Disabled;
    }
    if reason.starts_with("quota_")
        || reason.ends_with("circuit_unavailable")
        || reason == "retry_excluded"
        || reason == "cooldown"
    {
        return ExclusionBucket::RateLimitedOrCooling;
    }
    if reason == "lower_priority_order"
        || reason == "higher_load"
        || reason == "higher_latency"
        || reason == "higher_quota_usage"
        || reason == "zero_weight"
    {
        return ExclusionBucket::Deprioritized;
    }
    ExclusionBucket::Unknown
}

/// One candidate and everything that ruled it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateExclusion {
    /// Deployment or model id. Used for nothing but de-duplication — it is
    /// never carried into the summary. See the type-level note on
    /// [`ExhaustionSummary`].
    pub candidate: String,
    pub reasons: Vec<String>,
    /// When this candidate becomes available again, in milliseconds from now.
    pub reset_millis: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExhaustionBucketCount {
    pub bucket: ExclusionBucket,
    pub candidates: u32,
}

/// An aggregate, client-safe account of why nothing could serve.
///
/// This type is **structurally incapable of carrying an identifier**: counts and
/// one ETA, nothing else. That is deliberate and load-bearing. The per-candidate
/// detail — deployment ids, credential scopes, provider names, regions — stays
/// in `/v1/explain` and the `DecisionRecord`, which are tenant-authenticated and
/// governed by the retention policy. A summary shaped so that a future refactor
/// *could* append `(gpt-4o-us-east-key-3)` to the message is a summary that
/// eventually will.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExhaustionSummary {
    pub schema_version: u16,
    /// How many candidates were considered in total.
    pub checked: u32,
    /// Descending by count; ties broken by [`ExclusionBucket`] order.
    pub buckets: Vec<ExhaustionBucketCount>,
    /// Soonest moment any excluded candidate becomes available again.
    pub soonest_reset_millis: Option<u64>,
}

impl ExhaustionSummary {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: EXHAUSTION_SCHEMA_VERSION,
            checked: 0,
            buckets: Vec::new(),
            soonest_reset_millis: None,
        }
    }

    /// The bucket accounting for the most candidates, which drives the status
    /// code and the client-facing advice.
    #[must_use]
    pub fn dominant(&self) -> Option<ExclusionBucket> {
        self.buckets.first().map(|entry| entry.bucket)
    }

    /// A one-line, counts-only explanation.
    #[must_use]
    pub fn message(&self) -> String {
        if self.checked == 0 {
            return "no candidate models were configured for this request".to_owned();
        }
        let parts: Vec<String> = self
            .buckets
            .iter()
            .map(|entry| format!("{} {}", entry.candidates, describe(entry.bucket)))
            .collect();
        let plural = if self.checked == 1 { "" } else { "s" };
        let mut message = format!(
            "no eligible model: {} candidate{plural} checked ({})",
            self.checked,
            parts.join(", ")
        );
        if let Some(advice) = advice(self.dominant()) {
            message.push_str(". ");
            message.push_str(advice);
        }
        if let Some(reset) = self.soonest_reset_millis {
            let _ = write!(message, " Soonest reset {}.", format_eta(reset));
        }
        message
    }
}

const fn describe(bucket: ExclusionBucket) -> &'static str {
    match bucket {
        ExclusionBucket::Capability => "lack a required capability",
        ExclusionBucket::CredentialMissing => "have no usable credential",
        ExclusionBucket::PolicyExcluded => "excluded by tenant or residency policy",
        ExclusionBucket::Disabled => "disabled or retired",
        ExclusionBucket::RateLimitedOrCooling => "rate-limited or cooling down",
        ExclusionBucket::Deprioritized => "deprioritised",
        ExclusionBucket::Unknown => "unavailable",
    }
}

const fn advice(bucket: Option<ExclusionBucket>) -> Option<&'static str> {
    match bucket {
        Some(ExclusionBucket::Capability) => {
            Some("Relax the request's capability requirements or add a model that meets them.")
        }
        Some(ExclusionBucket::CredentialMissing) => {
            Some("Configure a credential for at least one deployment.")
        }
        Some(ExclusionBucket::PolicyExcluded) => {
            Some("Widen the tenant, region or residency policy.")
        }
        Some(ExclusionBucket::Disabled) => Some("Enable a deployment or add a replacement model."),
        Some(ExclusionBucket::RateLimitedOrCooling) => {
            Some("Add capacity or wait for the limits to reset.")
        }
        _ => None,
    }
}

/// Human-readable ETA, coarse on purpose: a precise millisecond count invites
/// a client to poll on exactly that boundary.
fn format_eta(millis: u64) -> String {
    let seconds = millis.div_ceil(1_000);
    if seconds < 90 {
        return format!("~{seconds}s");
    }
    let minutes = seconds.div_ceil(60);
    if minutes < 90 {
        return format!("~{minutes}m");
    }
    format!("~{}h", minutes.div_ceil(60))
}

/// Roll per-candidate exclusions into the aggregate summary.
///
/// A candidate carrying several reasons is attributed to the **most actionable**
/// one it carries (the minimum by [`ExclusionBucket`] order), so a model that is
/// both rate-limited and lacks vision is reported as a capability problem —
/// waiting for the rate limit would not have helped it.
///
/// The invariant `checked == sum(bucket.candidates)` follows from each candidate
/// landing in exactly one bucket, and is property-tested below.
#[must_use]
pub fn summarize_exhaustion(exclusions: &[CandidateExclusion]) -> ExhaustionSummary {
    let mut counts: Vec<u32> = vec![0; 7];
    let mut soonest: Option<u64> = None;

    for exclusion in exclusions {
        let bucket = exclusion
            .reasons
            .iter()
            .map(|reason| classify_exclusion(reason))
            .min()
            .unwrap_or(ExclusionBucket::Unknown);
        counts[bucket as usize] = counts[bucket as usize].saturating_add(1);
        if let Some(reset) = exclusion.reset_millis {
            soonest = Some(soonest.map_or(reset, |current: u64| current.min(reset)));
        }
    }

    let mut buckets: Vec<ExhaustionBucketCount> = ALL_BUCKETS
        .iter()
        .enumerate()
        .filter(|(index, _)| counts[*index] > 0)
        .map(|(index, bucket)| ExhaustionBucketCount {
            bucket: *bucket,
            candidates: counts[index],
        })
        .collect();
    // Descending count; ties fall back to declaration order, which is already
    // the order `ALL_BUCKETS` iterates, so the sort must be stable (it is).
    buckets.sort_by_key(|entry| core::cmp::Reverse(entry.candidates));

    ExhaustionSummary {
        schema_version: EXHAUSTION_SCHEMA_VERSION,
        checked: u32::try_from(exclusions.len()).unwrap_or(u32::MAX),
        buckets,
        soonest_reset_millis: soonest,
    }
}

const ALL_BUCKETS: [ExclusionBucket; 7] = [
    ExclusionBucket::Capability,
    ExclusionBucket::CredentialMissing,
    ExclusionBucket::PolicyExcluded,
    ExclusionBucket::Disabled,
    ExclusionBucket::RateLimitedOrCooling,
    ExclusionBucket::Deprioritized,
    ExclusionBucket::Unknown,
];

/// What the client should be told to do about an exhaustion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExhaustionDisposition {
    /// The request itself must change. A 4xx.
    ClientCapability,
    /// Nothing is wrong with the request; come back later. A 429.
    RetryLater,
    /// The deployment is misconfigured or down. A 5xx.
    Unavailable,
}

#[must_use]
pub const fn exhaustion_disposition(bucket: ExclusionBucket) -> ExhaustionDisposition {
    match bucket {
        ExclusionBucket::Capability | ExclusionBucket::PolicyExcluded => {
            ExhaustionDisposition::ClientCapability
        }
        ExclusionBucket::RateLimitedOrCooling => ExhaustionDisposition::RetryLater,
        ExclusionBucket::CredentialMissing
        | ExclusionBucket::Disabled
        | ExclusionBucket::Deprioritized
        | ExclusionBucket::Unknown => ExhaustionDisposition::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn excluded(candidate: &str, reasons: &[&str]) -> CandidateExclusion {
        CandidateExclusion {
            candidate: candidate.to_owned(),
            reasons: reasons.iter().map(|reason| (*reason).to_owned()).collect(),
            reset_millis: None,
        }
    }

    /// Every reason string produced anywhere in the workspace must classify.
    ///
    /// This list is maintained by hand against the producers in
    /// `urouter-ai::admission::exclusion_reasons`,
    /// `urouter-gateway::deployment_filter_reasons`, and the picker reasons in
    /// `urouter-contracts::capacity`. Adding a reason without adding a bucket
    /// for it fails here rather than silently becoming `Unknown` in a client's
    /// error body.
    #[test]
    fn classifier_covers_every_workspace_reason() {
        let inventory = [
            // urouter-ai::admission
            "model_not_active",
            "context_window_too_small",
            "missing_modality:image",
            "missing_modality:audio",
            "tool_calling_unsupported",
            "structured_output_unsupported",
            "reasoning_unsupported",
            // urouter-gateway::deployment_filter_reasons
            "deployment_disabled",
            "credential_unavailable",
            "endpoint_unavailable",
            "deployment_retired",
            "model_ineligible",
            "model_unavailable",
            "region_mismatch",
            "residency_mismatch",
            "tenant_not_allowed",
            // capacity / circuit
            "retry_excluded",
            "local_circuit_unavailable",
            "shared_circuit_unavailable",
            "shared_provider_circuit_unavailable",
            "lower_priority_order",
            "higher_load",
            "higher_latency",
            "higher_quota_usage",
            "zero_weight",
        ];
        for reason in inventory {
            assert_ne!(
                classify_exclusion(reason),
                ExclusionBucket::Unknown,
                "{reason} has no bucket"
            );
        }
    }

    #[test]
    fn credential_unavailable_is_not_swallowed_by_the_circuit_rule() {
        // Both end in `_unavailable`; only one of them means "wait".
        assert_eq!(
            classify_exclusion("credential_unavailable"),
            ExclusionBucket::CredentialMissing
        );
        assert_eq!(
            classify_exclusion("shared_provider_circuit_unavailable"),
            ExclusionBucket::RateLimitedOrCooling
        );
    }

    #[test]
    fn every_candidate_lands_in_exactly_one_bucket() {
        let exclusions = vec![
            excluded("a", &["context_window_too_small"]),
            excluded("b", &["credential_unavailable"]),
            excluded("c", &["local_circuit_unavailable"]),
            excluded("d", &["shared_circuit_unavailable"]),
            excluded("e", &["some_reason_nobody_wrote_a_bucket_for"]),
            excluded("f", &[]),
        ];
        let summary = summarize_exhaustion(&exclusions);
        assert_eq!(summary.checked, 6);
        let total: u32 = summary.buckets.iter().map(|entry| entry.candidates).sum();
        assert_eq!(total, summary.checked);
    }

    #[test]
    fn a_candidate_is_attributed_to_its_most_actionable_reason() {
        // Rate-limited AND lacking vision: waiting would not have helped it, so
        // it is a capability problem.
        let summary = summarize_exhaustion(&[excluded(
            "a",
            &["local_circuit_unavailable", "missing_modality:image"],
        )]);
        assert_eq!(summary.dominant(), Some(ExclusionBucket::Capability));
    }

    #[test]
    fn buckets_rank_by_count_then_by_actionability() {
        let exclusions = vec![
            excluded("a", &["local_circuit_unavailable"]),
            excluded("b", &["local_circuit_unavailable"]),
            excluded("c", &["local_circuit_unavailable"]),
            excluded("d", &["credential_unavailable"]),
            excluded("e", &["context_window_too_small"]),
        ];
        let summary = summarize_exhaustion(&exclusions);
        assert_eq!(
            summary.dominant(),
            Some(ExclusionBucket::RateLimitedOrCooling)
        );

        // On a tie, the more actionable bucket wins.
        let tied = vec![
            excluded("a", &["local_circuit_unavailable"]),
            excluded("b", &["context_window_too_small"]),
        ];
        assert_eq!(
            summarize_exhaustion(&tied).dominant(),
            Some(ExclusionBucket::Capability)
        );
    }

    #[test]
    fn the_soonest_reset_is_the_minimum_not_the_first() {
        let exclusions = vec![
            CandidateExclusion {
                candidate: "a".to_owned(),
                reasons: vec!["local_circuit_unavailable".to_owned()],
                reset_millis: Some(90_000),
            },
            CandidateExclusion {
                candidate: "b".to_owned(),
                reasons: vec!["local_circuit_unavailable".to_owned()],
                reset_millis: Some(5_000),
            },
            CandidateExclusion {
                candidate: "c".to_owned(),
                reasons: vec!["local_circuit_unavailable".to_owned()],
                reset_millis: None,
            },
        ];
        assert_eq!(
            summarize_exhaustion(&exclusions).soonest_reset_millis,
            Some(5_000)
        );
    }

    /// The summary must be structurally unable to leak an identifier. This is a
    /// belt-and-braces check on top of the type not having a field for one.
    #[test]
    fn the_message_never_names_a_candidate() {
        let exclusions = vec![
            excluded(
                "openai-us-east/gpt-5.6-terra#key-3",
                &["credential_unavailable"],
            ),
            excluded("anthropic-eu/claude#prod", &["region_mismatch"]),
        ];
        let message = summarize_exhaustion(&exclusions).message();
        for needle in [
            "openai", "anthropic", "us-east", "eu", "key-3", "prod", "gpt", "claude",
        ] {
            assert!(
                !message.contains(needle),
                "message leaked {needle:?}: {message}"
            );
        }
    }

    #[test]
    fn the_message_reads_like_advice() {
        let exclusions = vec![
            CandidateExclusion {
                candidate: "a".to_owned(),
                reasons: vec!["local_circuit_unavailable".to_owned()],
                reset_millis: Some(1_380_000),
            },
            excluded("b", &["local_circuit_unavailable"]),
            excluded("c", &["credential_unavailable"]),
        ];
        assert_eq!(
            summarize_exhaustion(&exclusions).message(),
            "no eligible model: 3 candidates checked \
             (2 rate-limited or cooling down, 1 have no usable credential). \
             Add capacity or wait for the limits to reset. Soonest reset ~23m."
        );
    }

    #[test]
    fn an_empty_pool_says_so_rather_than_counting_zero_buckets() {
        let summary = summarize_exhaustion(&[]);
        assert_eq!(summary.checked, 0);
        assert!(summary.buckets.is_empty());
        assert_eq!(summary.dominant(), None);
        assert_eq!(
            summary.message(),
            "no candidate models were configured for this request"
        );
    }

    #[test]
    fn disposition_maps_every_bucket() {
        use ExhaustionDisposition::{ClientCapability, RetryLater, Unavailable};
        assert_eq!(
            exhaustion_disposition(ExclusionBucket::Capability),
            ClientCapability
        );
        assert_eq!(
            exhaustion_disposition(ExclusionBucket::PolicyExcluded),
            ClientCapability
        );
        assert_eq!(
            exhaustion_disposition(ExclusionBucket::RateLimitedOrCooling),
            RetryLater
        );
        assert_eq!(
            exhaustion_disposition(ExclusionBucket::CredentialMissing),
            Unavailable
        );
        assert_eq!(
            exhaustion_disposition(ExclusionBucket::Disabled),
            Unavailable
        );
        assert_eq!(
            exhaustion_disposition(ExclusionBucket::Deprioritized),
            Unavailable
        );
        assert_eq!(
            exhaustion_disposition(ExclusionBucket::Unknown),
            Unavailable
        );
    }

    #[test]
    fn eta_formatting_is_coarse_and_rounds_up() {
        assert_eq!(format_eta(1), "~1s");
        assert_eq!(format_eta(5_000), "~5s");
        assert_eq!(format_eta(89_000), "~89s");
        assert_eq!(format_eta(90_000), "~2m");
        assert_eq!(format_eta(1_380_000), "~23m");
        assert_eq!(format_eta(7_200_000), "~2h");
    }
}
