//! Extraction of an upstream's own back-off hint from a failed response.
//!
//! Portions of the parsing strategy (the depth-capped body scan for a stated
//! `retryDelay`, and the anchored prose fallback) are adapted from `FreeLLMAPI`
//! `server/src/providers/base.ts`, MIT License, Copyright (c) 2026 Tashfeen
//! Ahmed.
//!
//! Everything here is a pure function over facts the caller has already pulled
//! off the wire. No clocks, no sockets: `now_unix_millis` is supplied, so an
//! HTTP-date `Retry-After` can be resolved to a delay without this crate
//! learning what time it is.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{FallbackCause, UpstreamErrorKind};

/// Ceiling on any extracted hint. A malformed or hostile `Retry-After` of
/// `99999999999` would otherwise bench a deployment effectively forever, since
/// the value feeds a cooldown expiry. A day is longer than any real provider
/// reset window, so clamping here cannot mask a genuine hint.
pub const MAX_BACKOFF_HINT_MILLIS: u64 = 86_400_000;

/// How deep the structured body scan will walk. Provider error envelopes nest a
/// few levels at most; the cap is what stops a pathological body from spinning.
pub const DEFAULT_BODY_SCAN_DEPTH: u8 = 6;

/// Only this many bytes of the body are scanned for prose. Prose is the weakest
/// signal and the most expensive to scan; a provider that buries a retry
/// sentence past 4 KiB of stack trace is not worth chasing.
pub const MAX_PROSE_SCAN_BYTES: usize = 4_096;

/// Where a hint came from. Recorded on the attempt so an operator can see how
/// often the gateway is reading a promise (`RetryAfter*`, `RateLimitReset`,
/// `ErrorBodyField`) versus guessing from a sentence (`ErrorBodyProse`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackoffSource {
    RetryAfterSeconds,
    RetryAfterDate,
    RateLimitReset,
    ErrorBodyField,
    ErrorBodyProse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamBackoff {
    pub backoff_millis: u64,
    pub source: BackoffSource,
}

impl UpstreamBackoff {
    #[must_use]
    const fn new(backoff_millis: u64, source: BackoffSource) -> Self {
        Self {
            backoff_millis: clamp_hint(backoff_millis),
            source,
        }
    }
}

/// Everything a failed upstream response tells us, with the transport already
/// stripped. Header names must be lowercased by the caller.
#[derive(Debug, Clone, Copy)]
pub struct UpstreamResponseFacts<'a> {
    pub status: u16,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: Option<&'a str>,
    pub now_unix_millis: u64,
    pub body_scan_depth: u8,
}

const fn clamp_hint(millis: u64) -> u64 {
    if millis > MAX_BACKOFF_HINT_MILLIS {
        MAX_BACKOFF_HINT_MILLIS
    } else {
        millis
    }
}

fn header<'a>(facts: &UpstreamResponseFacts<'a>, name: &str) -> Option<&'a str> {
    facts
        .headers
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
}

/// The back-off the upstream asked for, in priority order:
///
/// 1. `retry-after` as delta-seconds
/// 2. `retry-after` as an HTTP-date
/// 3. an `x-ratelimit-reset*` / `ratelimit-reset` header
/// 4. a stated field anywhere in the structured body
/// 5. an anchored retry sentence in the body text
///
/// A stated field is a promise; a sentence is an observation. That is the whole
/// ordering rationale — earlier sources are more authoritative, so the first hit
/// wins and no averaging happens.
#[must_use]
pub fn extract_backoff(facts: &UpstreamResponseFacts<'_>) -> Option<UpstreamBackoff> {
    if let Some(raw) = header(facts, "retry-after")
        && let Some(hint) = parse_retry_after(raw, facts.now_unix_millis)
    {
        return Some(hint);
    }

    for name in RATE_LIMIT_RESET_HEADERS {
        if let Some(raw) = header(facts, name)
            && let Some(millis) = parse_rate_limit_reset(raw, facts.now_unix_millis)
        {
            return Some(UpstreamBackoff::new(millis, BackoffSource::RateLimitReset));
        }
    }

    let body = facts.body?;
    if let Ok(parsed) = serde_json::from_str::<Value>(body)
        && let Some(millis) = scan_retry_delay_field(&parsed, facts.body_scan_depth)
    {
        return Some(UpstreamBackoff::new(millis, BackoffSource::ErrorBodyField));
    }

    let window = &body[..body.len().min(MAX_PROSE_SCAN_BYTES)];
    scan_retry_prose_millis(window)
        .map(|millis| UpstreamBackoff::new(millis, BackoffSource::ErrorBodyProse))
}

