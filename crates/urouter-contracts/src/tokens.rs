//! Token reservation arithmetic.
//!
//! Adapted from `FreeLLMAPI` `server/src/services/router.ts` (`OUTPUT_RESERVE_CAP`,
//! `routingReserveTokens`), MIT License, Copyright (c) 2026 Tashfeen Ahmed.

/// Default ceiling on the output tokens a request reserves.
///
/// Chosen to sit above what a chat completion actually emits in the
/// overwhelming majority of cases, and far below the `max_tokens` values
/// clients habitually send.
pub const OUTPUT_RESERVE_TOKENS_CAP: u64 = 2_000;

/// What to reserve when the client stated no output bound at all.
pub const DEFAULT_OUTPUT_RESERVE_TOKENS: u64 = 1_000;

/// The output-token budget a request *reserves*, as distinct from the budget it
/// is *allowed to consume*.
///
/// `max_tokens` is an upper bound clients almost never reach — SDK defaults and
/// copy-pasted snippets routinely send 32000 for a request that emits 200
/// tokens. Reserving the full amount against every candidate's context window
/// and against the per-minute token budget excludes the entire pool and returns
/// an exhaustion error **without a single upstream call**: a self-inflicted
/// outage caused by a number the client did not mean literally.
///
/// Under-reserving is the safe direction. Providers meter what was actually
/// consumed, so a too-small reservation risks an upstream 429 or 413 that the
/// retry and fallback machinery already handles, whereas over-reserving starves
/// routing before it starts.
///
/// The outgoing upstream request is never rewritten — only the reservation is
/// capped. A client that genuinely emits 32000 tokens still gets them.
///
/// `cap == 0` disables capping.
#[must_use]
pub const fn reserved_output_tokens(requested: Option<u64>, default: u64, cap: u64) -> u64 {
    let requested = match requested {
        Some(value) if value > 0 => value,
        _ => default,
    };
    if cap == 0 || requested < cap {
        requested
    } else {
        cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = OUTPUT_RESERVE_TOKENS_CAP;

    #[test]
    fn boundaries() {
        // Absent or zero falls back to the caller's default.
        assert_eq!(reserved_output_tokens(None, 1_000, CAP), 1_000);
        assert_eq!(reserved_output_tokens(Some(0), 1_000, CAP), 1_000);
        // Below the cap passes through untouched.
        assert_eq!(reserved_output_tokens(Some(1), 1_000, CAP), 1);
        assert_eq!(reserved_output_tokens(Some(CAP - 1), 1_000, CAP), CAP - 1);
        // At and above the cap is capped.
        assert_eq!(reserved_output_tokens(Some(CAP), 1_000, CAP), CAP);
        assert_eq!(reserved_output_tokens(Some(CAP + 1), 1_000, CAP), CAP);
        assert_eq!(reserved_output_tokens(Some(u64::MAX), 1_000, CAP), CAP);
    }

    #[test]
    fn the_reported_bug_no_longer_empties_the_pool() {
        // A client sending max_tokens: 32000 used to reserve all 32000 against
        // every model's context window, excluding a 32k-context pool entirely.
        assert_eq!(reserved_output_tokens(Some(32_000), 1_000, CAP), 2_000);
    }

    #[test]
    fn a_zero_cap_disables_capping() {
        assert_eq!(reserved_output_tokens(Some(32_000), 1_000, 0), 32_000);
        assert_eq!(reserved_output_tokens(None, 4_096, 0), 4_096);
    }

    #[test]
    fn a_default_above_the_cap_is_still_capped() {
        // An operator raising --quota-default-max-output-tokens must not be able
        // to sneak past the cap via the no-max_tokens path.
        assert_eq!(reserved_output_tokens(None, 8_192, CAP), CAP);
    }

    /// The admission path passes `default = 0` so that a request with no
    /// `max_tokens` reserves nothing, exactly as before this cap existed. The
    /// behaviour change is confined to requests that explicitly asked for more
    /// than the cap.
    #[test]
    fn admissions_zero_default_preserves_historical_behaviour() {
        assert_eq!(reserved_output_tokens(None, 0, CAP), 0);
        assert_eq!(reserved_output_tokens(Some(0), 0, CAP), 0);
        assert_eq!(reserved_output_tokens(Some(500), 0, CAP), 500);
    }
}
