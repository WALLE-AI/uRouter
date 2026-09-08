//! Trajectory signal extraction from a chat request.
//!
//! Answers "how is this run going?" — a question the request content cannot
//! answer. A conversation that has hit the same error four times needs a
//! different tier from one that is producing code steadily, and neither is
//! visible in the latest user message.
//!
//! The signals are already in the request: an agent harness sends its whole
//! trajectory on every turn, including tool calls and their results. Nothing
//! needs to be stored between turns and the caller does not have to cooperate.
//!
//! Dimension names and the severity table follow NVIDIA's Switchyard stage
//! router (Apache-2.0), whose pattern set was mined from real agent traces.
//! The scoring here is integer-only so that a decision can be replayed exactly.

use serde_json::Value;

use crate::{SignalOrigin, TrajectorySignalStrength};

/// Severity of the worst tool failure in the recent window, in millis.
const SEVERITY_SOFT: u16 = 300;
const SEVERITY_HARD: u16 = 700;
const SEVERITY_CRITICAL: u16 = 1_000;

/// Tool results scanned for `recent_*` counts and windowed severity.
pub const DEFAULT_RECENT_WINDOW: u8 = 3;
/// How deep a run must be before "not producing" is read as stalled rather than
/// as the ordinary quiet at the start of a task.
pub const DEFAULT_STALL_MIN_TOOL_RESULTS: u32 = 8;
/// Tool-result text beyond this is not scanned. A single result can be a whole
/// file; the failure indicators are near the top, and an unbounded scan would
/// make extraction cost scale with payload size.
pub const MAX_RESULT_SCAN_BYTES: usize = 4_096;

/// `(severity, lower-cased needles — any hit fires the pattern)`.
///
/// Substrings rather than regexes: the grammar needs no backtracking, and
/// `urouter-core` stays free of a regex dependency.
static ERROR_PATTERNS: &[(u16, &[&str])] = &[
    (
        SEVERITY_CRITICAL,
        &["out of memory", "memoryerror", "cannot allocate memory"],
    ),
    (
        SEVERITY_CRITICAL,
        &["connection refused", "connectionrefusederror", "econnrefused"],
    ),
    (SEVERITY_HARD, &["traceback (most recent call last)"]),
    (
        SEVERITY_HARD,
        &["modulenotfounderror:", "importerror:", "no module named "],
    ),
    (SEVERITY_HARD, &["command not found", "/usr/bin/env: "]),
    (SEVERITY_HARD, &["assertionerror"]),
    (SEVERITY_HARD, &["valueerror:"]),
    (SEVERITY_HARD, &["syntaxerror:"]),
    (
        SEVERITY_HARD,
        &["timed out", "timeouterror", "deadline exceeded"],
    ),
    (
        SEVERITY_HARD,
        &[
            "filenotfounderror:",
            "no such file or directory",
            // Anchored as "file does not exist", never a bare "does not
            // exist" — the short form fires on `ls` output and on prose that
            // merely mentions a missing thing.
            "file does not exist",
        ],
    ),
    // A non-zero exit with no recognisable exception behind it. Soft because a
    // command that "fails" is routine: a grep with no match, a test run used as
    // a probe.
    (
        SEVERITY_SOFT,
        &[
            "exit code 1",
            "exit code 2",
            "exit status 1",
            "returned non-zero",
            "exited with code",
        ],
    ),
];

/// Anchored test-success indicators. Deliberately narrow: a false positive here
/// de-escalates a run that is not actually settled.
static TEST_PASS_PATTERNS: &[&str] = &[
    "test result: ok",
    "tests passed",
    "all tests passed",
    "0 failed",
    "passed, 0 failed",
    "=== pass",
    "build succeeded",
];

/// Tool names that write new content.
static WRITE_TOOL_NAMES: &[&str] = &["write", "create_file", "new_file", "write_file"];
/// Tool names that modify existing content.
static EDIT_TOOL_NAMES: &[&str] = &[
    "edit",
    "multiedit",
    "notebookedit",
    "str_replace",
    "str_replace_based_edit_tool",
    "text_editor",
    "patch",
];
/// Tool names that read without changing anything.
static READ_TOOL_NAMES: &[&str] = &["read", "read_file", "glob", "grep", "list_dir", "ls"];
/// Planning and bookkeeping tools. Investigative, not productive — this is what
/// separates a run that is searching from one that is stuck.
static PLAN_TOOL_NAMES: &[&str] = &["todowrite", "todoread", "task", "plan", "exit_plan_mode"];

