use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use thiserror::Error;
use urouter_contracts::{
    CapacityCandidateSnapshot, CapacityLeasePlanError, CooldownDirective, DeploymentEvaluation,
    DeploymentPicker, FailureWindow, LocalCircuitAvailability, cooldown_directive,
    plan_capacity_lease_with_picker,
};

use crate::{RouteDeployment, UpstreamErrorKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CooldownPolicy {
    pub cooldown: Duration,
    pub window: Duration,
    pub failure_threshold_millis: u16,
}

impl Default for CooldownPolicy {
    fn default() -> Self {
        Self {
            cooldown: Duration::from_secs(5),
            window: Duration::from_secs(60),
            failure_threshold_millis: 500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentHealthSnapshot {
    pub deployment: String,
    pub state: CircuitState,
    pub successes: u64,
    pub failures: u64,
    pub in_flight: u64,
    pub cooldown_remaining_ms: u128,
}

#[derive(Debug, Error)]
pub enum CapacityError {
    #[error("tier has no configured deployments")]
    EmptyTier,
    #[error("all deployments are cooling or excluded")]
    Exhausted(Vec<DeploymentEvaluation>),
}

#[derive(Debug, Default)]
struct DeploymentHealth {
    events: VecDeque<(Instant, bool)>,
    cooldown_until: Option<Instant>,
    half_open_probe: bool,
    in_flight: u64,
    latency_ewma_ms: Option<u64>,
}

#[derive(Debug)]
pub struct CapacityManager {
    health: Mutex<BTreeMap<String, DeploymentHealth>>,
    ticket: AtomicU64,
    policy: CooldownPolicy,
    picker: DeploymentPicker,
}

impl CapacityManager {
    #[must_use]
    pub fn new(policy: CooldownPolicy) -> Arc<Self> {
        Arc::new(Self {
            health: Mutex::new(BTreeMap::new()),
            ticket: AtomicU64::new(0),
            policy,
            picker: DeploymentPicker::Weighted,
        })
    }

    #[must_use]
    pub fn with_picker(policy: CooldownPolicy, picker: DeploymentPicker) -> Arc<Self> {
        Arc::new(Self {
            health: Mutex::new(BTreeMap::new()),
            ticket: AtomicU64::new(0),
            policy,
            picker,
        })
    }

    pub fn select(
        self: &Arc<Self>,
        candidates: &[RouteDeployment],
        excluded: &BTreeSet<String>,
    ) -> Result<CapacityLease, CapacityError> {
        if candidates.is_empty() {
            return Err(CapacityError::EmptyTier);
        }
        let now = Instant::now();
        let mut health = self.health.lock().expect("capacity lock poisoned");
        let mut snapshots = Vec::with_capacity(candidates.len());
        for deployment in candidates {
            let state = health.entry(deployment.id.clone()).or_default();
            prune_events(state, now, self.policy.window);
            snapshots.push(CapacityCandidateSnapshot {
                id: deployment.id.clone(),
                order: deployment.order,
                weight: deployment.weight,
                retry_excluded: excluded.contains(&deployment.id),
                circuit: local_circuit_availability(state, now),
                in_flight: state.in_flight,
                latency_ewma_ms: state.latency_ewma_ms,
                quota_usage_millis: deployment.quota_usage_millis,
            });
        }
        let plan = plan_capacity_lease_with_picker(
            &snapshots,
            self.ticket.fetch_add(1, Ordering::Relaxed),
            self.picker,
        )
        .map_err(|error| match error {
            CapacityLeasePlanError::Empty => CapacityError::EmptyTier,
            CapacityLeasePlanError::Exhausted(evaluations) => CapacityError::Exhausted(evaluations),
            CapacityLeasePlanError::ZeroWeight => CapacityError::Exhausted(Vec::new()),
        })?;
        let selection = plan.selection;
        let selected = candidates
            .iter()
            .find(|deployment| deployment.id == selection.selected)
            .expect("policy selection references an input deployment");
        let selected_health = health
            .get_mut(&selected.id)
            .expect("eligible deployment health exists");
        if plan.reserve_half_open_probe {
            selected_health.half_open_probe = true;
        }
        selected_health.in_flight = selected_health.in_flight.saturating_add(1);
        let runners_up = selection
            .runners_up
            .iter()
            .map(|id| {
                candidates
                    .iter()
                    .find(|deployment| deployment.id == *id)
                    .expect("policy runner-up references an input deployment")
                    .clone()
            })
            .collect();
        Ok(CapacityLease {
            manager: Arc::clone(self),
            deployment: (*selected).clone(),
            runners_up,
            evaluations: selection.evaluations,
            tier_size: candidates.len(),
            completed: false,
        })
    }

    pub fn register(&self, deployments: &[RouteDeployment]) {
        let mut health = self.health.lock().expect("capacity lock poisoned");
        for deployment in deployments {
            health.entry(deployment.id.clone()).or_default();
        }
    }

    #[must_use]
    pub const fn picker(&self) -> DeploymentPicker {
        self.picker
    }

    pub fn observe_latency(&self, deployment: &str, latency_ms: u64) {
        let mut health = self.health.lock().expect("capacity lock poisoned");
        let state = health.entry(deployment.to_owned()).or_default();
        state.latency_ewma_ms = Some(state.latency_ewma_ms.map_or(latency_ms, |previous| {
            previous.saturating_mul(4).saturating_add(latency_ms) / 5
        }));
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<DeploymentHealthSnapshot> {
        let now = Instant::now();
        let mut health = self.health.lock().expect("capacity lock poisoned");
        health
            .iter_mut()
            .map(|(deployment, state)| {
                prune_events(state, now, self.policy.window);
                let (successes, failures) = event_counts(state);
                let circuit = match state.cooldown_until {
                    Some(until) if until > now => CircuitState::Open,
                    Some(_) => CircuitState::HalfOpen,
                    None => CircuitState::Closed,
                };
                DeploymentHealthSnapshot {
                    deployment: deployment.clone(),
                    state: circuit,
                    successes,
                    failures,
                    in_flight: state.in_flight,
                    cooldown_remaining_ms: state
                        .cooldown_until
                        .and_then(|until| until.checked_duration_since(now))
                        .map_or(0, |duration| duration.as_millis()),
                }
            })
            .collect()
    }

    fn finish(&self, deployment: &str, tier_size: usize, result: Result<(), UpstreamErrorKind>) {
        let now = Instant::now();
        let mut health = self.health.lock().expect("capacity lock poisoned");
        let state = health.entry(deployment.to_owned()).or_default();
        state.in_flight = state.in_flight.saturating_sub(1);
        state.half_open_probe = false;
        match result {
            Ok(()) => {
                state.events.push_back((now, true));
                state.cooldown_until = None;
            }
            Err(kind) => {
                prune_events(state, now, self.policy.window);
                let (successes, failures) = event_counts(state);
                match cooldown_directive(
                    kind,
                    FailureWindow {
                        successes,
                        failures,
                    },
                    tier_size,
                    self.policy.failure_threshold_millis,
                ) {
                    CooldownDirective::Ignore => {}
                    CooldownDirective::RecordFailure => {
                        state.events.push_back((now, false));
                    }
                    CooldownDirective::OpenCircuit => {
                        state.events.push_back((now, false));
                        state.cooldown_until = now.checked_add(self.policy.cooldown);
                    }
                }
            }
        }
    }

    fn abandon(&self, deployment: &str) {
        let mut health = self.health.lock().expect("capacity lock poisoned");
        if let Some(state) = health.get_mut(deployment) {
            state.in_flight = state.in_flight.saturating_sub(1);
            state.half_open_probe = false;
        }
    }
}

fn local_circuit_availability(state: &DeploymentHealth, now: Instant) -> LocalCircuitAvailability {
    match state.cooldown_until {
        None => LocalCircuitAvailability::Closed,
        Some(until) if until > now => LocalCircuitAvailability::Open,
        Some(_) if state.half_open_probe => LocalCircuitAvailability::HalfOpenProbeInFlight,
        Some(_) => LocalCircuitAvailability::HalfOpen,
    }
}

pub struct CapacityLease {
    manager: Arc<CapacityManager>,
    pub deployment: RouteDeployment,
    pub runners_up: Vec<RouteDeployment>,
    pub evaluations: Vec<DeploymentEvaluation>,
    tier_size: usize,
    completed: bool,
}

impl CapacityLease {
    pub fn complete(mut self, result: Result<(), UpstreamErrorKind>) {
        self.manager
            .finish(&self.deployment.id, self.tier_size, result);
        self.completed = true;
    }
}

impl Drop for CapacityLease {
    fn drop(&mut self) {
        if !self.completed {
            self.manager.abandon(&self.deployment.id);
        }
    }
}

fn prune_events(state: &mut DeploymentHealth, now: Instant, window: Duration) {
    while state
        .events
        .front()
        .is_some_and(|(at, _)| now.duration_since(*at) > window)
    {
        state.events.pop_front();
    }
}

fn event_counts(state: &DeploymentHealth) -> (u64, u64) {
    state
        .events
        .iter()
        .fold((0_u64, 0_u64), |(successes, failures), (_, success)| {
            if *success {
                (successes.saturating_add(1), failures)
            } else {
                (successes, failures.saturating_add(1))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use urouter_types::ModelId;

    fn deployment(id: &str, order: u16) -> RouteDeployment {
        RouteDeployment {
            id: id.to_owned(),
            model: ModelId::new("local-vllm/qwen3.5-4b").unwrap(),
            base_url: None,
            weight: 1,
            order,
            provider_scope: None,
            credential_scope: None,
            enabled: true,
            credential_available: true,
            region: None,
            residency: Vec::new(),
            tenant_allowlist: Vec::new(),
            quota_usage_millis: None,
            accept_new_requests: true,
            binding_grace_until_unix: None,
        }
    }

    #[test]
    fn failed_primary_opens_and_reselection_uses_backup() {
        let manager = CapacityManager::new(CooldownPolicy::default());
        let deployments = vec![deployment("primary", 0), deployment("backup", 1)];
        let lease = manager.select(&deployments, &BTreeSet::new()).unwrap();
        assert_eq!(lease.deployment.id, "primary");
        lease.complete(Err(UpstreamErrorKind::ServerError));

        let lease = manager.select(&deployments, &BTreeSet::new()).unwrap();
        assert_eq!(lease.deployment.id, "backup");
        let snapshot = manager.snapshot();
        assert_eq!(
            snapshot
                .iter()
                .find(|item| item.deployment == "primary")
                .unwrap()
                .state,
            CircuitState::Open
        );
    }

    #[test]
    fn single_deployment_protection_keeps_tier_available() {
        let manager = CapacityManager::new(CooldownPolicy::default());
        let deployments = vec![deployment("only", 0)];
        manager
            .select(&deployments, &BTreeSet::new())
            .unwrap()
            .complete(Err(UpstreamErrorKind::RateLimited));
        assert_eq!(manager.snapshot()[0].state, CircuitState::Closed);
        assert!(manager.select(&deployments, &BTreeSet::new()).is_ok());
    }

    #[test]
    fn half_open_allows_one_probe_and_success_closes_circuit() {
        let manager = CapacityManager::new(CooldownPolicy {
            cooldown: Duration::from_millis(1),
            ..CooldownPolicy::default()
        });
        let deployments = vec![deployment("primary", 0), deployment("backup", 1)];
        manager
            .select(&deployments, &BTreeSet::new())
            .unwrap()
            .complete(Err(UpstreamErrorKind::ServerError));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            manager
                .snapshot()
                .into_iter()
                .find(|item| item.deployment == "primary")
                .unwrap()
                .state,
            CircuitState::HalfOpen
        );

        let probe = manager.select(&deployments, &BTreeSet::new()).unwrap();
        assert_eq!(probe.deployment.id, "primary");
        let concurrent = manager.select(&deployments, &BTreeSet::new()).unwrap();
        assert_eq!(concurrent.deployment.id, "backup");
        probe.complete(Ok(()));
        assert_eq!(
            manager
                .snapshot()
                .into_iter()
                .find(|item| item.deployment == "primary")
                .unwrap()
                .state,
            CircuitState::Closed
        );
    }

    #[test]
    fn least_loaded_picker_moves_concurrent_work_to_idle_deployment() {
        let manager =
            CapacityManager::with_picker(CooldownPolicy::default(), DeploymentPicker::LeastLoaded);
        let deployments = vec![deployment("a", 0), deployment("b", 0)];
        let first = manager.select(&deployments, &BTreeSet::new()).unwrap();
        let second = manager.select(&deployments, &BTreeSet::new()).unwrap();
        assert_ne!(first.deployment.id, second.deployment.id);
    }

    #[test]
    fn latency_and_quota_pickers_consume_their_signals() {
        let deployments = vec![deployment("a", 0), deployment("b", 0)];
        let latency = CapacityManager::with_picker(
            CooldownPolicy::default(),
            DeploymentPicker::LowestLatency,
        );
        latency.observe_latency("a", 50);
        latency.observe_latency("b", 10);
        assert_eq!(
            latency
                .select(&deployments, &BTreeSet::new())
                .unwrap()
                .deployment
                .id,
            "b"
        );

        let mut quota_deployments = deployments;
        quota_deployments[0].quota_usage_millis = Some(800);
        quota_deployments[1].quota_usage_millis = Some(200);
        let quota = CapacityManager::with_picker(
            CooldownPolicy::default(),
            DeploymentPicker::LowestQuotaUsage,
        );
        assert_eq!(
            quota
                .select(&quota_deployments, &BTreeSet::new())
                .unwrap()
                .deployment
                .id,
            "b"
        );
    }
}
