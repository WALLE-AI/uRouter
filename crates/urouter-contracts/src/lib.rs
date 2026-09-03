//! Protocol-neutral contracts shared by the online and offline routing paths.
//!
//! This crate is pure: it does not read clocks, files, environment variables,
//! networks, or shared state. Runtime observations must be supplied explicitly.

mod backoff;
mod capacity;
mod cooldown;
mod exhaustion;
mod features;
mod latency;
mod quota;
mod retry;
mod state;
mod tier;
mod tokens;
mod upstream;

pub use backoff::*;
pub use capacity::*;
pub use cooldown::*;
pub use exhaustion::*;
pub use features::*;
pub use latency::*;
pub use quota::*;
pub use retry::*;
pub use state::*;
pub use tier::*;
pub use tokens::*;
pub use upstream::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retry_policy_is_typed_bounded_and_honors_retry_after() {
        let policy = RetryPolicy {
            max_retries: 2,
            base_backoff_ms: 50,
            max_backoff_ms: 1_000,
        };
        assert!(policy.should_retry(UpstreamErrorKind::Timeout, 0));
        assert!(policy.should_retry(UpstreamErrorKind::ServerError, 1));
        assert!(!policy.should_retry(UpstreamErrorKind::ServerError, 2));
        assert!(!policy.should_retry(UpstreamErrorKind::BadRequest, 0));
        assert_eq!(policy.backoff_ms(1, None), 100);
        assert_eq!(policy.backoff_ms(1, Some(750)), 750);
        assert_eq!(
            policy.directive(UpstreamErrorKind::Timeout, 0, 1, Some(750)),
            RetryDirective::RetrySameDeployment { backoff_ms: 750 }
        );
        assert_eq!(
            policy.directive(UpstreamErrorKind::ServerError, 0, 2, None),
            RetryDirective::ReselectDeployment { backoff_ms: 0 }
        );
        assert_eq!(
            policy.directive(UpstreamErrorKind::BadRequest, 0, 2, None),
            RetryDirective::Stop
        );
        assert_eq!(
            policy.directive(UpstreamErrorKind::Timeout, 0, 0, None),
            RetryDirective::Stop
        );
    }

    #[test]
    fn cooldown_policy_is_typed_and_preserves_single_deployment_availability() {
        let empty = FailureWindow {
            successes: 0,
            failures: 0,
        };
        assert_eq!(
            cooldown_directive(UpstreamErrorKind::BadRequest, empty, 2, 500),
            CooldownDirective::Ignore
        );
        assert_eq!(
            cooldown_directive(UpstreamErrorKind::RateLimited, empty, 1, 500),
            CooldownDirective::RecordFailure
        );
        assert_eq!(
            cooldown_directive(UpstreamErrorKind::RateLimited, empty, 2, 500),
            CooldownDirective::OpenCircuit
        );
        assert_eq!(
            cooldown_directive(
                UpstreamErrorKind::ServerError,
                FailureWindow {
                    successes: 1,
                    failures: 0,
                },
                2,
                500,
            ),
            CooldownDirective::OpenCircuit
        );
    }

    #[test]
    fn cooldown_opening_is_monotonic_across_failure_thresholds() {
        for successes in 0..8 {
            for failures in 0..8 {
                let window = FailureWindow {
                    successes,
                    failures,
                };
                let lower = cooldown_directive(UpstreamErrorKind::Timeout, window, 2, 250);
                let higher = cooldown_directive(UpstreamErrorKind::Timeout, window, 2, 750);
                assert!(
                    higher != CooldownDirective::OpenCircuit
                        || lower == CooldownDirective::OpenCircuit
                );
                assert_ne!(
                    cooldown_directive(UpstreamErrorKind::Timeout, window, 1, 0),
                    CooldownDirective::OpenCircuit
                );
            }
        }
    }

    #[test]
    fn fallback_plan_is_depth_bounded_and_deduplicates_branches() {
        let tiers = vec![
            FallbackTierSpec {
                tier: "efficient".to_owned(),
                fallbacks: vec!["balanced".to_owned(), "capable".to_owned()],
            },
            FallbackTierSpec {
                tier: "balanced".to_owned(),
                fallbacks: vec!["capable".to_owned()],
            },
            FallbackTierSpec {
                tier: "capable".to_owned(),
                fallbacks: Vec::new(),
            },
        ];
        assert_eq!(
            plan_fallback_tiers(&tiers, "efficient", 0).unwrap().tiers,
            ["efficient"]
        );
        assert_eq!(
            plan_fallback_tiers(&tiers, "efficient", 2).unwrap().tiers,
            ["efficient", "balanced", "capable"]
        );
    }

    #[test]
    fn fallback_plan_rejects_unknown_references_and_cycles() {
        let unknown = vec![FallbackTierSpec {
            tier: "efficient".to_owned(),
            fallbacks: vec!["missing".to_owned()],
        }];
        assert_eq!(
            plan_fallback_tiers(&unknown, "efficient", 1),
            Err(RoutingPlanError::UnknownTier("missing".to_owned()))
        );

        let cycle = vec![
            FallbackTierSpec {
                tier: "efficient".to_owned(),
                fallbacks: vec!["capable".to_owned()],
            },
            FallbackTierSpec {
                tier: "capable".to_owned(),
                fallbacks: vec!["efficient".to_owned()],
            },
        ];
        assert_eq!(
            plan_fallback_tiers(&cycle, "efficient", 2),
            Err(RoutingPlanError::Cycle("efficient".to_owned()))
        );
    }

    #[test]
    fn deployment_selection_preserves_order_and_weighted_ticket_behavior() {
        let candidates = vec![
            DeploymentCandidate {
                id: "b".to_owned(),
                order: 0,
                weight: 2,
                available: true,
                unavailable_reasons: Vec::new(),
            },
            DeploymentCandidate {
                id: "a".to_owned(),
                order: 0,
                weight: 1,
                available: true,
                unavailable_reasons: Vec::new(),
            },
            DeploymentCandidate {
                id: "backup".to_owned(),
                order: 1,
                weight: 100,
                available: true,
                unavailable_reasons: Vec::new(),
            },
        ];
        assert_eq!(
            select_weighted_deployment(&candidates, 0).unwrap().selected,
            "a"
        );
        assert_eq!(
            select_weighted_deployment(&candidates, 1).unwrap().selected,
            "b"
        );
        assert_eq!(
            select_weighted_deployment(&candidates, 2).unwrap().selected,
            "b"
        );
        assert_eq!(
            select_weighted_deployment(&candidates, 0)
                .unwrap()
                .runners_up,
            ["b"]
        );
    }

    #[test]
    fn deployment_selection_excludes_unavailable_before_ordering() {
        let candidates = vec![
            DeploymentCandidate {
                id: "primary".to_owned(),
                order: 0,
                weight: 1,
                available: false,
                unavailable_reasons: vec!["cooldown".to_owned()],
            },
            DeploymentCandidate {
                id: "backup".to_owned(),
                order: 1,
                weight: 1,
                available: true,
                unavailable_reasons: Vec::new(),
            },
        ];
        assert_eq!(
            select_weighted_deployment(&candidates, 0).unwrap().selected,
            "backup"
        );
        let selection = select_weighted_deployment(&candidates, 0).unwrap();
        assert_eq!(
            selection.evaluations[0].disposition,
            DeploymentDisposition::Excluded
        );
        assert_eq!(selection.evaluations[0].reasons, ["cooldown"]);
    }

    #[test]
    fn capacity_lease_plan_reserves_only_the_selected_half_open_probe() {
        let candidates = vec![
            CapacityCandidateSnapshot {
                id: "open-primary".to_owned(),
                order: 0,
                weight: 1,
                retry_excluded: false,
                circuit: LocalCircuitAvailability::Open,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
            CapacityCandidateSnapshot {
                id: "half-open".to_owned(),
                order: 0,
                weight: 1,
                retry_excluded: false,
                circuit: LocalCircuitAvailability::HalfOpen,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
            CapacityCandidateSnapshot {
                id: "backup".to_owned(),
                order: 1,
                weight: 1,
                retry_excluded: false,
                circuit: LocalCircuitAvailability::Closed,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
        ];
        let plan = plan_capacity_lease(&candidates, 0).unwrap();
        assert_eq!(plan.selection.selected, "half-open");
        assert!(plan.reserve_half_open_probe);
        assert!(plan.selection.evaluations.iter().any(|candidate| {
            candidate.deployment == "open-primary"
                && candidate.reasons == ["local_circuit_unavailable"]
        }));
    }

    #[test]
    fn capacity_lease_plan_exhaustion_retains_all_exclusion_reasons() {
        let candidates = vec![
            CapacityCandidateSnapshot {
                id: "retry".to_owned(),
                order: 0,
                weight: 1,
                retry_excluded: true,
                circuit: LocalCircuitAvailability::Closed,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
            CapacityCandidateSnapshot {
                id: "busy-probe".to_owned(),
                order: 0,
                weight: 1,
                retry_excluded: true,
                circuit: LocalCircuitAvailability::HalfOpenProbeInFlight,
                in_flight: 0,
                latency_ewma_ms: None,
                quota_usage_millis: None,
                unavailable_reasons: Vec::new(),
            },
        ];
        let CapacityLeasePlanError::Exhausted(evaluations) =
            plan_capacity_lease(&candidates, 0).unwrap_err()
        else {
            panic!("expected exhausted capacity plan");
        };
        assert_eq!(evaluations.len(), 2);
        assert_eq!(evaluations[0].reasons, ["retry_excluded"]);
        assert_eq!(
            evaluations[1].reasons,
            ["retry_excluded", "local_circuit_unavailable"]
        );
    }

    #[test]
    fn capacity_pickers_use_signals_and_deterministic_weighted_ties() {
        let candidate = |id: &str, load, latency, quota| CapacityCandidateSnapshot {
            id: id.to_owned(),
            order: 0,
            weight: 1,
            retry_excluded: false,
            circuit: LocalCircuitAvailability::Closed,
            in_flight: load,
            latency_ewma_ms: latency,
            quota_usage_millis: quota,
            unavailable_reasons: Vec::new(),
        };
        let candidates = vec![
            candidate("a", 2, Some(30), Some(500)),
            candidate("b", 0, Some(10), Some(700)),
            candidate("c", 1, Some(20), Some(100)),
        ];
        assert_eq!(
            plan_capacity_lease_with_picker(&candidates, 0, DeploymentPicker::LeastLoaded)
                .unwrap()
                .selection
                .selected,
            "b"
        );
        assert_eq!(
            plan_capacity_lease_with_picker(&candidates, 0, DeploymentPicker::LowestLatency)
                .unwrap()
                .selection
                .selected,
            "b"
        );
        assert_eq!(
            plan_capacity_lease_with_picker(&candidates, 0, DeploymentPicker::LowestQuotaUsage)
                .unwrap()
                .selection
                .selected,
            "c"
        );
    }

    fn tier_input() -> TierDecisionInput {
        TierDecisionInput {
            candidates: vec![
                TierCandidate {
                    index: 0,
                    tier: "efficient".to_owned(),
                },
                TierCandidate {
                    index: 1,
                    tier: "capable".to_owned(),
                },
            ],
            pin_tier: None,
            floor_index: 0,
            auxiliary: false,
            high_quality: false,
            low_cost: false,
        }
    }

    #[test]
    fn tier_cascade_records_abstention_before_default_selection() {
        let selection = select_tier_with_cascade(&tier_input()).unwrap();
        assert_eq!(selection.index, 0);
        assert_eq!(selection.reason, "default_efficient");
        assert_eq!(selection.evaluations.len(), 3);
        assert_eq!(selection.evaluations[0].outcome, RuleOutcome::Abstain);
        assert_eq!(selection.evaluations[2].outcome, RuleOutcome::Selected);
    }

    #[test]
    fn tier_cascade_preserves_pin_quality_and_auxiliary_semantics() {
        let mut input = tier_input();
        input.pin_tier = Some("capable".to_owned());
        assert_eq!(select_tier_with_cascade(&input).unwrap().index, 1);

        input.pin_tier = None;
        input.high_quality = true;
        assert_eq!(select_tier_with_cascade(&input).unwrap().index, 1);

        input.auxiliary = true;
        input.low_cost = true;
        let selection = select_tier_with_cascade(&input).unwrap();
        assert_eq!(selection.index, 0);
        assert_eq!(selection.reason, "cost_preference");
    }

    #[test]
    fn tier_cascade_rejects_unknown_pin_and_empty_floor() {
        let mut input = tier_input();
        input.pin_tier = Some("unknown".to_owned());
        assert_eq!(
            select_tier_with_cascade(&input),
            Err(TierDecisionError::UnknownPinnedTier("unknown".to_owned()))
        );
        input.pin_tier = None;
        input.floor_index = 2;
        assert_eq!(
            select_tier_with_cascade(&input),
            Err(TierDecisionError::NoEligibleTier)
        );
    }

    #[test]
    fn extracts_stable_chat_features_without_semantic_guessing() {
        let frame = FeatureFrame::from_openai_chat(&json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "solve"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,x"}}
                ]},
                {"role": "tool", "content": "result"}
            ],
            "tools": [{"type": "function"}],
            "response_format": {"type": "json_schema"},
            "reasoning_effort": "high",
            "max_completion_tokens": 64
        }));
        assert_eq!(frame.schema_version, FEATURE_SCHEMA_VERSION);
        assert_eq!(frame.message_count, 2);
        assert_eq!(frame.input_text_bytes, 11);
        assert_eq!(frame.available_tool_count, 1);
        assert!(frame.content.has_tool_result);
        assert!(frame.requests.structured_output);
        assert!(frame.requests.reasoning);
        assert!(frame.content.contains_image);
        assert_eq!(frame.requested_max_output_tokens, Some(64));
    }

    #[test]
    fn summary_trace_does_not_claim_full_filter_coverage() {
        let trace = RoutingTrace::current_admission_summary(
            "small",
            ["large".to_owned()],
            [(
                "text-only".to_owned(),
                vec!["missing_modality:image".to_owned()],
            )],
            "default_efficient",
        );
        assert_eq!(trace.completeness, TraceCompleteness::Summary);
        assert_eq!(
            trace.candidates[0].disposition,
            CandidateDisposition::Selected
        );
        assert_eq!(
            trace.candidates[1].disposition,
            CandidateDisposition::Eligible
        );
        assert_eq!(
            trace.candidates[2].disposition,
            CandidateDisposition::Excluded
        );
        assert_eq!(trace.candidates[2].reasons, ["missing_modality:image"]);
        assert_eq!(trace.completeness, TraceCompleteness::Summary);
    }

    #[test]
    fn deterministic_record_context_is_complete_without_fake_propensity() {
        let features = FeatureFrame::from_openai_chat(&json!({
            "messages": [{"role": "user", "content": "hello"}]
        }));
        let context = DecisionRecordContext::deterministic(
            "req_1",
            RevisionSet {
                catalog: "sha256:catalog".to_owned(),
                route: "sha256:route".to_owned(),
                feature_schema: FEATURE_SCHEMA_VERSION,
                policy: "current_gateway:v1".to_owned(),
            },
            features,
            RoutingTrace::current_summary("small", [], "default"),
            vec!["small".to_owned()],
        );
        assert!(context.training_complete());
        assert_eq!(context.propensity_millionths, None);
    }

    #[test]
    fn shared_state_failure_matrix_protects_correctness_domains() {
        for domain in [
            SharedStateDomain::Budget,
            SharedStateDomain::Quota,
            SharedStateDomain::Binding,
            SharedStateDomain::Circuit,
            SharedStateDomain::DecisionRecord,
        ] {
            assert_eq!(
                shared_state_failure_policy(domain),
                BackendFailurePolicy::FailClosed
            );
        }
        for domain in [SharedStateDomain::Metrics, SharedStateDomain::LatencySignal] {
            assert_eq!(
                shared_state_failure_policy(domain),
                BackendFailurePolicy::FailOpen
            );
        }
    }
}
