//! Startup configuration checks for `--dry-run`.
//!
//! These run without any upstream, Redis or listener activity: they validate the
//! Catalog, the Route and the argument combination alone. The report is the
//! machine-readable contract that deployment manifests are checked against.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DryRunStatus {
    Pass,
    Fail,
    Warning,
    NotApplicable,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ValidationCostClass {
    Free,
    Local,
    Remote,
    PostHoc,
}

pub(crate) const CASCADE_COST_CLASSES: [ValidationCostClass; 5] = [
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Local,
    ValidationCostClass::Remote,
    ValidationCostClass::PostHoc,
];

pub(crate) const FILTER_COST_CLASSES: [ValidationCostClass; 10] = [
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Free,
    ValidationCostClass::Local,
    ValidationCostClass::Local,
    ValidationCostClass::Local,
    ValidationCostClass::Local,
];

#[derive(Debug, Serialize)]
pub(crate) struct DryRunCheck {
    pub(crate) number: u8,
    pub(crate) id: &'static str,
    pub(crate) status: DryRunStatus,
    pub(crate) message: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct DryRunError {
    pub(crate) code: &'static str,
    pub(crate) path: String,
    pub(crate) message: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct DryRunReport {
    pub(crate) schema_version: u16,
    pub(crate) valid: bool,
    pub(crate) catalog: Option<Value>,
    pub(crate) route: Option<Value>,
    pub(crate) checks: Vec<DryRunCheck>,
    pub(crate) errors: Vec<DryRunError>,
}

pub(crate) const DRY_RUN_CHECK_IDS: [&str; 19] = [
    "cascade_cost_class_order",
    "filter_cost_class_order",
    "artifact_feature_schema",
    "artifact_tier_subset",
    "target_catalog_and_cost",
    "referenced_provider_auth",
    "exploration_requires_recording",
    "tier_targets_and_fallbacks",
    "budget_soft_below_hard",
    "multi_instance_state_policy",
    "tool_route_model_decider",
    "tier_capability_equivalence",
    "catalog_manifest",
    "custom_provider_compat",
    "endpoint_placeholders",
    "cost_override_reason",
    "quota_limit_consistency",
    "quota_scope_topology",
    "intelligence_signal_sources",
];

pub(crate) fn check(
    number: u8,
    id: &'static str,
    status: DryRunStatus,
    message: impl Into<String>,
) -> DryRunCheck {
    DryRunCheck {
        number,
        id,
        status,
        message: message.into(),
    }
}

pub(crate) fn blocked_checks(message: &str) -> Vec<DryRunCheck> {
    (1_u8..)
        .zip(DRY_RUN_CHECK_IDS.iter())
        .map(|(number, id)| check(number, id, DryRunStatus::Blocked, message))
        .collect()
}

pub(crate) fn configuration_dry_run(args: &Args) -> DryRunReport {
    if let Err(error) = validate_args(args) {
        return failed_dry_run(
            "invalid_arguments",
            "arguments",
            error.to_string(),
            "configuration arguments are invalid",
        );
    }
    let catalog_source = match fs::read(&args.catalog) {
        Ok(source) => source,
        Err(error) => {
            return failed_dry_run(
                "catalog_read_failed",
                args.catalog.display().to_string(),
                error.to_string(),
                "catalog could not be loaded",
            );
        }
    };
    let catalog_text = match std::str::from_utf8(&catalog_source) {
        Ok(text) => text,
        Err(error) => {
            return failed_dry_run(
                "catalog_encoding_invalid",
                args.catalog.display().to_string(),
                error.to_string(),
                "catalog is not UTF-8",
            );
        }
    };
    let catalog = match CatalogSnapshot::from_json_str(catalog_text) {
        Ok(catalog) => catalog,
        Err(error) => {
            return failed_dry_run(
                "catalog_invalid",
                args.catalog.display().to_string(),
                error.to_string(),
                "catalog parsing or validation failed",
            );
        }
    };
    let route_source = match fs::read_to_string(&args.route) {
        Ok(source) => source,
        Err(error) => {
            return failed_dry_run(
                "route_read_failed",
                args.route.display().to_string(),
                error.to_string(),
                "route configuration could not be loaded",
            );
        }
    };
    let route: RouteConfig = match serde_json::from_str(&route_source) {
        Ok(route) => route,
        Err(error) => {
            return failed_dry_run(
                "route_parse_failed",
                args.route.display().to_string(),
                error.to_string(),
                "route configuration is invalid JSON",
            );
        }
    };
    dry_run_report(args, &catalog_source, &catalog, &route)
}

pub(crate) fn failed_dry_run(
    code: &'static str,
    path: impl Into<String>,
    message: String,
    blocked_message: &str,
) -> DryRunReport {
    DryRunReport {
        schema_version: 1,
        valid: false,
        catalog: None,
        route: None,
        checks: blocked_checks(blocked_message),
        errors: vec![DryRunError {
            code,
            path: path.into(),
            message,
        }],
    }
}

pub(crate) fn dry_run_report(
    args: &Args,
    catalog_source: &[u8],
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> DryRunReport {
    let deployment_count = route
        .tiers
        .iter()
        .map(|tier| tier.effective_deployments().len())
        .sum::<usize>();
    let route_validation = route.validate(catalog);
    let route_status = if route_validation.is_ok() {
        DryRunStatus::Pass
    } else {
        DryRunStatus::Fail
    };
    let route_message = route_validation
        .as_ref()
        .map_or_else(std::string::ToString::to_string, |()| {
            "route validation passed".to_owned()
        });
    let (auth_status, auth_message) = referenced_auth_check(catalog, route);
    let (manifest_status, manifest_message) = catalog_manifest_check(args, catalog_source, catalog);
    let mut checks = schema_dry_run_checks(args, catalog, route);
    checks.extend(route_dry_run_checks(
        args,
        catalog,
        route,
        route_status,
        &route_message,
        auth_status,
        auth_message,
    ));
    checks.extend(catalog_dry_run_checks(
        catalog_source,
        manifest_status,
        manifest_message,
    ));
    let valid = checks.iter().all(|item| item.status != DryRunStatus::Fail);
    DryRunReport {
        schema_version: 1,
        valid,
        catalog: Some(json!({
            "schema_version": catalog.schema_version(),
            "content_revision": catalog.hashes().content,
            "models": catalog.models().count()
        })),
        route: Some(json!({
            "id": route.id,
            "revision": route.revision(),
            "tiers": route.tiers.len(),
            "deployments": deployment_count
        })),
        checks,
        errors: Vec::new(),
    }
}

pub(crate) fn referenced_auth_check(
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> (DryRunStatus, String) {
    let missing_auth = route
        .tiers
        .iter()
        .flat_map(TierConfig::effective_deployments)
        .filter_map(|deployment| catalog.model(&deployment.model))
        .filter_map(|model| catalog.provider(&model.provider))
        .filter_map(|provider| match &provider.auth {
            urouter_ai::auth::AuthSpec::ApiKeyEnv { env, .. }
                if std::env::var_os(env).is_none() =>
            {
                Some(env.clone())
            }
            urouter_ai::auth::AuthSpec::OAuthClientCredentials {
                client_id_env,
                client_secret_env,
                ..
            } if std::env::var_os(client_id_env).is_none()
                || std::env::var_os(client_secret_env).is_none() =>
            {
                Some(format!("{client_id_env},{client_secret_env}"))
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    if missing_auth.is_empty() {
        (
            DryRunStatus::Pass,
            "all referenced provider credentials are resolvable or explicitly disabled".to_owned(),
        )
    } else {
        (
            DryRunStatus::Fail,
            format!(
                "missing credential environment variables: {}",
                missing_auth.into_iter().collect::<Vec<_>>().join(", ")
            ),
        )
    }
}

pub(crate) fn catalog_manifest_check(
    args: &Args,
    catalog_source: &[u8],
    catalog: &CatalogSnapshot,
) -> (DryRunStatus, String) {
    let manifest_path = args.catalog.with_file_name("manifest.json");
    match fs::read_to_string(&manifest_path)
        .map_err(|error| error.to_string())
        .and_then(|source| {
            serde_json::from_str::<CatalogManifest>(&source).map_err(|error| error.to_string())
        }) {
        Ok(manifest) if manifest.matches(catalog_source, catalog) => (
            DryRunStatus::Pass,
            format!("manifest matches {}", args.catalog.display()),
        ),
        Ok(_) => (
            DryRunStatus::Fail,
            "manifest hashes do not match the catalog".to_owned(),
        ),
        Err(error) => (
            DryRunStatus::Fail,
            format!(
                "manifest {} is unavailable or invalid: {error}",
                manifest_path.display()
            ),
        ),
    }
}

pub(crate) fn schema_dry_run_checks(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> Vec<DryRunCheck> {
    let artifact_configured = args.artifact_active.is_some() || args.artifact_candidate.is_some();
    let (artifact_status, artifact_message) = if artifact_configured {
        match load_artifact_runtime(args, catalog, route) {
            Ok(Some(_)) => (
                DryRunStatus::Pass,
                "artifact schema, signature, revisions, gates, and tiers passed".to_owned(),
            ),
            Ok(None) => (
                DryRunStatus::Fail,
                "artifact configuration did not produce a runtime".to_owned(),
            ),
            Err(error) => (DryRunStatus::Fail, error.to_string()),
        }
    } else {
        (
            DryRunStatus::NotApplicable,
            "no router artifact is configured".to_owned(),
        )
    };
    vec![
        check(
            1,
            DRY_RUN_CHECK_IDS[0],
            monotonic_cost_class_status(&CASCADE_COST_CLASSES),
            "compiled Rule/Signal/Model/Judge/Escalation cascade is cost-monotonic",
        ),
        check(
            2,
            DRY_RUN_CHECK_IDS[1],
            monotonic_cost_class_status(&FILTER_COST_CLASSES),
            "compiled tenant/auth/catalog/capacity/policy filter chain is cost-monotonic",
        ),
        check(
            3,
            DRY_RUN_CHECK_IDS[2],
            artifact_status,
            artifact_message.clone(),
        ),
        check(4, DRY_RUN_CHECK_IDS[3], artifact_status, artifact_message),
    ]
}

pub(crate) fn route_dry_run_checks(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
    route_status: DryRunStatus,
    route_message: &str,
    auth_status: DryRunStatus,
    auth_message: String,
) -> Vec<DryRunCheck> {
    vec![
        check(5, DRY_RUN_CHECK_IDS[4], route_status, route_message),
        check(6, DRY_RUN_CHECK_IDS[5], auth_status, auth_message),
        check(
            7,
            DRY_RUN_CHECK_IDS[6],
            if args.exploration_epsilon_millionths == 0
                || args.records.is_some()
                || args.redis_url.is_some()
            {
                DryRunStatus::Pass
            } else {
                DryRunStatus::Fail
            },
            if args.exploration_epsilon_millionths == 0 {
                "controlled exploration is disabled"
            } else if args.records.is_some() || args.redis_url.is_some() {
                "controlled exploration has a persistent DecisionRecord sink"
            } else {
                "controlled exploration requires --records so propensity evidence is persisted"
            },
        ),
        check(8, DRY_RUN_CHECK_IDS[7], route_status, route_message),
        check(
            9,
            DRY_RUN_CHECK_IDS[8],
            budget_boundary_status(args),
            budget_boundary_message(args),
        ),
        check(
            10,
            DRY_RUN_CHECK_IDS[9],
            if args.redis_url.is_none()
                || args.on_state_unavailable.as_deref() == Some("fail_closed")
            {
                DryRunStatus::Pass
            } else {
                DryRunStatus::Fail
            },
            if args.redis_url.is_none() {
                "single-instance state does not require a cross-instance failure policy"
            } else if args.on_state_unavailable.as_deref() == Some("fail_closed") {
                "multi-instance correctness state explicitly fails closed"
            } else {
                "Redis requires --on-state-unavailable fail_closed"
            },
        ),
        check(
            11,
            DRY_RUN_CHECK_IDS[10],
            tool_decider_status(args, catalog, route),
            tool_decider_message(args, catalog, route),
        ),
        check(12, DRY_RUN_CHECK_IDS[11], route_status, route_message),
        // Numbered after the catalog checks because they were added later; the
        // report is assembled from three functions and each check states its
        // own number.
        quota_limit_consistency_check(route),
        quota_scope_topology_check(args, route),
        intelligence_signal_sources_check(route),
    ]
}

pub(crate) fn catalog_dry_run_checks(
    catalog_source: &[u8],
    manifest_status: DryRunStatus,
    manifest_message: String,
) -> Vec<DryRunCheck> {
    let (override_status, override_message) = cost_override_reason_check(catalog_source);
    vec![
        check(13, DRY_RUN_CHECK_IDS[12], manifest_status, manifest_message),
        check(
            14,
            DRY_RUN_CHECK_IDS[13],
            DryRunStatus::Pass,
            "catalog validation requires explicit compatibility for custom providers",
        ),
        check(
            15,
            DRY_RUN_CHECK_IDS[14],
            DryRunStatus::Pass,
            "catalog validation resolved every endpoint placeholder from provider env",
        ),
        check(16, DRY_RUN_CHECK_IDS[15], override_status, override_message),
    ]
}

/// A configured ledger that cannot behave as its author intended.
///
/// Two shapes are caught: a duplicated `(scope, window, dimension)`, where one
/// of the two values is silently lost, and a daily cap below its own per-minute
/// cap, where the minute window always rejects first so the daily one can never
/// bind.
fn quota_limit_consistency_check(route: &RouteConfig) -> DryRunCheck {
    let limits = route.quota_limit_set();
    if limits.limits.is_empty() {
        return check(
            17,
            DRY_RUN_CHECK_IDS[16],
            DryRunStatus::NotApplicable,
            "no scoped quota limits are configured".to_owned(),
        );
    }
    let duplicates = limits.duplicates();
    if !duplicates.is_empty() {
        return check(
            17,
            DRY_RUN_CHECK_IDS[16],
            DryRunStatus::Fail,
            format!("{} duplicate quota limit(s) configured", duplicates.len()),
        );
    }
    let unreachable = limits.inconsistent_windows();
    if !unreachable.is_empty() {
        return check(
            17,
            DRY_RUN_CHECK_IDS[16],
            DryRunStatus::Warning,
            format!(
                "{} daily cap(s) sit below their own per-minute cap and can never bind",
                unreachable.len()
            ),
        );
    }
    check(
        17,
        DRY_RUN_CHECK_IDS[16],
        DryRunStatus::Pass,
        format!("{} scoped quota limit(s) are internally consistent", limits.limits.len()),
    )
}

/// Cross-tenant quota scopes cannot be made hash-slot safe on Redis Cluster.
///
/// A single `EVAL` for one request touches tenant-, provider- and
/// credential-scoped keys. Provider keys are shared across tenants, so no hash
/// tag can co-slot every key in a plan. The workspace's `redis` dependency has
/// no `cluster` feature today, which is why this is a warning rather than a
/// failure — but it must be stated, not discovered.
fn quota_scope_topology_check(args: &Args, route: &RouteConfig) -> DryRunCheck {
    let shared_scopes = route.quota_limits.iter().any(|limit| {
        matches!(
            limit.scope,
            urouter_contracts::QuotaScopeKind::Provider
                | urouter_contracts::QuotaScopeKind::Credential
        )
    });
    if !shared_scopes {
        return check(
            18,
            DRY_RUN_CHECK_IDS[17],
            DryRunStatus::NotApplicable,
            "no cross-tenant quota scopes are configured".to_owned(),
        );
    }
    let clustered = args
        .redis_url
        .as_deref()
        .is_some_and(|url| url.contains("cluster") || url.matches(',').count() > 0);
    if clustered {
        return check(
            18,
            DRY_RUN_CHECK_IDS[17],
            DryRunStatus::Fail,
            "provider/credential quota scopes cannot be hash-slot safe on a Redis Cluster endpoint"
                .to_owned(),
        );
    }
    check(
        18,
        DRY_RUN_CHECK_IDS[17],
        DryRunStatus::Pass,
        "cross-tenant quota scopes are served by a single-slot Redis endpoint".to_owned(),
    )
}

/// Reports which intelligence signal sources are switched on.
///
/// The point of this check is that `shadow` and `on` are indistinguishable from
/// `off` in the response body — a source with no producer wired up silently
/// contributes nothing. Without this check an operator who enabled one would
/// have no way to tell whether it was running, and "configured" would read as
/// "working". It states plainly what is armed and what is still inert.
fn intelligence_signal_sources_check(route: &RouteConfig) -> DryRunCheck {
    let Some(config) = route.intelligence.as_ref() else {
        return check(
            19,
            DRY_RUN_CHECK_IDS[18],
            DryRunStatus::NotApplicable,
            "no intelligence signal sources are configured".to_owned(),
        );
    };
    let sources = [
        ("trajectory", config.trajectory.mode),
        ("intent", config.intent.mode),
    ];
    // Producers land in a later change; until then every non-off mode is inert.
    let inert: Vec<String> = sources
        .iter()
        .filter(|(_, mode)| mode.produces())
        .map(|(name, mode)| format!("{name}={}", mode.as_str()))
        .collect();
    if inert.is_empty() {
        return check(
            19,
            DRY_RUN_CHECK_IDS[18],
            DryRunStatus::Pass,
            "all intelligence signal sources are off".to_owned(),
        );
    }
    check(
        19,
        DRY_RUN_CHECK_IDS[18],
        DryRunStatus::Warning,
        format!(
            "{} enabled but no producer is wired up yet; these sources contribute nothing",
            inert.join(", ")
        ),
    )
}

pub(crate) fn monotonic_cost_class_status(classes: &[ValidationCostClass]) -> DryRunStatus {
    if classes.windows(2).all(|pair| pair[0] <= pair[1]) {
        DryRunStatus::Pass
    } else {
        DryRunStatus::Fail
    }
}

pub(crate) fn budget_boundary_status(args: &Args) -> DryRunStatus {
    if (args.tenant_budget_nano_usd == 0 && args.tenant_budget_soft_nano_usd == 0)
        || (args.tenant_budget_soft_nano_usd > 0
            && args.tenant_budget_soft_nano_usd < args.tenant_budget_nano_usd)
    {
        DryRunStatus::Pass
    } else {
        DryRunStatus::Fail
    }
}

pub(crate) fn budget_boundary_message(args: &Args) -> &'static str {
    if args.tenant_budget_nano_usd == 0 && args.tenant_budget_soft_nano_usd == 0 {
        "tenant budget is disabled"
    } else if args.tenant_budget_soft_nano_usd > 0
        && args.tenant_budget_soft_nano_usd < args.tenant_budget_nano_usd
    {
        "tenant soft budget is positive and below the hard budget"
    } else {
        "enabled budget requires 0 < --tenant-budget-soft-nano-usd < --tenant-budget-nano-usd"
    }
}

pub(crate) fn route_supports_tools(catalog: &CatalogSnapshot, route: &RouteConfig) -> bool {
    route.tiers.iter().any(|tier| {
        catalog
            .model(&tier.model)
            .is_some_and(|model| model.capabilities.tool_calling)
    })
}

pub(crate) fn tool_decider_status(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> DryRunStatus {
    let expects_tools = route_supports_tools(catalog, route);
    if !expects_tools || args.artifact_active.is_some() || args.artifact_candidate.is_some() {
        DryRunStatus::Pass
    } else {
        DryRunStatus::Warning
    }
}

pub(crate) fn tool_decider_message(
    args: &Args,
    catalog: &CatalogSnapshot,
    route: &RouteConfig,
) -> &'static str {
    let expects_tools = route_supports_tools(catalog, route);
    if !expects_tools {
        "route has no enabled targets"
    } else if args.artifact_active.is_some() || args.artifact_candidate.is_some() {
        "route has a configured model decider artifact"
    } else {
        "route has enabled targets but no model decider artifact; rule/signal routing remains active"
    }
}

pub(crate) fn cost_override_reason_check(catalog_source: &[u8]) -> (DryRunStatus, String) {
    let Ok(value) = serde_json::from_slice::<Value>(catalog_source) else {
        return (
            DryRunStatus::Fail,
            "catalog could not be decoded for cost override validation".to_owned(),
        );
    };
    let mut overrides = 0_usize;
    let mut missing_reason = 0_usize;
    visit_cost_overrides(&value, &mut overrides, &mut missing_reason);
    if missing_reason == 0 {
        (
            DryRunStatus::Pass,
            format!("validated {overrides} catalog cost override(s)"),
        )
    } else {
        (
            DryRunStatus::Fail,
            format!("{missing_reason} of {overrides} cost override(s) lack a non-empty reason"),
        )
    }
}

pub(crate) fn visit_cost_overrides(
    value: &Value,
    overrides: &mut usize,
    missing_reason: &mut usize,
) {
    match value {
        Value::Object(object) => {
            if let Some(cost_override) = object.get("cost_override") {
                *overrides += 1;
                if cost_override
                    .get("reason")
                    .and_then(Value::as_str)
                    .is_none_or(|reason| reason.trim().is_empty())
                {
                    *missing_reason += 1;
                }
            }
            for child in object.values() {
                visit_cost_overrides(child, overrides, missing_reason);
            }
        }
        Value::Array(array) => {
            for child in array {
                visit_cost_overrides(child, overrides, missing_reason);
            }
        }
        _ => {}
    }
}