/// Headers in the order they are consulted. `-requests` before `-tokens`
/// because a request-rate reset is the shorter, more common wait; taking the
/// longer one first would over-bench a deployment that only tripped the RPM.
const RATE_LIMIT_RESET_HEADERS: [&str; 4] = [
    "x-ratelimit-reset-requests",
    "x-ratelimit-reset-tokens",
    "x-ratelimit-reset",
    "ratelimit-reset",
];

// ── Retry-After ─────────────────────────────────────────────────────────────

/// `Retry-After` in either RFC 7231 form: delta-seconds, or an HTTP-date.
#[must_use]
pub fn parse_retry_after(value: &str, now_unix_millis: u64) -> Option<UpstreamBackoff> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        let seconds: u64 = trimmed.parse().ok()?;
        return Some(UpstreamBackoff::new(
            seconds.saturating_mul(1_000),
            BackoffSource::RetryAfterSeconds,
        ));
    }
    let at = parse_http_date_unix_millis(trimmed)?;
    Some(UpstreamBackoff::new(
        at.saturating_sub(now_unix_millis),
        BackoffSource::RetryAfterDate,
    ))
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Unix milliseconds for an HTTP-date, in any of the three RFC 7231 forms:
/// IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), RFC 850
/// (`Sunday, 06-Nov-94 08:49:37 GMT`), and asctime (`Sun Nov  6 08:49:37 1994`).
///
/// Deliberately hand-rolled rather than pulling in a date crate: every date
/// library's entry point is anchored on `SystemTime`, and this crate's contract
/// is that it never learns the time. A `-> Option<u64>` of unix millis has no
/// such ambiguity.
#[must_use]
pub fn parse_http_date_unix_millis(value: &str) -> Option<u64> {
    let rest = value.trim().trim_end_matches("GMT").trim_end_matches("UTC");
    let rest = rest.trim();
    // Drop the day-of-week prefix, in both the comma'd and asctime forms.
    let rest = rest.split_once(',').map_or_else(
        || rest.split_once(' ').map_or(rest, |(_, tail)| tail),
        |(_, tail)| tail,
    );
    let rest = rest.trim();

    let mut fields = rest.split_whitespace();
    let first = fields.next()?;

    let (day, month, year, time) = if first.contains('-') {
        // RFC 850: 06-Nov-94 08:49:37
        let mut parts = first.split('-');
        let day = parts.next()?.parse::<u32>().ok()?;
        let month = month_index(parts.next()?)?;
        let two_digit = parts.next()?.parse::<i64>().ok()?;
        // RFC 6265-style windowing: a two-digit year more than 50 years ahead
        // is read as the previous century.
        let year = if two_digit < 70 {
            2000 + two_digit
        } else {
            1900 + two_digit
        };
        (day, month, year, fields.next()?)
    } else if let Ok(day) = first.parse::<u32>() {
        // IMF-fixdate: 06 Nov 1994 08:49:37
        let month = month_index(fields.next()?)?;
        let year = fields.next()?.parse::<i64>().ok()?;
        (day, month, year, fields.next()?)
    } else {
        // asctime: Nov  6 08:49:37 1994
        let month = month_index(first)?;
        let day = fields.next()?.parse::<u32>().ok()?;
        let time = fields.next()?;
        let year = fields.next()?.parse::<i64>().ok()?;
        (day, month, year, time)
    };

    let mut hms = time.split(':');
    let hour: u64 = hms.next()?.parse().ok()?;
    let minute: u64 = hms.next()?.parse().ok()?;
    let second: u64 = hms.next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 || day == 0 || day > 31 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let seconds = days.checked_mul(86_400)?.checked_add(
        i64::try_from(hour * 3_600 + minute * 60 + second).ok()?,
    )?;
    u64::try_from(seconds).ok()?.checked_mul(1_000)
}

fn month_index(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let key = lower.get(..3)?;
    MONTHS
        .iter()
        .position(|month| *month == key)
        .map(|index| u32::try_from(index).unwrap_or(0) + 1)
}

