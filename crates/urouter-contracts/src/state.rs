use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedStateDomain {
    Budget,
    Quota,
    Binding,
    Circuit,
    DecisionRecord,
    Metrics,
    LatencySignal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendFailurePolicy {
    FailClosed,
    FailOpen,
}

#[must_use]
pub const fn shared_state_failure_policy(domain: SharedStateDomain) -> BackendFailurePolicy {
    match domain {
        SharedStateDomain::Budget
        | SharedStateDomain::Quota
        | SharedStateDomain::Binding
        | SharedStateDomain::Circuit
        | SharedStateDomain::DecisionRecord => BackendFailurePolicy::FailClosed,
        SharedStateDomain::Metrics | SharedStateDomain::LatencySignal => {
            BackendFailurePolicy::FailOpen
        }
    }
}
