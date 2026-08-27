# `urouter-ai` MVP implementation status

Last updated: 2026-08-26

## Completed

- ADRs for fact/policy separation, fixed-point money, and snapshot versioning.
- `urouter-types` with validated IDs, usage, fixed-point rates, and nano-USD.
- Immutable `CatalogSnapshot` with canonical model and alias indexes.
- Aggregated catalog validation and custom-provider Compat enforcement.
- Exact actual-cost calculation with cache components and long-context tiers.
- Explicit counterfactual estimate type, separate from actual cost.
- Endpoint templates, API-key auth plans, loopback-safe no-auth, and header rules.
- Separate content, pricing, capability, compatibility, and source-file hashes.
- Capability admission, Deployment validation, DecisionRecord evidence, and model projection consumers.
- `urouter-catalog` check/show/list/cost/diff/manifest commands.
- Officially sourced OpenAI and Anthropic seed entries plus a local test fixture.
- Golden pricing vectors, negative validation tests, CI, fmt, clippy, and tests.
- Concurrent read-path coverage and a repeatable 10k-model/1m-operation release benchmark.
- Catalog change pull-request checklist for source, pricing, compatibility, and manifest review.
- Local vLLM endpoint smoke-tested on `127.0.0.1:8087`; observed deployment facts are pinned in the catalog.
- Qwen3.8-27B (currently `127.0.0.1:19121/starvlm`) passed text, image, tools,
  JSON Schema, streaming usage, and reasoning tests; the earlier verification endpoint was 8094.
- `urouter-smoke` now reproduces capability-driven endpoint checks and emits actual usage, cost, and complete `CatalogEvidence`.
- Catalog variant/alias provenance APIs and duplicate provider/upstream/API validation.
- Stable CLI `--json`, per-model semantic diff categories, and pull-request baseline diff automation.
- 33 pricing golden vectors plus deterministic generated tests for 5k loader, 10k template, and 10k cost inputs.
- Public API/semver review with non-exhaustive fact enums and a Gateway rustdoc example.
- The first Gateway vertical slice consumes the catalog for admission, endpoint planning,
  request compatibility rewrites, exact cost calculation, and reproducible decision evidence.
- The Gateway continuation adds typed timeout/retry attempts, cancellation propagation,
  bounded JSONL sinks, paginated records, idempotent feedback, paired overrides, and metrics.
- The Capacity continuation adds multi-deployment selection, retry reselection, rolling
  cooldown/circuit state, bounded acyclic tier fallback, deployment audit fields, and a
  live tier-health endpoint.
- The Task-aware continuation adds typed Task/Agent/Call identity, exact primary-model
  continuity, auxiliary bypass, safe migration boundaries, compatibility-mode disclosure,
  hashed task audit keys, and binding lifecycle endpoints.
- The Governance continuation adds trusted-header tenant scoping, explicit
  `none`/`metadata_only` recording policies, training and remote-judge eligibility,
  retention TTL sweeping, persistent tenant/task/decision deletion, and dry-run explain.
- The first multi-instance continuation introduces an async task-binding state port,
  a bounded memory implementation, and Redis atomic CAS bindings with TTL,
  tenant indexes, first-success-wins, and authorized generation changes.
- Shared capacity continuation adds a Redis circuit gate around local weighted
  selection, deployment/credential/provider failure scopes, atomic global
  Half-Open probing, cancellation cleanup, and merged tier-health visibility.
- Management-plane continuation adds tenant-scoped Bearer-token RBAC, reader/operator/admin
  endpoint policy, synchronous redacted JSONL audit, overlapping keys, activation windows,
  and last-known-good hot reload.
- Multi-instance restart continuation verifies new repository instances, gateway process
  restart, Redis AOF restart, automatic reconnect, persisted task/circuit state, and a
  stable redacted HTTP 503 contract while shared state is unavailable.
- Shared governance continuation makes Redis authoritative for tenant-scoped
  DecisionRecord and feedback, including bounded ordered indexes, retention TTL,
  atomic signal-kind merge, dynamically hydrated outcomes, and cross-instance
  decision/task/tenant deletion.
- Session continuity adds contract v2 `conversation + branch` identity, exact
  model/provider/API and prompt/toolset pinning, branch isolation, safe identity
  migration, Redis CAS persistence, and session lifecycle endpoints while retaining
  contract v1 task-binding compatibility.
- Agent adapter continuation adds AionUI/WorkBuddy OpenAI-compatible entrypoints,
  conversation/turn header mapping, canonical prompt/toolset identity derivation,
  deterministic auxiliary-call bypass, and baseline-versus-routable capability
  discovery with `capability_required` admission.

## Deliberately deferred

- OAuth and ambient cloud credentials.
- Dynamic catalog refresh and ETag handling.
- Cross-provider message handoff.
- Cross-provider wire translation; M0 HTTP execution currently supports OpenAI-compatible chat.
- PyO3 bindings and the Python lab.
- Bulk import from third-party model catalogs.

These are outside the approved facts-kernel MVP and should be selected only after
the first Gateway vertical slice identifies a real need.

## External verification still required

- Paid endpoint smoke tests require user-provided OpenAI/Anthropic test credentials.
- The generic test-only fixture remains intentionally synthetic; real local deployments are covered
  by the 8087 and current 19121/starvlm entries.
- Pricing and capability sources must be rechecked when `checked_at` becomes stale.

## Reproduce the release gate

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --release --workspace
cargo doc --workspace --no-deps
cargo run -q -p urouter-catalog -- check catalog/catalog.json
cargo run -q -p urouter-catalog -- --json diff catalog/catalog.json catalog/catalog.json
cargo run --release -p urouter-catalog --example release_benchmark
```

Controlled endpoint verification is intentionally separate from ordinary CI:

```bash
cargo run -q -p urouter-smoke -- --model local-vllm-qwen38/qwen3.8-27b --json
```

See [`gateway-m0.md`](gateway-m0.md) for the running Auto Gateway and its remaining scope.