/// Howard Hinnant's days-from-civil: days since 1970-01-01 for a proleptic
/// Gregorian date. Exact for every year in range, leap years included.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = u64::try_from(year - era * 400).unwrap_or(0); // [0, 399]
    let shifted_month = i64::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year =
        (153 * u64::try_from(shifted_month).unwrap_or(0) + 2) / 5 + u64::from(day) - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + i64::try_from(day_of_era).unwrap_or(0) - 719_468
}

// ── x-ratelimit-reset* ──────────────────────────────────────────────────────

/// An `x-ratelimit-reset*` value, which providers spell three different ways:
/// a duration literal (`1.5s`, `6m0s`), delta-seconds, or an absolute epoch.
///
/// The epoch disambiguation is by magnitude: a bare `1700000000` cannot
/// plausibly be "wait 54 years", and a 13-digit number cannot plausibly be
/// seconds. Anything smaller is read as a delta.
#[must_use]
pub fn parse_rate_limit_reset(value: &str, now_unix_millis: u64) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(millis) = parse_duration_literal_millis(trimmed) {
        return Some(millis);
    }
    let whole: u64 = trimmed
        .split_once('.')
        .map_or(trimmed, |(whole, _)| whole)
        .parse()
        .ok()?;
    if whole >= 1_000_000_000_000 {
        // epoch milliseconds
        Some(whole.saturating_sub(now_unix_millis))
    } else if whole >= 1_000_000_000 {
        // epoch seconds
        Some(whole.saturating_mul(1_000).saturating_sub(now_unix_millis))
    } else {
        scaled_millis(trimmed, 1_000)
    }
}

/// A duration literal: `17s`, `1.5s`, `250ms`, `6m0s`, `1h30m`. Composite forms
/// sum; a bare number with no unit is not a duration and returns `None` so the
/// caller can decide what a unitless value means.
#[must_use]
pub fn parse_duration_literal_millis(value: &str) -> Option<u64> {
    let bytes = value.trim().as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut index = 0;
    let mut segments = 0;
    while index < bytes.len() {
        let start = index;
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
            index += 1;
        }
        if index == start {
            return None;
        }
        let magnitude = core::str::from_utf8(&bytes[start..index]).ok()?;
        let unit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        if index == unit_start {
            return None;
        }
        let unit = core::str::from_utf8(&bytes[unit_start..index]).ok()?;
        let millis = scaled_millis(magnitude, unit_millis(unit)?)?;
        total = total.saturating_add(millis);
        segments += 1;
    }
    (segments > 0).then_some(total)
}

fn unit_millis(unit: &str) -> Option<u64> {
    match unit.to_ascii_lowercase().as_str() {
        "ms" | "milli" | "millis" | "millisecond" | "milliseconds" => Some(1),
        "s" | "sec" | "secs" | "second" | "seconds" => Some(1_000),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(60_000),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(3_600_000),
        _ => None,
    }
}

// ── structured body ─────────────────────────────────────────────────────────

/// Keys a provider might state a back-off under. Walking for these is more
/// durable than a list of exact JSON paths: the shape varies by provider and
/// even by endpoint within one provider, and a walk costs less to maintain than
/// one parser per adapter.
const RETRY_DELAY_KEYS: [&str; 7] = [
    "retrydelay",
    "retryafter",
    "retryafterseconds",
    "retryafterms",
    "retryaftermillis",
    "resetat",
    "retryafterinms",
];

