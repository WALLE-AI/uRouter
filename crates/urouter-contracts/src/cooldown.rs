use serde::{Deserialize, Serialize};


use crate::UpstreamErrorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureWindow {
    pub successes: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CooldownDirective {
    Ignore,
    RecordFailure,
    OpenCircuit,
}

#[must_use]
pub fn cooldown_directive(
    kind: UpstreamErrorKind,
    window: FailureWindow,
    tier_size: usize,
    failure_threshold_millis: u16,
) -> CooldownDirective {
    if !matches!(
        kind,
        UpstreamErrorKind::Transport
            | UpstreamErrorKind::Timeout
            | UpstreamErrorKind::RateLimited
            | UpstreamErrorKind::ServerError
            | UpstreamErrorKind::ProviderUnavailable
            | UpstreamErrorKind::Unauthorized
            | UpstreamErrorKind::NotFound
    ) {
        return CooldownDirective::Ignore;
    }
    let failures = window.failures.saturating_add(1);
    let total = window.successes.saturating_add(failures);
    let failure_millis = failures
        .saturating_mul(1_000)
        .checked_div(total)
        .unwrap_or(0);
    if tier_size > 1
        && (kind == UpstreamErrorKind::RateLimited
            || failure_millis >= u64::from(failure_threshold_millis))
    {
        CooldownDirective::OpenCircuit
    } else {
        CooldownDirective::RecordFailure
    }
}
