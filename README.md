# uRouter

Agent-aware model routing infrastructure. The first implementation milestone is
the `urouter-ai` model-facts and pricing kernel. The M0 Gateway now exposes an
OpenAI-compatible `urouter/auto` route over that kernel.

```bash
cargo test --workspace
cargo run -p urouter-catalog -- check catalog/catalog.json
cargo run -p urouter-catalog -- list catalog/catalog.json --capability tools
cargo run -p urouter-catalog -- show catalog/catalog.json gpt-5.6-terra
cargo run -p urouter-catalog -- cost catalog/catalog.json openai/gpt-5.6-terra \
  --usage catalog/fixtures/usage-mixed.json
cargo run -p urouter-catalog -- --json diff old-catalog.json catalog/catalog.json
cargo run -p urouter-smoke -- --model local-vllm-qwen38/qwen3.8-27b --json
cargo run -p urouter-soak -- --gateway http://127.0.0.1:8787 \
  --requests 5000 --concurrency 32 --max-p95-ms 250 --max-error-rate 0.001
cargo run -q -p urouter-gateway -- --bind 127.0.0.1:8787 \
  --records /tmp/urouter-gateway-decisions.jsonl
cargo run -q -p urouter-gateway -- --dry-run
# Multi-instance task continuity:
cargo run -q -p urouter-gateway -- --bind 127.0.0.1:8787 \
  --redis-url redis://127.0.0.1:6379/ --redis-prefix urouter-prod \
  --tenant-max-in-flight 32 --tenant-requests-per-minute 600 \
  --tenant-tokens-per-minute 1000000 --quota-default-max-output-tokens 4096 \
  --tenant-budget-nano-usd 50000000000 --budget-period-seconds 2592000 \
  --quota-lease-ttl-seconds 86400 \
  --require-tenant-header \
  --management-keyring gateway/management-keyring.json \
  --management-audit /var/log/urouter/management-audit.jsonl
cargo run --release -p urouter-catalog --example release_benchmark
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/check-secrets.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/check-pure-crates.ps1
```

P4-P6 add a governed offline pipeline, signed learned-policy rollout, normalized
protocol adapters, a replay-aware client and an in-process facade:

```bash
cargo run -p urouter-lab -- normalize --gateway-records decisions.jsonl --output normalized.json
cargo run -p urouter-lab -- dataset --records records.json --filter filter.json \
  --deletion-generations generations.json --feedback-policy feedback-policy.json \
  --output dataset.json
cargo run -p urouter-lab -- counterfactual --samples samples.json --output report.json
cargo run -p urouter-lab -- benchmark --cases benchmark.json --output benchmark-report.json
```

The Gateway also accepts non-streaming `POST /v1/responses` and
`POST /v1/messages`. Provider deployments may use `open_ai_chat`,
`open_ai_responses`, or `anthropic_messages`; non-Chat streaming handoff is
explicitly rejected until a byte-safe stream adapter is installed. Start a
learned policy only with a signed artifact and an environment-injected key:

```bash
cargo run -p urouter-gateway -- \
  --artifact-active artifact.json \
  --artifact-signing-key-env UROUTER_ARTIFACT_SIGNING_KEY \
  --artifact-shadow
```

Artifact status/promote/rollback/kill/rollout/observe endpoints use the existing
management RBAC and audit boundary. Controlled epsilon exploration is disabled
by default and additionally requires request-level recording, training consent,
explicit exploration authorization and a server-bounded budget.

The Gateway serves model discovery and chat plus paginated decision records,
feedback ingestion, deployment health, and Prometheus metrics. It supports
multi-deployment retry reselection, cooldown/circuit state, and bounded tier
fallback. Agent integrations can additionally bind primary calls to one exact
model per task while routing auxiliary calls independently. Tenant-scoped task,
feedback, and decision state, retention TTL, metadata-only/no-record policies,
persistent deletion, and a dry-run explain endpoint complete the M0 governance
boundary. Select `urouter/auto` as the model and place routing controls under the
top-level `urouter` object. That object is removed before forwarding to the
selected provider.

