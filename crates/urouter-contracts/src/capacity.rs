use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentCandidate {
    pub id: String,
    pub order: u16,
    pub weight: u32,
    pub available: bool,
    pub unavailable_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentDisposition {
    Selected,
    RunnerUp,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentEvaluation {
    pub deployment: String,
    pub disposition: DeploymentDisposition,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentSelection {
    pub selected: String,
    pub runners_up: Vec<String>,
    pub evaluations: Vec<DeploymentEvaluation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalCircuitAvailability {
    Closed,
    Open,
    HalfOpen,
    HalfOpenProbeInFlight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityCandidateSnapshot {
    pub id: String,
    pub order: u16,
    pub weight: u32,
    pub retry_excluded: bool,
    pub circuit: LocalCircuitAvailability,
    pub in_flight: u64,
    pub latency_ewma_ms: Option<u64>,
    pub quota_usage_millis: Option<u16>,
    #[serde(default)]
    pub unavailable_reasons: Vec<String>,
}

pub const CAPACITY_SNAPSHOT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorRef {
    pub shard: String,
    pub offset_bytes: u64,
    pub dimensions: u32,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacitySnapshot {
    pub schema_version: u16,
    pub candidates: Vec<CapacityCandidateSnapshot>,
}

impl Default for CapacitySnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl CapacitySnapshot {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            schema_version: CAPACITY_SNAPSHOT_SCHEMA_VERSION,
            candidates: Vec::new(),
        }
    }

    #[must_use]
    pub const fn new(candidates: Vec<CapacityCandidateSnapshot>) -> Self {
        Self {
            schema_version: CAPACITY_SNAPSHOT_SCHEMA_VERSION,
            candidates,
        }
    }

    #[must_use]
    pub fn candidate(&self, deployment: &str) -> Option<&CapacityCandidateSnapshot> {
        self.candidates
            .iter()
            .find(|candidate| candidate.id == deployment)
    }

    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.schema_version == CAPACITY_SNAPSHOT_SCHEMA_VERSION
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentPicker {
    #[default]
    Weighted,
    LeastLoaded,
    LowestLatency,
    LowestQuotaUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityLeasePlan {
    pub selection: DeploymentSelection,
    pub reserve_half_open_probe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityLeasePlanError {
    Empty,
    Exhausted(Vec<DeploymentEvaluation>),
    ZeroWeight,
}

pub fn plan_capacity_lease(
    candidates: &[CapacityCandidateSnapshot],
    ticket: u64,
) -> Result<CapacityLeasePlan, CapacityLeasePlanError> {
    plan_capacity_lease_with_picker(candidates, ticket, DeploymentPicker::Weighted)
}

pub fn plan_capacity_lease_with_picker(
    candidates: &[CapacityCandidateSnapshot],
    ticket: u64,
    picker: DeploymentPicker,
) -> Result<CapacityLeasePlan, CapacityLeasePlanError> {
    if candidates.is_empty() {
        return Err(CapacityLeasePlanError::Empty);
    }
    let mut policy_candidates = candidates
        .iter()
        .map(|candidate| {
            let mut unavailable_reasons = candidate.unavailable_reasons.clone();
            if candidate.retry_excluded {
                unavailable_reasons.push("retry_excluded".to_owned());
            }
            if matches!(
                candidate.circuit,
                LocalCircuitAvailability::Open | LocalCircuitAvailability::HalfOpenProbeInFlight
            ) {
                unavailable_reasons.push("local_circuit_unavailable".to_owned());
            }
            DeploymentCandidate {
                id: candidate.id.clone(),
                order: candidate.order,
                weight: candidate.weight,
                available: unavailable_reasons.is_empty(),
                unavailable_reasons,
            }
        })
        .collect::<Vec<_>>();
    if !policy_candidates
        .iter()
        .any(|candidate| candidate.available)
    {
        return Err(CapacityLeasePlanError::Exhausted(
            policy_candidates
                .into_iter()
                .map(|candidate| DeploymentEvaluation {
                    deployment: candidate.id,
                    disposition: DeploymentDisposition::Excluded,
                    reasons: candidate.unavailable_reasons,
                })
                .collect(),
        ));
    }
    apply_picker(&mut policy_candidates, candidates, picker);
    let selection =
        select_weighted_deployment(&policy_candidates, ticket).map_err(|error| match error {
            DeploymentSelectionError::Empty => CapacityLeasePlanError::Empty,
            DeploymentSelectionError::Exhausted => CapacityLeasePlanError::Exhausted(Vec::new()),
            DeploymentSelectionError::ZeroWeight => CapacityLeasePlanError::ZeroWeight,
        })?;
    let reserve_half_open_probe = candidates.iter().any(|candidate| {
        candidate.id == selection.selected
            && candidate.circuit == LocalCircuitAvailability::HalfOpen
    });
    Ok(CapacityLeasePlan {
        selection,
        reserve_half_open_probe,
    })
}

fn apply_picker(
    policy: &mut [DeploymentCandidate],
    snapshots: &[CapacityCandidateSnapshot],
    picker: DeploymentPicker,
) {
    let minimum_order = policy
        .iter()
        .filter(|candidate| candidate.available)
        .map(|candidate| candidate.order)
        .min();
    let Some(minimum_order) = minimum_order else {
        return;
    };
    let metric = |snapshot: &CapacityCandidateSnapshot| match picker {
        DeploymentPicker::Weighted => None,
        DeploymentPicker::LeastLoaded => Some(snapshot.in_flight),
        DeploymentPicker::LowestLatency => snapshot.latency_ewma_ms,
        DeploymentPicker::LowestQuotaUsage => snapshot.quota_usage_millis.map(u64::from),
    };
    let best = snapshots
        .iter()
        .filter(|snapshot| {
            policy.iter().any(|candidate| {
                candidate.id == snapshot.id
                    && candidate.available
                    && candidate.order == minimum_order
            })
        })
        .filter_map(&metric)
        .min();
    let Some(best) = best else {
        return;
    };
    let reason = match picker {
        DeploymentPicker::Weighted => return,
        DeploymentPicker::LeastLoaded => "higher_load",
        DeploymentPicker::LowestLatency => "higher_latency",
        DeploymentPicker::LowestQuotaUsage => "higher_quota_usage",
    };
    for candidate in policy
        .iter_mut()
        .filter(|candidate| candidate.available && candidate.order == minimum_order)
    {
        let candidate_metric = snapshots
            .iter()
            .find(|snapshot| snapshot.id == candidate.id)
            .and_then(metric);
        if candidate_metric.is_some_and(|value| value > best) {
            candidate.available = false;
            candidate.unavailable_reasons.push(reason.to_owned());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentSelectionError {
    Empty,
    Exhausted,
    ZeroWeight,
}


pub fn select_weighted_deployment(
    candidates: &[DeploymentCandidate],
    ticket: u64,
) -> Result<DeploymentSelection, DeploymentSelectionError> {
    if candidates.is_empty() {
        return Err(DeploymentSelectionError::Empty);
    }
    let minimum_order = candidates
        .iter()
        .filter(|candidate| candidate.available)
        .map(|candidate| candidate.order)
        .min()
        .ok_or(DeploymentSelectionError::Exhausted)?;
    let mut eligible = candidates
        .iter()
        .filter(|candidate| candidate.available && candidate.order == minimum_order)
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| left.id.cmp(&right.id));
    let total_weight = eligible.iter().try_fold(0_u64, |sum, candidate| {
        sum.checked_add(u64::from(candidate.weight))
    });
    let total_weight = total_weight
        .filter(|weight| *weight > 0)
        .ok_or(DeploymentSelectionError::ZeroWeight)?;
    let mut position = ticket % total_weight;
    let selected = eligible
        .iter()
        .find(|candidate| {
            if position < u64::from(candidate.weight) {
                true
            } else {
                position -= u64::from(candidate.weight);
                false
            }
        })
        .ok_or(DeploymentSelectionError::ZeroWeight)?;
    let selected = selected.id.clone();
    let evaluations = candidates
        .iter()
        .map(|candidate| {
            let (disposition, reasons) = if !candidate.available {
                (
                    DeploymentDisposition::Excluded,
                    if candidate.unavailable_reasons.is_empty() {
                        vec!["unavailable".to_owned()]
                    } else {
                        candidate.unavailable_reasons.clone()
                    },
                )
            } else if candidate.order != minimum_order {
                (
                    DeploymentDisposition::Excluded,
                    vec!["lower_priority_order".to_owned()],
                )
            } else if candidate.id == selected {
                (DeploymentDisposition::Selected, Vec::new())
            } else {
                (DeploymentDisposition::RunnerUp, Vec::new())
            };
            DeploymentEvaluation {
                deployment: candidate.id.clone(),
                disposition,
                reasons,
            }
        })
        .collect();
    Ok(DeploymentSelection {
        selected: selected.clone(),
        runners_up: eligible
            .iter()
            .filter(|candidate| candidate.id != selected)
            .map(|candidate| candidate.id.clone())
            .collect(),
        evaluations,
    })
}
