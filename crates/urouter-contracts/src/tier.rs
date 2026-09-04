use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackTierSpec {
    pub tier: String,
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingPlan {
    pub tiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingPlanError {
    EmptyTier,
    DuplicateTier(String),
    UnknownTier(String),
    Cycle(String),
}

pub fn plan_fallback_tiers(
    tiers: &[FallbackTierSpec],
    selected: &str,
    max_depth: u8,
) -> Result<RoutingPlan, RoutingPlanError> {
    let mut indexes = BTreeMap::new();
    for (index, tier) in tiers.iter().enumerate() {
        if tier.tier.is_empty() {
            return Err(RoutingPlanError::EmptyTier);
        }
        if indexes.insert(tier.tier.as_str(), index).is_some() {
            return Err(RoutingPlanError::DuplicateTier(tier.tier.clone()));
        }
    }
    let mut planned = Vec::new();
    let mut emitted = BTreeSet::new();
    let mut active = BTreeSet::new();
    append_planned_tier(
        tiers,
        &indexes,
        selected,
        max_depth,
        &mut emitted,
        &mut active,
        &mut planned,
    )?;
    Ok(RoutingPlan { tiers: planned })
}

fn append_planned_tier(
    tiers: &[FallbackTierSpec],
    indexes: &BTreeMap<&str, usize>,
    tier_name: &str,
    remaining: u8,
    emitted: &mut BTreeSet<String>,
    active: &mut BTreeSet<String>,
    planned: &mut Vec<String>,
) -> Result<(), RoutingPlanError> {
    if active.contains(tier_name) {
        return Err(RoutingPlanError::Cycle(tier_name.to_owned()));
    }
    let index = indexes
        .get(tier_name)
        .copied()
        .ok_or_else(|| RoutingPlanError::UnknownTier(tier_name.to_owned()))?;
    if !emitted.insert(tier_name.to_owned()) {
        return Ok(());
    }
    planned.push(tier_name.to_owned());
    if remaining == 0 {
        return Ok(());
    }
    active.insert(tier_name.to_owned());
    for fallback in &tiers[index].fallbacks {
        append_planned_tier(
            tiers,
            indexes,
            fallback,
            remaining - 1,
            emitted,
            active,
            planned,
        )?;
    }
    active.remove(tier_name);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierCandidate {
    pub index: usize,
    pub tier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierDecisionInput {
    pub candidates: Vec<TierCandidate>,
    pub pin_tier: Option<String>,
    pub floor_index: usize,
    pub auxiliary: bool,
    pub high_quality: bool,
    pub low_cost: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOutcome {
    Abstain,
    Selected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEvaluation {
    pub rule: String,
    pub outcome: RuleOutcome,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSelection {
    pub index: usize,
    pub reason: String,
    pub evaluations: Vec<RuleEvaluation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierDecisionError {
    UnknownPinnedTier(String),
    NoEligibleTier,
}

trait TierRule {
    fn id(&self) -> &'static str;

    fn evaluate(
        &self,
        input: &TierDecisionInput,
        candidates: &[&TierCandidate],
    ) -> Result<Option<(usize, String)>, TierDecisionError>;
}

struct PinRule;

impl TierRule for PinRule {
    fn id(&self) -> &'static str {
        "pin"
    }

    fn evaluate(
        &self,
        input: &TierDecisionInput,
        candidates: &[&TierCandidate],
    ) -> Result<Option<(usize, String)>, TierDecisionError> {
        let Some(pin) = &input.pin_tier else {
            return Ok(None);
        };
        candidates
            .iter()
            .find(|candidate| candidate.tier == *pin)
            .map(|candidate| (candidate.index, "preference_pin".to_owned()))
            .map(Some)
            .ok_or_else(|| TierDecisionError::UnknownPinnedTier(pin.clone()))
    }
}

struct QualityRule;

impl TierRule for QualityRule {
    fn id(&self) -> &'static str {
        "quality"
    }

    /// `high_quality` means "the tier we would otherwise take is not enough",
    /// so this steps up one tier from the floor rather than jumping to the top.
    ///
    /// [`DefaultRule`] takes `candidates.first()` — the floor — so the tier
    /// above it is `candidates.get(1)`. Jumping to `last()` would make every
    /// middle tier unreachable: any quality signal, however weak, would buy the
    /// most expensive deployment in the route. That is the opposite of routing
    /// to the cheapest tier that still clears the bar.
    ///
    /// With only two tiers configured `get(1)` and `last()` are the same
    /// element, so this is behaviour-preserving until a third tier exists; see
    /// `quality_steps_up_one_tier_and_is_unchanged_for_two_tiers`.
    fn evaluate(
        &self,
        input: &TierDecisionInput,
        candidates: &[&TierCandidate],
    ) -> Result<Option<(usize, String)>, TierDecisionError> {
        Ok((input.high_quality && !input.auxiliary).then(|| {
            (
                candidates
                    .get(1)
                    .or_else(|| candidates.last())
                    .expect("cascade receives eligible candidates")
                    .index,
                "quality_guard".to_owned(),
            )
        }))
    }
}

struct DefaultRule;

impl TierRule for DefaultRule {
    fn id(&self) -> &'static str {
        "default"
    }

    fn evaluate(
        &self,
        input: &TierDecisionInput,
        candidates: &[&TierCandidate],
    ) -> Result<Option<(usize, String)>, TierDecisionError> {
        Ok(Some((
            candidates
                .first()
                .expect("cascade receives eligible candidates")
                .index,
            if input.low_cost {
                "cost_preference".to_owned()
            } else {
                "default_efficient".to_owned()
            },
        )))
    }
}

pub fn select_tier_with_cascade(
    input: &TierDecisionInput,
) -> Result<TierSelection, TierDecisionError> {
    let all = input.candidates.iter().collect::<Vec<_>>();
    if all.is_empty() {
        return Err(TierDecisionError::NoEligibleTier);
    }
    let filtered = input
        .candidates
        .iter()
        .filter(|candidate| candidate.index >= input.floor_index)
        .collect::<Vec<_>>();
    let rules: [&dyn TierRule; 3] = [&PinRule, &QualityRule, &DefaultRule];
    let mut evaluations = Vec::with_capacity(rules.len());
    for (position, rule) in rules.into_iter().enumerate() {
        let candidates = if position == 0 { &all } else { &filtered };
        if candidates.is_empty() {
            return Err(TierDecisionError::NoEligibleTier);
        }
        match rule.evaluate(input, candidates)? {
            Some((index, reason)) => {
                evaluations.push(RuleEvaluation {
                    rule: rule.id().to_owned(),
                    outcome: RuleOutcome::Selected,
                    reason: reason.clone(),
                });
                return Ok(TierSelection {
                    index,
                    reason,
                    evaluations,
                });
            }
            None => evaluations.push(RuleEvaluation {
                rule: rule.id().to_owned(),
                outcome: RuleOutcome::Abstain,
                reason: "not_applicable".to_owned(),
            }),
        }
    }
    Err(TierDecisionError::NoEligibleTier)
}
