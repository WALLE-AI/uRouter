use serde::{Deserialize, Serialize};

use crate::{UpstreamBackoff, UpstreamErrorKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u8,
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl RetryPolicy {
    #[must_use]
    pub const fn should_retry(self, kind: UpstreamErrorKind, retries_used: u8) -> bool {
        kind.retryable() && retries_used < self.max_retries
    }

    /// Exponential back-off, or the upstream's own hint when it gave one.
    ///
    /// The hint is clamped by `max_backoff_ms` like every other wait. It is a
    /// duration this process will *sleep* while holding a capacity lease, a
    /// budget reservation and an in-flight permit; an upstream does not get to
    /// set that bound, only to suggest a value inside it. The unclamped figure
    /// is still preserved for the client — see [`RetryPlan`].
    #[must_use]
    pub fn backoff_ms(self, retries_used: u8, retry_after_ms: Option<u64>) -> u64 {
        retry_after_ms.map_or_else(
            || {
                self.base_backoff_ms
                    .saturating_mul(
                        1_u64
                            .checked_shl(u32::from(retries_used))
                            .unwrap_or(u64::MAX),
                    )
                    .min(self.max_backoff_ms)
            },
            |hint| hint.min(self.max_backoff_ms),
        )
    }

    /// Retained for callers that only need the directive. Prefer
    /// [`RetryPolicy::plan_retry`], which additionally reports the unclamped
    /// hint so the gateway can advertise an honest `Retry-After`.
    #[must_use]
    pub fn directive(
        self,
        kind: UpstreamErrorKind,
        retries_used: u8,
        candidate_count: usize,
        retry_after_ms: Option<u64>,
    ) -> RetryDirective {
        self.plan_retry(
            kind,
            retries_used,
            candidate_count,
            retry_after_ms.map(|backoff_millis| UpstreamBackoff {
                backoff_millis,
                source: crate::BackoffSource::RetryAfterSeconds,
            }),
        )
        .directive
    }

    /// The full retry decision: what to do next, plus what to tell the client.
    ///
    /// Three behaviours worth stating explicitly, because each one used to be a
    /// silent loss:
    ///
    /// * The hint survives reselection. `ReselectDeployment` carries a
    ///   `backoff_ms`, so a provider-wide 429 no longer results in an immediate
    ///   hammer of the next deployment on the same provider.
    /// * The hint is clamped for sleeping (see [`RetryPolicy::backoff_ms`]) but
    ///   reported unclamped in `advertised_retry_after_millis`.
    /// * A hint longer than `max_backoff_ms` with nowhere else to go returns
    ///   `Stop`. Sleeping thirty seconds inside a request while holding four
    ///   leases is worse for everyone than a fast 429 that says exactly when to
    ///   come back.
    #[must_use]
    pub fn plan_retry(
        self,
        kind: UpstreamErrorKind,
        retries_used: u8,
        candidate_count: usize,
        hint: Option<UpstreamBackoff>,
    ) -> RetryPlan {
        let advertised = hint.map(|hint| hint.backoff_millis);
        let hint_millis = advertised;

        if !self.should_retry(kind, retries_used) || candidate_count == 0 {
            return RetryPlan {
                directive: RetryDirective::Stop,
                advertised_retry_after_millis: advertised,
            };
        }

        let waits_past_bound = hint_millis.is_some_and(|millis| millis > self.max_backoff_ms);
        if waits_past_bound && candidate_count == 1 {
            return RetryPlan {
                directive: RetryDirective::Stop,
                advertised_retry_after_millis: advertised,
            };
        }

        let backoff_ms = self.backoff_ms(retries_used, hint_millis);
        let directive = if candidate_count == 1 {
            RetryDirective::RetrySameDeployment { backoff_ms }
        } else {
            RetryDirective::ReselectDeployment {
                // Without a hint, reselection is immediate — the historical
                // behaviour. A hint means the *provider* asked for a pause, and
                // sibling deployments usually share that provider's budget.
                backoff_ms: hint_millis.map_or(0, |_| backoff_ms),
            }
        };
        RetryPlan {
            directive,
            advertised_retry_after_millis: advertised,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RetryDirective {
    Stop,
    RetrySameDeployment { backoff_ms: u64 },
    ReselectDeployment { backoff_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPlan {
    pub directive: RetryDirective,
    /// The upstream's hint, unclamped. Never a sleep duration — this is what
    /// the gateway puts in its own outbound `Retry-After` so a client can back
    /// off for the full window even when this process refused to block for it.
    pub advertised_retry_after_millis: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BackoffSource;

    const POLICY: RetryPolicy = RetryPolicy {
        max_retries: 2,
        base_backoff_ms: 50,
        max_backoff_ms: 1_000,
    };

    fn hint(millis: u64) -> UpstreamBackoff {
        UpstreamBackoff {
            backoff_millis: millis,
            source: BackoffSource::RetryAfterSeconds,
        }
    }

    #[test]
    fn a_hint_is_clamped_for_sleeping_but_advertised_in_full() {
        let plan = POLICY.plan_retry(UpstreamErrorKind::RateLimited, 0, 2, Some(hint(30_000)));
        assert_eq!(
            plan.directive,
            RetryDirective::ReselectDeployment { backoff_ms: 1_000 }
        );
        assert_eq!(plan.advertised_retry_after_millis, Some(30_000));
    }

    #[test]
    fn a_hint_survives_reselection() {
        let plan = POLICY.plan_retry(UpstreamErrorKind::RateLimited, 0, 3, Some(hint(400)));
        assert_eq!(
            plan.directive,
            RetryDirective::ReselectDeployment { backoff_ms: 400 }
        );
    }

    #[test]
    fn reselection_without_a_hint_stays_immediate() {
        let plan = POLICY.plan_retry(UpstreamErrorKind::ServerError, 0, 3, None);
        assert_eq!(
            plan.directive,
            RetryDirective::ReselectDeployment { backoff_ms: 0 }
        );
        assert_eq!(plan.advertised_retry_after_millis, None);
    }

    #[test]
    fn a_hint_past_the_bound_with_nowhere_to_go_stops_and_tells_the_client() {
        let plan = POLICY.plan_retry(UpstreamErrorKind::RateLimited, 0, 1, Some(hint(30_000)));
        assert_eq!(plan.directive, RetryDirective::Stop);
        assert_eq!(plan.advertised_retry_after_millis, Some(30_000));
    }

    #[test]
    fn a_hint_inside_the_bound_is_slept_on_the_same_deployment() {
        let plan = POLICY.plan_retry(UpstreamErrorKind::RateLimited, 0, 1, Some(hint(750)));
        assert_eq!(
            plan.directive,
            RetryDirective::RetrySameDeployment { backoff_ms: 750 }
        );
    }

    #[test]
    fn terminal_kinds_and_exhausted_retries_stop_but_still_advertise() {
        let plan = POLICY.plan_retry(UpstreamErrorKind::BadRequest, 0, 2, Some(hint(5_000)));
        assert_eq!(plan.directive, RetryDirective::Stop);
        assert_eq!(plan.advertised_retry_after_millis, Some(5_000));

        let plan = POLICY.plan_retry(UpstreamErrorKind::Timeout, 2, 2, None);
        assert_eq!(plan.directive, RetryDirective::Stop);

        let plan = POLICY.plan_retry(UpstreamErrorKind::Timeout, 0, 0, None);
        assert_eq!(plan.directive, RetryDirective::Stop);
    }

    #[test]
    fn exponential_backoff_is_unchanged_without_a_hint() {
        assert_eq!(POLICY.backoff_ms(0, None), 50);
        assert_eq!(POLICY.backoff_ms(1, None), 100);
        assert_eq!(POLICY.backoff_ms(2, None), 200);
        // Still bounded.
        assert_eq!(POLICY.backoff_ms(60, None), 1_000);
    }
}
