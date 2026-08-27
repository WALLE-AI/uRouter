# `urouter-ai` public API review

Review date: 2026-08-26  
Version reviewed: 0.1.0

## Stable MVP surface

- `CatalogSnapshot::from_json_str` and `from_document` build validated immutable snapshots.
- `model`, `resolve_model`, `model_variant`, and `provider` are deterministic, I/O-free lookups.
- `eligible_models` applies facts-only hard admission and returns exclusion reasons.
- `EndpointPlan::for_model` materializes endpoint, auth requirements, and public headers without
  resolving credentials.
- `calculate_actual_cost` returns fixed-point component costs from actual usage.
- `CatalogEvidence::from_catalog` pins the schema, content, pricing, model, provider, API, and price
  source needed by a future DecisionRecord.
- `validate_deployments` and `model_projections` are narrow consumer interfaces, not policy engines.

## Compatibility decisions

- Public fact enums are `non_exhaustive`; consumers must retain fallback branches as providers add
  capabilities or wire formats.
- Catalog JSON is versioned independently from the Rust crate. Unsupported schema versions fail at
  load time rather than being guessed.
- Existing public structs remain constructible for tests and configuration tooling. Adding required
  fields to them is therefore a semver-breaking change before a schema migration exists.
- Serialization is a catalog/tool contract. Internal `Arc` indexes and canonicalization details are
  not exposed.

## Deliberately unstable or internal

- Validator implementation details and index layout.
- CLI human-readable wording; `--json` output is the automation contract.
- Smoke harness prompts and latency values. Check names and pass/fail semantics are stable, but the
  prompt corpus may evolve.
- Any future routing score, workload class, tier, or learned policy. These do not belong in this
  crate.

## Gateway integration rule

Gateway should hold an `Arc<CatalogSnapshot>` and atomically replace the whole snapshot when dynamic
refresh is implemented. Request paths must not read catalog files, environment variables, or remote
model lists. Credential resolution belongs to Gateway and consumes `AuthPlan` only at execution time.
