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
    CapacityCandidateSnapshot, CapacityLeasePlanError, CapacitySnapshot, CooldownDirective,
    DeploymentEvaluation, DeploymentPicker, FailureWindow, LATENCY_SAMPLE_CAP_MILLIS,
    LocalCircuitAvailability, cooldown_directive, effective_quota_usage_millis,
    latency_sample_millis, plan_capacity_lease_with_picker,
};

use crate::{RouteDeployment, UpstreamErrorKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CooldownPolicy {
    pub cooldown: Duration,
    pub window: Duration,
    pub failure_threshold_millis: u16,
    /// Ceiling on a single attempt's contribution to the latency EWMA; `0`
    /// disables capping. See [`latency_sample_millis`].
    pub latency_sample_cap_millis: u64,
}

impl Default for CooldownPolicy {
    fn default() -> Self {
        Self {
            cooldown: Duration::from_secs(5),
            window: Duration::from_secs(60),
            failure_threshold_millis: 500,
            latency_sample_cap_millis: LATENCY_SAMPLE_CAP_MILLIS,
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
    #[error("capacity snapshot does not match the candidate set")]
    InvalidSnapshot,
}

#[derive(Debug, Default)]
struct DeploymentHealth {
    events: VecDeque<(Instant, bool)>,
    cooldown_until: Option<Instant>,
    half_open_probe: bool,
    in_flight: u64,
    latency_ewma_ms: Option<u64>,
    /// Live utilisation from the quota ledger, in per mille.
    ///
    /// `RouteDeployment::quota_usage_millis` is an operator-typed constant that
    /// nothing ever wrote at runtime, so `DeploymentPicker::LowestQuotaUsage`
    /// ranked on a value that could not change. This is the observed number
    /// that supersedes it.
    observed_quota_usage_millis: Option<u16>,
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
        let snapshot = self.capacity_snapshot(candidates, excluded);
        self.select_from_snapshot(candidates, snapshot)
    }

    pub fn select_from_snapshot(
        self: &Arc<Self>,
        candidates: &[RouteDeployment],
        snapshot: CapacitySnapshot,
    ) -> Result<CapacityLease, CapacityError> {
        if candidates.is_empty() {
            return Err(CapacityError::EmptyTier);
        }
        if !snapshot.is_compatible()
            || snapshot.candidates.len() != candidates.len()
            || candidates
                .iter()
                .any(|candidate| snapshot.candidate(&candidate.id).is_none())
        {
            return Err(CapacityError::InvalidSnapshot);
        }
        let plan = plan_capacity_lease_with_picker(
            &snapshot.candidates,
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
        let mut health = self.health.lock().expect("capacity lock poisoned");
        let selected_health = health.entry(selected.id.clone()).or_default();
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
            snapshot,
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
    pub fn capacity_snapshot(
        &self,
        candidates: &[RouteDeployment],
        excluded: &BTreeSet<String>,
    ) -> CapacitySnapshot {
        let now = Instant::now();
        let mut health = self.health.lock().expect("capacity lock poisoned");
        snapshot_locked(&mut health, candidates, excluded, now, self.policy.window)
    }

    #[must_use]
    pub const fn picker(&self) -> DeploymentPicker {
        self.picker
    }

    #[must_use]
    pub const fn latency_sample_cap_millis(&self) -> u64 {
        self.policy.latency_sample_cap_millis
    }

    /// Fold an attempt's wall-clock latency into the deployment's EWMA, but only
    /// when the outcome is actually evidence of speed.
    ///
    /// The gating lives in [`latency_sample_millis`] so the "is this a speed
    /// signal" question has exactly one answer, testable without a manager.
    pub fn observe_outcome(
        &self,
        deployment: &str,
        outcome: Result<(), UpstreamErrorKind>,
        latency_ms: u64,
    ) {
        let Some(sample) =
            latency_sample_millis(outcome, latency_ms, self.policy.latency_sample_cap_millis)
        else {
            return;
        };
        let mut health = self.health.lock().expect("capacity lock poisoned");
        let state = health.entry(deployment.to_owned()).or_default();
        state.latency_ewma_ms = Some(state.latency_ewma_ms.map_or(sample, |previous| {
            previous.saturating_mul(4).saturating_add(sample) / 5
        }));
    }

    /// Record live quota utilisation for a deployment.
    pub fn observe_quota_usage(&self, deployment: &str, used_millis: u16) {
        let mut health = self.health.lock().expect("capacity lock poisoned");
        health
            .entry(deployment.to_owned())
            .or_default()
            .observed_quota_usage_millis = Some(used_millis);
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
    pub snapshot: CapacitySnapshot,
    tier_size: usize,
    completed: bool,
}

fn snapshot_locked(
    health: &mut BTreeMap<String, DeploymentHealth>,
    candidates: &[RouteDeployment],
    excluded: &BTreeSet<String>,
    now: Instant,
    window: Duration,
) -> CapacitySnapshot {
    CapacitySnapshot::new(
        candidates
            .iter()
            .map(|deployment| {
                let state = health.entry(deployment.id.clone()).or_default();
                prune_events(state, now, window);
                CapacityCandidateSnapshot {
                    id: deployment.id.clone(),
                    order: deployment.order,
                    weight: deployment.weight,
                    retry_excluded: excluded.contains(&deployment.id),
                    circuit: local_circuit_availability(state, now),
                    in_flight: state.in_flight,
                    latency_ewma_ms: state.latency_ewma_ms,
                    // Observed utilisation supersedes the configured constant;
                    // the constant remains the cold-start value so a fresh
                    // process still has something to rank on.
                    quota_usage_millis: effective_quota_usage_millis(
                        deployment.quota_usage_millis,
                        state.observed_quota_usage_millis,
                    ),
                    unavailable_reasons: Vec::new(),
                }
            })
            .collect(),
    )
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
    fn lease_retains_the_exact_pre_reservation_capacity_snapshot() {
        let manager = CapacityManager::new(CooldownPolicy::default());
        let deployments = vec![deployment("primary", 0), deployment("backup", 1)];
        let lease = manager.select(&deployments, &BTreeSet::new()).unwrap();
        assert!(lease.snapshot.is_compatible());
        assert_eq!(lease.snapshot.candidate("primary").unwrap().in_flight, 0);

        let live = manager.capacity_snapshot(&deployments, &BTreeSet::new());
        assert_eq!(live.candidate("primary").unwrap().in_flight, 1);
        assert_eq!(lease.snapshot.candidate("primary").unwrap().in_flight, 0);
        lease.complete(Ok(()));
    }

    #[test]
    fn externally_enriched_snapshot_excludes_shared_circuit_before_selection() {
        let manager = CapacityManager::new(CooldownPolicy::default());
        let deployments = vec![deployment("primary", 0), deployment("backup", 1)];
        let mut snapshot = manager.capacity_snapshot(&deployments, &BTreeSet::new());
        let primary = snapshot
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == "primary")
            .unwrap();
        primary.circuit = LocalCircuitAvailability::Open;
        primary
            .unavailable_reasons
            .push("shared_provider_circuit_unavailable".to_owned());

        let lease = manager
            .select_from_snapshot(&deployments, snapshot)
            .unwrap();
        assert_eq!(lease.deployment.id, "backup");
        let primary = lease
            .evaluations
            .iter()
            .find(|evaluation| evaluation.deployment == "primary")
            .unwrap();
        assert!(
            primary
                .reasons
                .contains(&"shared_provider_circuit_unavailable".to_owned())
        );
        lease.complete(Ok(()));
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
        latency.observe_outcome("a", Ok(()), 50);
        latency.observe_outcome("b", Ok(()), 10);
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

    /// A deployment that rejects every call in 5 ms is not fast, it is broken.
    /// Before the outcome gate those 5 ms rejections fed the EWMA, so a
    /// deployment could *lower* its latency score by failing harder and win
    /// `LowestLatency` over a healthy peer — the picker steered traffic at the
    /// deployment least able to serve it.
    #[test]
    fn fast_failures_do_not_drag_the_latency_ewma_down() {
        let manager = CapacityManager::with_picker(
            CooldownPolicy::default(),
            DeploymentPicker::LowestLatency,
        );
        // One honest, slow-ish success establishes the baseline.
        manager.observe_outcome("broken", Ok(()), 200);
        let baseline = ewma(&manager, "broken");
        assert_eq!(baseline, Some(200));

        for kind in [
            UpstreamErrorKind::Unauthorized,
            UpstreamErrorKind::NotFound,
            UpstreamErrorKind::BadRequest,
            UpstreamErrorKind::RateLimited,
            UpstreamErrorKind::ServerError,
            UpstreamErrorKind::ProviderUnavailable,
            UpstreamErrorKind::Transport,
        ] {
            manager.observe_outcome("broken", Err(kind), 5);
        }
        assert_eq!(
            ewma(&manager, "broken"),
            baseline,
            "non-speed failures must leave the EWMA untouched"
        );

        // A timeout, by contrast, IS the deployment being slow and must count.
        manager.observe_outcome("broken", Err(UpstreamErrorKind::Timeout), 5_000);
        assert!(
            ewma(&manager, "broken") > baseline,
            "a timeout must raise the latency EWMA"
        );
    }

    fn ewma(manager: &CapacityManager, deployment: &str) -> Option<u64> {
        manager
            .health
            .lock()
            .expect("capacity lock poisoned")
            .get(deployment)
            .and_then(|state| state.latency_ewma_ms)
    }

    /// A hang contributes the cap, not its unbounded wall-clock cost, so one
    /// stuck socket cannot flatten the axis for every other sample.
    #[test]
    fn timeout_latency_is_capped_in_the_ewma() {
        let manager = CapacityManager::with_picker(
            CooldownPolicy {
                latency_sample_cap_millis: 1_000,
                ..CooldownPolicy::default()
            },
            DeploymentPicker::LowestLatency,
        );
        manager.observe_outcome("slow", Err(UpstreamErrorKind::Timeout), 1_200_000);
        assert_eq!(ewma(&manager, "slow"), Some(1_000));
    }
}
