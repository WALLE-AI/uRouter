use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DeploymentEvaluation, RuleEvaluation};

pub const FEATURE_SCHEMA_VERSION: u16 = 1;
pub const TRACE_SCHEMA_VERSION: u16 = 1;
pub const DECISION_RECORD_SCHEMA_VERSION: u16 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureFrame {
    pub schema_version: u16,
    pub message_count: u32,
    pub input_text_bytes: u64,
    pub available_tool_count: u32,
    pub content: ContentFeatures,
    pub requests: RequestFeatures,
    pub requested_max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentFeatures {
    pub has_tool_result: bool,
    pub contains_image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestFeatures {
    pub structured_output: bool,
    pub reasoning: bool,
}

impl FeatureFrame {
    #[must_use]
    pub fn from_openai_chat(request: &Value) -> Self {
        let messages = request
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut input_text_bytes = 0_u64;
        let mut contains_image = false;
        let mut has_tool_result = false;
        for message in messages {
            has_tool_result |= message.get("role").and_then(Value::as_str) == Some("tool");
            if let Some(content) = message.get("content") {
                collect_content_features(content, &mut input_text_bytes, &mut contains_image);
            }
        }

        let available_tool_count = request
            .get("tools")
            .and_then(Value::as_array)
            .map_or(0, |tools| saturating_u32(tools.len()));
        let requests_structured_output = request
            .pointer("/response_format/type")
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "json_object" | "json_schema"));
        let requests_reasoning = request
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .is_some_and(|effort| !effort.eq_ignore_ascii_case("none"));
        let requested_max_output_tokens = request
            .get("max_completion_tokens")
            .or_else(|| request.get("max_output_tokens"))
            .or_else(|| request.get("max_tokens"))
            .and_then(Value::as_u64);

        Self {
            schema_version: FEATURE_SCHEMA_VERSION,
            message_count: saturating_u32(messages.len()),
            input_text_bytes,
            available_tool_count,
            content: ContentFeatures {
                has_tool_result,
                contains_image,
            },
            requests: RequestFeatures {
                structured_output: requests_structured_output,
                reasoning: requests_reasoning,
            },
            requested_max_output_tokens,
        }
    }
}

fn collect_content_features(content: &Value, text_bytes: &mut u64, contains_image: &mut bool) {
    match content {
        Value::String(text) => {
            *text_bytes = text_bytes.saturating_add(text.len() as u64);
        }
        Value::Array(parts) => {
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str);
                *contains_image |= matches!(kind, Some("image_url" | "input_image" | "image"));
                if matches!(kind, Some("text" | "input_text"))
                    && let Some(text) = part.get("text").and_then(Value::as_str)
                {
                    *text_bytes = text_bytes.saturating_add(text.len() as u64);
                }
            }
        }
        _ => {}
    }
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceCompleteness {
    Summary,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateDisposition {
    Selected,
    Eligible,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateTrace {
    pub candidate: String,
    pub disposition: CandidateDisposition,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingTrace {
    pub schema_version: u16,
    pub completeness: TraceCompleteness,
    pub policy: String,
    pub reason: String,
    #[serde(default)]
    pub decisions: Vec<RuleEvaluation>,
    #[serde(default)]
    pub runtime_filters: Vec<DeploymentEvaluation>,
    #[serde(default)]
    pub policy_filters: Vec<RuleEvaluation>,
    pub candidates: Vec<CandidateTrace>,
}

impl RoutingTrace {
    #[must_use]
    pub fn current_summary(
        selected: impl Into<String>,
        alternatives: impl IntoIterator<Item = String>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        let mut candidates = vec![CandidateTrace {
            candidate: selected.into(),
            disposition: CandidateDisposition::Selected,
            reasons: vec![reason.clone()],
        }];
        candidates.extend(alternatives.into_iter().map(|candidate| CandidateTrace {
            candidate,
            disposition: CandidateDisposition::Eligible,
            reasons: Vec::new(),
        }));
        Self {
            schema_version: TRACE_SCHEMA_VERSION,
            completeness: TraceCompleteness::Summary,
            policy: "current_gateway".to_owned(),
            reason,
            decisions: Vec::new(),
            runtime_filters: Vec::new(),
            policy_filters: Vec::new(),
            candidates,
        }
    }

    #[must_use]
    pub fn with_decisions(mut self, decisions: Vec<RuleEvaluation>) -> Self {
        self.decisions = decisions;
        self
    }

    #[must_use]
    pub fn with_runtime_filters(mut self, runtime_filters: Vec<DeploymentEvaluation>) -> Self {
        self.runtime_filters = runtime_filters;
        self
    }

    #[must_use]
    pub fn with_policy_filters(mut self, policy_filters: Vec<RuleEvaluation>) -> Self {
        self.policy_filters = policy_filters;
        self.completeness = TraceCompleteness::Full;
        self
    }

    #[must_use]
    pub fn current_admission_summary(
        selected: impl Into<String>,
        alternatives: impl IntoIterator<Item = String>,
        excluded: impl IntoIterator<Item = (String, Vec<String>)>,
        reason: impl Into<String>,
    ) -> Self {
        let mut trace = Self::current_summary(selected, alternatives, reason);
        trace.candidates.extend(
            excluded
                .into_iter()
                .map(|(candidate, reasons)| CandidateTrace {
                    candidate,
                    disposition: CandidateDisposition::Excluded,
                    reasons,
                }),
        );
        trace
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionSet {
    pub catalog: String,
    pub route: String,
    pub feature_schema: u16,
    pub policy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecordContext {
    pub schema_version: u16,
    pub request_id: String,
    pub revisions: RevisionSet,
    pub features: FeatureFrame,
    pub trace: RoutingTrace,
    pub eligible_candidates: Vec<String>,
    pub propensity_millionths: Option<u32>,
}

impl DecisionRecordContext {
    #[must_use]
    pub fn deterministic(
        request_id: impl Into<String>,
        revisions: RevisionSet,
        features: FeatureFrame,
        trace: RoutingTrace,
        eligible_candidates: Vec<String>,
    ) -> Self {
        Self {
            schema_version: DECISION_RECORD_SCHEMA_VERSION,
            request_id: request_id.into(),
            revisions,
            features,
            trace,
            eligible_candidates,
            propensity_millionths: None,
        }
    }

    #[must_use]
    pub fn training_complete(&self) -> bool {
        self.schema_version == DECISION_RECORD_SCHEMA_VERSION
            && !self.request_id.is_empty()
            && !self.revisions.catalog.is_empty()
            && !self.revisions.route.is_empty()
            && !self.revisions.policy.is_empty()
            && !self.eligible_candidates.is_empty()
    }
}