/// Multiply a decimal literal by a millisecond scale using integer arithmetic.
///
/// `"7.66"` at scale 1000 is exactly 7660, not 7659.999…. Floats are avoided
/// here on principle: this crate's outputs feed replayable decisions, and a
/// value that differs in the last bit between platforms is a value that breaks
/// replay. Fractional digits past the scale's precision are truncated.
fn scaled_millis(literal: &str, scale_millis: u64) -> Option<u64> {
    let (whole, fraction) = literal.split_once('.').unwrap_or((literal, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut total = whole.saturating_mul(scale_millis);
    if !fraction.is_empty() {
        // Only as many fractional digits as the scale can represent; more would
        // round to zero anyway.
        let digits = fraction.len().min(9);
        let value: u64 = fraction.get(..digits)?.parse().ok()?;
        let divisor = 10_u64.checked_pow(u32::try_from(digits).ok()?)?;
        total = total.saturating_add(value.saturating_mul(scale_millis) / divisor);
    }
    Some(total)
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Depth-capped search for the first stated back-off field anywhere in the body.
#[must_use]
pub fn scan_retry_delay_field(body: &Value, max_depth: u8) -> Option<u64> {
    scan_retry_delay_inner(body, max_depth, 0)
}

fn scan_retry_delay_inner(node: &Value, max_depth: u8, depth: u8) -> Option<u64> {
    if depth > max_depth {
        return None;
    }
    match node {
        Value::Array(items) => items
            .iter()
            .find_map(|item| scan_retry_delay_inner(item, max_depth, depth + 1)),
        Value::Object(map) => {
            for (key, value) in map {
                let normalized = normalize_key(key);
                if RETRY_DELAY_KEYS.contains(&normalized.as_str())
                    && let Some(millis) = stated_delay_millis(&normalized, value)
                {
                    return Some(millis);
                }
            }
            map.values()
                .find_map(|value| scan_retry_delay_inner(value, max_depth, depth + 1))
        }
        _ => None,
    }
}

fn stated_delay_millis(key: &str, value: &Value) -> Option<u64> {
    // A `_ms`-suffixed key states milliseconds; everything else states seconds.
    let already_millis = key.ends_with("ms") || key.ends_with("millis");
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if let Some(millis) = parse_duration_literal_millis(trimmed) {
                return Some(millis);
            }
            scaled_millis(trimmed, if already_millis { 1 } else { 1_000 })
        }
        Value::Number(number) => {
            scaled_millis(&number.to_string(), if already_millis { 1 } else { 1_000 })
        }
        _ => None,
    }
}

// ── prose ───────────────────────────────────────────────────────────────────

/// Phrases that make a following number a back-off rather than a coincidence.
///
/// The anchor requirement is the safety property. Without it, `"you have 30
/// tokens remaining"` or `"your 30 second timeout"` becomes a 30-second bench.
const PROSE_ANCHORS: [&str; 5] = [
    "try again in",
    "retry after",
    "retry in",
    "please wait",
    "available in",
];

/// Characters of filler tolerated between the anchor and the number, e.g.
/// `try again in about 7.66s`.
const MAX_ANCHOR_FILLER: usize = 32;

/// An anchored retry sentence, in milliseconds. Returns `None` when no anchor
/// phrase is present — a number on its own is never a back-off.
#[must_use]
pub fn scan_retry_prose_millis(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let mut best: Option<usize> = None;
    for anchor in PROSE_ANCHORS {
        if let Some(at) = lower.find(anchor)
            && best.is_none_or(|current| at < current)
        {
            best = Some(at + anchor.len());
        }
    }
    let mut cursor = best?;
    let bytes = lower.as_bytes();
    let limit = (cursor + MAX_ANCHOR_FILLER).min(bytes.len());
    while cursor < limit && !bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    if cursor >= limit {
        return None;
    }
    let start = cursor;
    while cursor < bytes.len() && (bytes[cursor].is_ascii_digit() || bytes[cursor] == b'.') {
        cursor += 1;
    }
    let magnitude = lower.get(start..cursor)?.to_owned();
    while cursor < bytes.len() && bytes[cursor] == b' ' {
        cursor += 1;
    }
    let unit_start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
        cursor += 1;
    }
    scaled_millis(&magnitude, unit_millis(lower.get(unit_start..cursor)?)?)
}

// ── body-aware classification ───────────────────────────────────────────────

/// Keys under which providers put a machine-readable error code.
const ERROR_CODE_KEYS: [&str; 5] = ["code", "type", "reason", "status", "errorcode"];

/// The first machine-readable error code found in the body, lowercased.
#[must_use]
pub fn upstream_error_code(body: &Value, max_depth: u8) -> Option<String> {
    upstream_error_code_inner(body, max_depth, 0)
}

fn upstream_error_code_inner(node: &Value, max_depth: u8, depth: u8) -> Option<String> {
    if depth > max_depth {
        return None;
    }
    match node {
        Value::Array(items) => items
            .iter()
            .find_map(|item| upstream_error_code_inner(item, max_depth, depth + 1)),
        Value::Object(map) => {
            for (key, value) in map {
                if ERROR_CODE_KEYS.contains(&normalize_key(key).as_str())
                    && let Value::String(text) = value
                    && !text.trim().is_empty()
                {
                    return Some(text.trim().to_ascii_lowercase());
                }
            }
            map.values()
                .find_map(|value| upstream_error_code_inner(value, max_depth, depth + 1))
        }
        _ => None,
    }
}