/// Tuning for [`extract_trajectory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryConfig {
    pub recent_window: u8,
    pub stall_min_tool_results: u32,
    /// Substrings that mark a context-compaction summary.
    ///
    /// Empty by default and therefore inert: no marker is common to every
    /// harness, and guessing one would silently pin unrelated conversations to
    /// an expensive tier. Operators set the header their harness emits.
    pub compaction_markers: Vec<String>,
}

impl Default for TrajectoryConfig {
    fn default() -> Self {
        Self {
            recent_window: DEFAULT_RECENT_WINDOW,
            stall_min_tool_results: DEFAULT_STALL_MIN_TOOL_RESULTS,
            compaction_markers: Vec::new(),
        }
    }
}

/// What the request says about how the run is going.
///
/// The booleans are separate fields rather than a state enum because they are
/// not mutually exclusive as a set: `tests_passed` and `compacted` can each hold
/// alongside either stall flag. Only `spinning` and `exploring` partition, and
/// that is enforced where they are derived.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrajectorySignals {
    /// Tool-result payloads in the request. See [`extract_trajectory`] for why
    /// this is not a message count.
    pub tool_result_count: u32,
    /// Worst failure severity across the recent window, in millis.
    pub severity_millis: u16,
    /// Clean tool results counting back from the newest. `0` if the last failed.
    pub no_error_streak: u32,
    /// Deep into a run, producing nothing, and not investigating either.
    pub spinning: bool,
    /// Deep into a run, producing nothing, but reading and planning.
    pub exploring: bool,
    /// Share of recent tool calls that wrote or edited, in millis.
    pub production_intensity_millis: u16,
    /// A recent tool result looks like a passing test run.
    pub tests_passed: bool,
    /// The context carries a compaction summary.
    pub compacted: bool,
}

impl TrajectorySignals {
    /// Whether anything at all was observed. A single-turn chat produces no
    /// trajectory, and an all-zero signal set must not be read as "healthy".
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tool_result_count == 0
    }

    /// The signal kinds this trajectory contributes, in the wire vocabulary
    /// already accepted from callers in `urouter.signals[]`.
    ///
    /// Only non-zero strengths are emitted: a zero would be recorded as an
    /// inferred signal that says nothing, making the trace harder to read
    /// without changing any decision.
    #[must_use]
    pub fn signal_strengths(&self) -> Vec<TrajectorySignalStrength> {
        let mut out = Vec::new();
        let mut push = |kind: &'static str, millis: u16| {
            if millis > 0 {
                out.push(TrajectorySignalStrength {
                    kind,
                    strength_millis: millis,
                    origin: SignalOrigin::Inferred,
                });
            }
        };
        push("severity", self.severity_millis);
        push("spinning", u16::from(self.spinning) * 1_000);
        push("exploring", u16::from(self.exploring) * 1_000);
        push("production_intensity", self.production_intensity_millis);
        out
    }
}