Task bindings use a bounded in-memory backend by default. Supplying `--redis-url`
enables atomic cross-instance bindings with TTL and first-success-wins semantics,
plus shared deployment/credential/provider circuit state and one global Half-Open probe.
Contract v2 adds tenant-scoped `conversation + branch` bindings that pin the exact
model/provider/API and prompt/toolset identity; contract v1 task bindings remain supported.
Agent products can use `/v1/adapters/aionui` or `/v1/adapters/workbuddy` as an
OpenAI-compatible provider base path and inject per-request conversation, turn, and
call-kind headers. The adapter derives stable prompt/toolset hashes and maps auxiliary
title/summary calls without changing the primary session binding.
Supplying `--management-keyring` enables Bearer-token RBAC for management endpoints,
requires an explicit tenant header, and requires a synchronously persisted audit file.
The keyring is hot-reloaded and supports overlapping activation windows for rotation.
Redis-backed routing state fails closed with a stable HTTP 503 while unavailable and
reconnects automatically with bounded connection/response timeouts; the first request
during a reconnect window may still receive 503. Restart durability depends on Redis
AOF/RDB configuration.
With `--redis-url`, task bindings, circuits, idempotency claims, DecisionRecords,
feedback, and tenant concurrency permits all use Redis as the single authority. Local
`--records`/`--feedback-records` files are rejected in this mode to prevent deleted data
remaining on another gateway's disk. `--tenant-max-in-flight` defaults to `0`
(unlimited); a positive value atomically limits concurrent requests per tenant.
`--tenant-requests-per-minute` also defaults to `0`; a positive value applies a rolling
60-second request window. `--tenant-tokens-per-minute` adds a weighted rolling window.
TPM admission reserves serialized request bytes divided by four plus the requested
output limit (or `--quota-default-max-output-tokens`), including retry input allowance;
successful responses settle against Usage, while failures, cancellations, and missing
Usage retain the conservative reservation. Admitted RPM entries are not returned on
completion or cancellation, while in-flight permits are. Redis permits expire after
`--quota-lease-ttl-seconds` (default one day), so until lease renewal is implemented this
value must exceed the longest expected request or stream. Quota backend errors currently
fail closed with the standard state-backend HTTP 503.
`--tenant-budget-nano-usd` enables a hard per-period budget (`0` is unlimited):
admission reserves the worst-case fixed-point cost across configured fallbacks and
retries, successful Usage settles actual cost, and missing Usage or cancellation retains
the conservative reservation. `--budget-period-seconds` defaults to 30 days.

P2 adds typed fallback chains, dependency-aware readiness, graceful drain and
OpenMetrics duration/TTFT/cost/fallback histograms. `GET /health/live` reports
process liveness; `GET /health/ready` also checks drain state, shared-state
dependencies and the required control revision. Run the reproducible Redis and
upstream fault gate with `scripts/run-p2-chaos.ps1`; it writes
`target/p2-chaos-report.json` and fails CI when a scenario fails.

P3 adds provider-neutral discovery, reviewed candidates, bounded capability
probes, signed control manifests and hot reload. Bootstrap the checked-in
Catalog/Route revision and start reload mode with:

```powershell
cargo run -q -p urouter-catalog -- sync control-manifest
cargo run -q -p urouter-gateway -- `
  --bind 127.0.0.1:8787 `
  --control-manifest gateway/control-manifest.json `
  --control-reload-seconds 5 `
  --control-failure-policy last_good
```

`GET /v1/catalog` returns the active revision and ETag. RBAC-protected refresh
and rollback endpoints load only the server-configured manifest and signing key.
See `catalog/providers/README.md` for discovery, review, paid-probe budget,
publish, rollback, OAuth bearer-file/ambient credentials and retirement steps.

Semantic routing remains conservative: exact greetings prefer the efficient
tier, equations require a reasoning-capable tier, and unknown text abstains to
the existing policy. Realtime weather declares `requires_tools` and requires a
Host-disclosed `get_weather` tool; the Gateway never invents or executes live
weather data. A missing required tool returns `missing_required_tool` before an
upstream call, while a later tool-result message continues on a capable model.

The running Gateway exposes its P0 machine-readable API contract at
`GET /openapi.json`. `POST /v1/explain` does not call an upstream provider and returns
`schema_version: 1`. Configuration-level `--dry-run` also avoids upstream, Redis, and
listener activity; it returns the 16 design checks with explicit
`pass/fail/warning/not_applicable/blocked` status and exits with code 2 on failure.

Execution status is tracked in
[`docs/requirements-traceability.md`](docs/requirements-traceability.md). Architecture
and migration decisions are under [`docs/adr`](docs/adr), and the deployment trust
boundary is documented in
[`docs/security/threat-model.md`](docs/security/threat-model.md).
Detailed P2/P3 code and external acceptance status is in
[`docs/p2-p3-execution-status.md`](docs/p2-p3-execution-status.md).
P4-P6 code evidence and remaining production acceptance are in
[`docs/p4-p6-execution-status.md`](docs/p4-p6-execution-status.md), with public
compatibility rules in [`docs/public-api-and-semver.md`](docs/public-api-and-semver.md).

The first P1 slice adds the pure `urouter-contracts` crate. Explain responses now carry
a versioned structural `feature_frame` and a `routing_trace` explicitly marked
`summary`. Execution records additionally retain deployment selection and exclusion
reasons, including requests exhausted before any upstream attempt; auth, budget, and
future policy-filter tracing remain tracked as P1 work.

Implementation scope and remaining external verification are tracked in
[`docs/urouter-ai-implementation-status.md`](docs/urouter-ai-implementation-status.md).
The Gateway contract, selection rules, and local end-to-end evidence are in
[`docs/gateway-m0.md`](docs/gateway-m0.md).