const CONTEXT_WINDOW_CODES: [&str; 5] = [
    "context_length_exceeded",
    "context_window_exceeded",
    "prompt_too_long",
    "string_above_max_length",
    "max_tokens_exceeded",
];

const CONTENT_POLICY_CODES: [&str; 4] = [
    "content_policy_violation",
    "content_filter",
    "safety",
    "prohibited_content",
];

const QUOTA_CODES: [&str; 4] = [
    "rate_limit_exceeded",
    "quota_exceeded",
    "resource_exhausted",
    "insufficient_quota",
];

fn matches_any(code: Option<&str>, needles: &[&str]) -> bool {
    code.is_some_and(|code| needles.iter().any(|needle| code.contains(needle)))
}

/// The base status table, with one body-derived override.
///
/// A provider that answers a rate limit with something other than 429 (some
/// answer 400 or 403 with `insufficient_quota`) is still rate-limited, and
/// treating it as a terminal `BadRequest` means never retrying and never
/// cooling the deployment down.
///
/// Note what is deliberately NOT overridden: a `context_length_exceeded` stays
/// `BadRequest`. It is genuinely non-retryable — the same prompt on the same
/// deployment will fail identically — and the deployment is healthy, so it must
/// not be counted a failure. Only the *fallback* routing changes; see
/// [`fallback_cause_for`].
#[must_use]
pub fn classify_upstream(status: u16, body_code: Option<&str>) -> UpstreamErrorKind {
    if status != 429 && (400..500).contains(&status) && matches_any(body_code, &QUOTA_CODES) {
        return UpstreamErrorKind::RateLimited;
    }
    match status {
        401 | 403 => UpstreamErrorKind::Unauthorized,
        404 => UpstreamErrorKind::NotFound,
        429 => UpstreamErrorKind::RateLimited,
        502..=504 => UpstreamErrorKind::ProviderUnavailable,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::BadRequest,
    }
}