/// Reads the trajectory out of an OpenAI-chat shaped request.
///
/// Pure: no clock, no randomness, no I/O. Every inbound protocol is normalised
/// to this shape before routing, so one parser covers all of them.
///
/// # Counting tool results
///
/// A tool result is identified by the message carrying a `tool_call_id`, not by
/// `role == "tool"`, and each content part counts as one payload. Both details
/// are load-bearing for multi-protocol parity:
///
/// - Anthropic returns tool results inside a **user** message, and the
///   normalisation keeps that role. Counting `role == "tool"` would score every
///   Anthropic trajectory as zero.
/// - Anthropic also batches several tool results into one message, while
///   OpenAI-chat sends one message each. Counting messages would score the same
///   logical trajectory 1 against 3.
///
/// Keying on `tool_call_id` and summing parts makes both paths agree, which is
/// what stops the client's choice of wire protocol from changing its route.
#[must_use]
pub fn extract_trajectory(request: &Value, config: &TrajectoryConfig) -> TrajectorySignals {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return TrajectorySignals::default();
    };

    // Tool-result payload texts, oldest first.
    let mut results: Vec<String> = Vec::new();
    // Tool call names, oldest first, aligned with nothing in particular: they
    // are windowed by count, like the results.
    let mut calls: Vec<String> = Vec::new();
    let mut compacted = false;

    for message in messages {
        for marker in &config.compaction_markers {
            if !marker.is_empty() && message_text(message).contains(marker.as_str()) {
                compacted = true;
            }
        }
        for call in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .or_else(|| call.get("name").and_then(Value::as_str))
            {
                calls.push(name.to_ascii_lowercase());
            }
        }
        if !carries_tool_result(message) {
            continue;
        }
        results.extend(tool_result_payloads(message));
    }

    let window = usize::from(config.recent_window).max(1);
    let recent_results = results.iter().rev().take(window);
    let severity_millis = recent_results
        .map(|text| classify_severity(text))
        .max()
        .unwrap_or(0);
    let no_error_streak = results
        .iter()
        .rev()
        .take_while(|text| classify_severity(text) == 0)
        .count();
    let tests_passed = results
        .iter()
        .rev()
        .take(window)
        .any(|text| matches_any(text, TEST_PASS_PATTERNS));

    let recent_calls: Vec<&String> = calls.iter().rev().take(window).collect();
    let recent_write = count_matching(&recent_calls, WRITE_TOOL_NAMES);
    let recent_edit = count_matching(&recent_calls, EDIT_TOOL_NAMES);
    let recent_read = count_matching(&recent_calls, READ_TOOL_NAMES);
    let recent_plan = count_matching(&recent_calls, PLAN_TOOL_NAMES);

    let tool_result_count = u32::try_from(results.len()).unwrap_or(u32::MAX);
    let deep_enough = tool_result_count >= config.stall_min_tool_results;
    let no_production = recent_write == 0 && recent_edit == 0;
    let investigating = recent_read > 0 || recent_plan > 0;
    // Partitioned so at most one fires. Both push towards a stronger tier, and
    // letting them both fire would double-count one situation.
    let spinning = deep_enough && no_production && !investigating;
    let exploring = deep_enough && no_production && investigating;

    let produced = recent_write + recent_edit;
    let classified = produced + recent_read + recent_plan;
    let production_intensity_millis = (produced * 1_000)
        .checked_div(classified)
        .and_then(|ratio| u16::try_from(ratio).ok())
        .unwrap_or(0);

    TrajectorySignals {
        tool_result_count,
        severity_millis,
        no_error_streak: u32::try_from(no_error_streak).unwrap_or(u32::MAX),
        spinning,
        exploring,
        production_intensity_millis,
        tests_passed,
        compacted,
    }
}

