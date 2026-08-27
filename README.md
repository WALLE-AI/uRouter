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
# Multi-instance task continuity:
cargo run -q -p urouter-gateway -- --bind 127.0.0.1:8787 \
  --redis-url redis://127.0.0.1:6379/ --redis-prefix urouter-prod \
  --require-tenant-header \
  --management-keyring gateway/management-keyring.json \
  --management-audit /var/log/urouter/management-audit.jsonl
cargo run --release -p urouter-catalog --example release_benchmark
```

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
With `--redis-url`, task bindings, circuits, DecisionRecords, and feedback all use Redis
as the single authority. Local `--records`/`--feedback-records` files are rejected in
this mode to prevent deleted data remaining on another gateway's disk.

Implementation scope and remaining external verification are tracked in
[`docs/urouter-ai-implementation-status.md`](docs/urouter-ai-implementation-status.md).
The Gateway contract, selection rules, and local end-to-end evidence are in
[`docs/gateway-m0.md`](docs/gateway-m0.md).