/// The fallback chain a failure should follow.
///
/// This is where the body actually changes routing: `FallbackCause::ContextWindow`
/// and `ContentPolicy` exist in the enum and are honoured by
/// `TierConfig::fallbacks_for`, but without body inspection nothing upstream can
/// ever produce them — every over-long prompt collapsed into a terminal
/// `BadRequest` instead of falling over to a longer-context tier.
#[must_use]
pub fn fallback_cause_for(kind: UpstreamErrorKind, body_code: Option<&str>) -> FallbackCause {
    if matches_any(body_code, &CONTEXT_WINDOW_CODES) {
        return FallbackCause::ContextWindow;
    }
    if matches_any(body_code, &CONTENT_POLICY_CODES) {
        return FallbackCause::ContentPolicy;
    }
    FallbackCause::from(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_700_000_000_000; // 2023-11-14T22:13:20Z

    fn facts<'a>(
        status: u16,
        headers: &'a [(&'a str, &'a str)],
        body: Option<&'a str>,
    ) -> UpstreamResponseFacts<'a> {
        UpstreamResponseFacts {
            status,
            headers,
            body,
            now_unix_millis: NOW,
            body_scan_depth: DEFAULT_BODY_SCAN_DEPTH,
        }
    }

    #[test]
    fn retry_after_delta_seconds() {
        let hint = parse_retry_after("30", NOW).unwrap();
        assert_eq!(hint.backoff_millis, 30_000);
        assert_eq!(hint.source, BackoffSource::RetryAfterSeconds);
    }

    #[test]
    fn retry_after_http_date_forms_agree() {
        // All three RFC 7231 forms of the same instant.
        let imf = parse_http_date_unix_millis("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        let rfc850 = parse_http_date_unix_millis("Sunday, 06-Nov-94 08:49:37 GMT").unwrap();
        let asctime = parse_http_date_unix_millis("Sun Nov  6 08:49:37 1994").unwrap();
        assert_eq!(imf, 784_111_777_000);
        assert_eq!(imf, rfc850);
        assert_eq!(imf, asctime);
    }

    #[test]
    fn http_date_handles_epoch_and_leap_years() {
        assert_eq!(
            parse_http_date_unix_millis("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(0)
        );
        // 2000 is a leap year (divisible by 400); 1900 is not.
        assert_eq!(
            parse_http_date_unix_millis("Tue, 29 Feb 2000 00:00:00 GMT"),
            Some(951_782_400_000)
        );
        assert_eq!(
            parse_http_date_unix_millis("Mon, 29 Feb 2024 12:00:00 GMT"),
            Some(1_709_208_000_000)
        );
        assert_eq!(parse_http_date_unix_millis("not a date"), None);
    }

    #[test]
    fn retry_after_date_becomes_a_delay_not_an_instant() {
        let headers = [("retry-after", "Tue, 14 Nov 2023 22:14:20 GMT")];
        let hint = extract_backoff(&facts(429, &headers, None)).unwrap();
        assert_eq!(hint.backoff_millis, 60_000);
        assert_eq!(hint.source, BackoffSource::RetryAfterDate);
    }

    #[test]
    fn a_date_already_in_the_past_is_zero_not_a_wrap() {
        let headers = [("retry-after", "Thu, 01 Jan 1970 00:00:00 GMT")];
        let hint = extract_backoff(&facts(429, &headers, None)).unwrap();
        assert_eq!(hint.backoff_millis, 0);
    }

    #[test]
    fn hostile_retry_after_is_clamped_to_one_day() {
        let hint = parse_retry_after("99999999999", NOW).unwrap();
        assert_eq!(hint.backoff_millis, MAX_BACKOFF_HINT_MILLIS);
    }

    #[test]
    fn rate_limit_reset_accepts_duration_delta_and_epoch() {
        // Go-style composite duration, as OpenAI emits.
        assert_eq!(parse_rate_limit_reset("6m0s", NOW), Some(360_000));
        assert_eq!(parse_rate_limit_reset("1.5s", NOW), Some(1_500));
        assert_eq!(parse_rate_limit_reset("250ms", NOW), Some(250));
        // Bare delta seconds.
        assert_eq!(parse_rate_limit_reset("42", NOW), Some(42_000));
        // Absolute epoch seconds, 30s in the future.
        assert_eq!(parse_rate_limit_reset("1700000030", NOW), Some(30_000));
        // Absolute epoch milliseconds.
        assert_eq!(parse_rate_limit_reset("1700000030000", NOW), Some(30_000));
    }

    #[test]
    fn google_retry_info_protobuf_duration() {
        let body = json!({
            "error": {
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "details": [
                    {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "17s"}
                ]
            }
        });
        assert_eq!(
            scan_retry_delay_field(&body, DEFAULT_BODY_SCAN_DEPTH),
            Some(17_000)
        );
    }

    #[test]
    fn body_field_millis_suffix_is_not_multiplied() {
        let body = json!({"error": {"retry_after_ms": 1_500}});
        assert_eq!(
            scan_retry_delay_field(&body, DEFAULT_BODY_SCAN_DEPTH),
            Some(1_500)
        );
        let body = json!({"error": {"retry_after": 3}});
        assert_eq!(
            scan_retry_delay_field(&body, DEFAULT_BODY_SCAN_DEPTH),
            Some(3_000)
        );
    }

    #[test]
    fn body_scan_respects_the_depth_cap() {
        let deep = json!({"a": {"b": {"c": {"d": {"e": {"f": {"g": {"retryDelay": "5s"}}}}}}}});
        assert_eq!(scan_retry_delay_field(&deep, 2), None);
        assert!(scan_retry_delay_field(&deep, 10).is_some());
    }

    #[test]
    fn anchored_prose_is_read_unanchored_numbers_are_not() {
        assert_eq!(
            scan_retry_prose_millis("Rate limit reached. Please try again in 7.66s."),
            Some(7_660)
        );
        assert_eq!(
            scan_retry_prose_millis("retry after 30 seconds"),
            Some(30_000)
        );
        assert_eq!(scan_retry_prose_millis("try again in about 2m"), Some(120_000));

        // The whole point of the anchor: these must NOT become a back-off.
        assert_eq!(scan_retry_prose_millis("you have 30 tokens remaining"), None);
        assert_eq!(scan_retry_prose_millis("your 30 second timeout elapsed"), None);
        assert_eq!(scan_retry_prose_millis("error 429 from upstream"), None);
        assert_eq!(scan_retry_prose_millis(""), None);
    }

    #[test]
    fn prose_anchor_filler_is_bounded() {
        // A number far past the anchor is not the anchor's number.
        let far = format!("try again in{} 5s", " ".repeat(64));
        assert_eq!(scan_retry_prose_millis(&far), None);
    }

    #[test]
    fn a_stated_field_outranks_a_sentence() {
        let body = r#"{"error":{"message":"try again in 90s","retry_after":3}}"#;
        let hint = extract_backoff(&facts(429, &[], Some(body))).unwrap();
        assert_eq!(hint.backoff_millis, 3_000);
        assert_eq!(hint.source, BackoffSource::ErrorBodyField);
    }

    #[test]
    fn a_header_outranks_the_body() {
        let headers = [("retry-after", "5")];
        let body = r#"{"error":{"retry_after":60}}"#;
        let hint = extract_backoff(&facts(429, &headers, Some(body))).unwrap();
        assert_eq!(hint.backoff_millis, 5_000);
        assert_eq!(hint.source, BackoffSource::RetryAfterSeconds);
    }

    #[test]
    fn prose_is_the_last_resort_and_is_labelled_as_such() {
        let body = r#"{"error":{"message":"Rate limited, please try again in 12s"}}"#;
        let hint = extract_backoff(&facts(429, &[], Some(body))).unwrap();
        assert_eq!(hint.backoff_millis, 12_000);
        assert_eq!(hint.source, BackoffSource::ErrorBodyProse);
    }

    #[test]
    fn no_hint_anywhere_is_none() {
        assert!(extract_backoff(&facts(500, &[], Some("upstream exploded"))).is_none());
        assert!(extract_backoff(&facts(500, &[], None)).is_none());
    }

    #[test]
    fn error_code_extraction_finds_nested_codes() {
        let body = json!({"error": {"code": "context_length_exceeded", "message": "…"}});
        assert_eq!(
            upstream_error_code(&body, DEFAULT_BODY_SCAN_DEPTH).as_deref(),
            Some("context_length_exceeded")
        );
        let body = json!({"error": {"type": "overloaded_error"}});
        assert_eq!(
            upstream_error_code(&body, DEFAULT_BODY_SCAN_DEPTH).as_deref(),
            Some("overloaded_error")
        );
    }

    #[test]
    fn context_length_keeps_its_kind_but_changes_its_fallback() {
        let code = Some("context_length_exceeded");
        let kind = classify_upstream(400, code);
        // Still terminal: retrying the same prompt on the same deployment is futile
        // and the deployment itself is healthy.
        assert_eq!(kind, UpstreamErrorKind::BadRequest);
        assert!(!kind.retryable());
        // But the fallback chain now knows why, so a longer-context tier is reachable.
        assert_eq!(
            fallback_cause_for(kind, code),
            FallbackCause::ContextWindow
        );
    }

    #[test]
    fn content_filter_routes_to_its_own_chain() {
        let code = Some("content_policy_violation");
        assert_eq!(
            fallback_cause_for(classify_upstream(400, code), code),
            FallbackCause::ContentPolicy
        );
    }

    #[test]
    fn quota_exhaustion_on_a_non_429_is_still_rate_limiting() {
        assert_eq!(
            classify_upstream(403, Some("insufficient_quota")),
            UpstreamErrorKind::RateLimited
        );
        assert_eq!(
            classify_upstream(400, Some("resource_exhausted")),
            UpstreamErrorKind::RateLimited
        );
        // A 5xx is not reinterpreted — only 4xx codes are.
        assert_eq!(
            classify_upstream(503, Some("resource_exhausted")),
            UpstreamErrorKind::ProviderUnavailable
        );
    }

    #[test]
    fn status_table_is_unchanged_without_a_body_code() {
        assert_eq!(classify_upstream(401, None), UpstreamErrorKind::Unauthorized);
        assert_eq!(classify_upstream(403, None), UpstreamErrorKind::Unauthorized);
        assert_eq!(classify_upstream(404, None), UpstreamErrorKind::NotFound);
        assert_eq!(classify_upstream(429, None), UpstreamErrorKind::RateLimited);
        assert_eq!(
            classify_upstream(502, None),
            UpstreamErrorKind::ProviderUnavailable
        );
        assert_eq!(classify_upstream(500, None), UpstreamErrorKind::ServerError);
        assert_eq!(classify_upstream(422, None), UpstreamErrorKind::BadRequest);
    }
}