/// A message carries a tool result when it names the call it answers.
fn carries_tool_result(message: &Value) -> bool {
    message
        .get("tool_call_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

/// One payload per content part, lower-cased and length-capped for scanning.
fn tool_result_payloads(message: &Value) -> Vec<String> {
    match message.get("content") {
        Some(Value::String(text)) => vec![scan_text(text)],
        Some(Value::Array(parts)) if !parts.is_empty() => {
            parts.iter().map(|part| scan_text(&part_text(part))).collect()
        }
        // A tool result with no content is still a tool result: the call
        // happened, and dropping it would understate the depth of the run.
        _ => vec![String::new()],
    }
}

fn part_text(part: &Value) -> String {
    match part {
        Value::String(text) => text.clone(),
        Value::Object(_) => part
            .get("text")
            .and_then(Value::as_str)
            .map_or_else(|| part.to_string(), str::to_owned),
        other => other.to_string(),
    }
}

/// Whole-message text, used only for compaction markers.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => scan_text(text),
        Some(Value::Array(parts)) => scan_text(
            &parts
                .iter()
                .map(part_text)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => String::new(),
    }
}

/// Lower-case and cap. Truncation is on a character boundary so the result is
/// always valid UTF-8, which matters for CJK payloads.
fn scan_text(text: &str) -> String {
    let mut end = text.len().min(MAX_RESULT_SCAN_BYTES);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_ascii_lowercase()
}

fn classify_severity(text: &str) -> u16 {
    ERROR_PATTERNS
        .iter()
        .filter(|(_, needles)| matches_any(text, needles))
        .map(|(severity, _)| *severity)
        .max()
        .unwrap_or(0)
}

fn matches_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// Counts calls whose name contains one of `names`.
///
/// Substring rather than equality because harnesses namespace their tools
/// (`mcp__fs__read_file`, `functions.write`), and an exact match would score
/// every namespaced trajectory as unclassified.
fn count_matching(calls: &[&String], names: &[&str]) -> u32 {
    u32::try_from(
        calls
            .iter()
            .filter(|call| names.iter().any(|name| call.contains(name)))
            .count(),
    )
    .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn extract(request: &Value) -> TrajectorySignals {
        extract_trajectory(request, &TrajectoryConfig::default())
    }

    /// The same logical trajectory sent over OpenAI-chat and over Anthropic
    /// must produce identical signals.
    ///
    /// The two wire formats disagree twice: Anthropic returns tool results in a
    /// **user** message rather than a tool one, and batches several results into
    /// a single message. Either difference alone would make a client's choice of
    /// protocol change its route.
    #[test]
    fn the_same_trajectory_scores_identically_across_wire_protocols() {
        let assistant = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "c3", "type": "function", "function": {"name": "Read", "arguments": "{}"}}
            ]
        });

        // OpenAI-chat: one message per result, role "tool", content a string.
        let openai = json!({"messages": [
            {"role": "user", "content": "go"},
            assistant,
            {"role": "tool", "tool_call_id": "c1", "content": "ok"},
            {"role": "tool", "tool_call_id": "c2", "content": "Traceback (most recent call last)"},
            {"role": "tool", "tool_call_id": "c3", "content": "ok"}
        ]});

        // Anthropic after normalisation: one user message, three content parts,
        // and only the first tool_use_id survives the conversion.
        let anthropic = json!({"messages": [
            {"role": "user", "content": "go"},
            assistant,
            {"role": "user", "tool_call_id": "c1", "content": [
                {"type": "text", "text": "ok"},
                {"type": "text", "text": "Traceback (most recent call last)"},
                {"type": "text", "text": "ok"}
            ]}
        ]});

        let from_openai = extract(&openai);
        let from_anthropic = extract(&anthropic);
        assert_eq!(
            from_openai, from_anthropic,
            "wire protocol changed the trajectory signals"
        );
        assert_eq!(from_openai.tool_result_count, 3);
        assert_eq!(from_openai.severity_millis, SEVERITY_HARD);
    }

    /// Counting `role == "tool"` would score the Anthropic shape as zero. This
    /// pins the reason the code keys on `tool_call_id` instead.
    #[test]
    fn tool_results_are_found_by_call_id_not_by_role() {
        let signals = extract(&json!({"messages": [
            {"role": "user", "tool_call_id": "c1", "content": "ok"}
        ]}));
        assert_eq!(signals.tool_result_count, 1);

        // A tool-role message without an id is not a result carrier.
        let signals = extract(&json!({"messages": [{"role": "tool", "content": "ok"}]}));
        assert_eq!(signals.tool_result_count, 0);
        assert!(signals.is_empty());
    }

    #[test]
    fn severity_is_the_window_maximum_and_decays_out_of_it() {
        let request = json!({"messages": [
            {"role": "tool", "tool_call_id": "a", "content": "AssertionError: boom"},
            {"role": "tool", "tool_call_id": "b", "content": "ok"},
            {"role": "tool", "tool_call_id": "c", "content": "ok"}
        ]});
        // A window of three still sees the error: it persists through the turns
        // spent recovering from it, instead of clearing on the next clean call.
        assert_eq!(extract(&request).severity_millis, SEVERITY_HARD);
        let narrow = TrajectoryConfig {
            recent_window: 1,
            ..TrajectoryConfig::default()
        };
        assert_eq!(extract_trajectory(&request, &narrow).severity_millis, 0);
    }

    #[test]
    fn severity_takes_the_worst_of_several_matching_patterns() {
        let signals = extract(&json!({"messages": [{
            "role": "tool", "tool_call_id": "a",
            "content": "exit code 1\nTraceback (most recent call last)"
        }]}));
        assert_eq!(signals.severity_millis, SEVERITY_HARD);
    }

    /// The short form fires on ordinary `ls` output and on prose.
    #[test]
    fn missing_file_matching_is_anchored() {
        let benign = extract(&json!({"messages": [{
            "role": "tool", "tool_call_id": "a",
            "content": "the directory does not exist in the listing above"
        }]}));
        assert_eq!(benign.severity_millis, 0);

        let real = extract(&json!({"messages": [{
            "role": "tool", "tool_call_id": "a",
            "content": "Error: File does not exist."
        }]}));
        assert_eq!(real.severity_millis, SEVERITY_HARD);
    }

    #[test]
    fn spinning_and_exploring_are_mutually_exclusive() {
        let deep = |tool: &str| {
            let mut messages = vec![json!({
                "role": "assistant", "content": "",
                "tool_calls": [{"id": "x", "type": "function",
                                "function": {"name": tool, "arguments": "{}"}}]
            })];
            for index in 0..8 {
                messages.push(json!({
                    "role": "tool", "tool_call_id": format!("c{index}"), "content": "ok"
                }));
            }
            extract(&json!({"messages": messages}))
        };

        let stuck = deep("Bash");
        assert!(stuck.spinning && !stuck.exploring);

        let searching = deep("Read");
        assert!(searching.exploring && !searching.spinning);

        let producing = deep("Write");
        assert!(!producing.spinning && !producing.exploring);
        assert_eq!(producing.production_intensity_millis, 1_000);
    }

    /// A shallow run that happens not to be producing is not stalled.
    #[test]
    fn stall_detection_requires_depth() {
        let signals = extract(&json!({"messages": [
            {"role": "assistant", "content": "",
             "tool_calls": [{"id": "x", "type": "function",
                             "function": {"name": "Bash", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"}
        ]}));
        assert!(!signals.spinning && !signals.exploring);
    }

    /// Harnesses namespace their tools; exact-name matching would classify
    /// every namespaced call as unclassified.
    #[test]
    fn namespaced_tool_names_are_classified() {
        let signals = extract(&json!({"messages": [
            {"role": "assistant", "content": "",
             "tool_calls": [{"id": "x", "type": "function",
                             "function": {"name": "mcp__fs__write_file", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"}
        ]}));
        assert_eq!(signals.production_intensity_millis, 1_000);
    }

    #[test]
    fn no_error_streak_counts_back_from_the_newest_result() {
        let signals = extract(&json!({"messages": [
            {"role": "tool", "tool_call_id": "a", "content": "AssertionError"},
            {"role": "tool", "tool_call_id": "b", "content": "ok"},
            {"role": "tool", "tool_call_id": "c", "content": "ok"}
        ]}));
        assert_eq!(signals.no_error_streak, 2);

        let signals = extract(&json!({"messages": [
            {"role": "tool", "tool_call_id": "a", "content": "ok"},
            {"role": "tool", "tool_call_id": "b", "content": "AssertionError"}
        ]}));
        assert_eq!(signals.no_error_streak, 0);
    }

    #[test]
    fn tests_passed_is_recognised_only_from_recent_results() {
        let signals = extract(&json!({"messages": [
            {"role": "tool", "tool_call_id": "a", "content": "test result: ok. 42 passed"}
        ]}));
        assert!(signals.tests_passed);

        let signals = extract(&json!({"messages": [
            {"role": "tool", "tool_call_id": "a", "content": "i will now run the tests"}
        ]}));
        assert!(!signals.tests_passed);
    }

    /// No marker is shipped, because none is common to every harness and a
    /// wrong guess pins unrelated conversations to an expensive tier.
    #[test]
    fn compaction_is_inert_until_a_marker_is_configured() {
        let request = json!({"messages": [
            {"role": "user", "content": "<conversation-summary>earlier turns</conversation-summary>"},
            {"role": "tool", "tool_call_id": "a", "content": "ok"}
        ]});
        assert!(!extract(&request).compacted);

        let configured = TrajectoryConfig {
            compaction_markers: vec!["<conversation-summary>".to_owned()],
            ..TrajectoryConfig::default()
        };
        assert!(extract_trajectory(&request, &configured).compacted);
    }

    /// A single-turn chat has no trajectory. An all-zero signal set must not be
    /// mistaken for evidence that the run is healthy.
    #[test]
    fn a_request_without_tool_activity_produces_nothing() {
        let signals = extract(&json!({"messages": [{"role": "user", "content": "hello"}]}));
        assert!(signals.is_empty());
        assert!(signals.signal_strengths().is_empty());

        assert!(extract(&json!({})).is_empty());
        assert!(extract(&json!({"messages": "not an array"})).is_empty());
    }

    /// Truncation must land on a character boundary or the scan panics on any
    /// CJK payload longer than the cap.
    #[test]
    fn oversized_multibyte_payloads_are_truncated_safely() {
        let payload = "中".repeat(MAX_RESULT_SCAN_BYTES);
        let signals = extract(&json!({"messages": [
            {"role": "tool", "tool_call_id": "a", "content": payload}
        ]}));
        assert_eq!(signals.tool_result_count, 1);
        assert_eq!(signals.severity_millis, 0);
    }

    #[test]
    fn only_non_zero_strengths_are_emitted() {
        let signals = TrajectorySignals {
            tool_result_count: 1,
            severity_millis: 700,
            production_intensity_millis: 0,
            ..TrajectorySignals::default()
        };
        let emitted = signals.signal_strengths();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].kind, "severity");
        assert_eq!(emitted[0].strength_millis, 700);
        assert_eq!(emitted[0].origin, SignalOrigin::Inferred);
    }
}
