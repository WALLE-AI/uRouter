# uRouter Gateway M0

Last updated: 2026-08-30

## Outcome

M0 provides a runnable OpenAI-compatible Auto gateway backed by the immutable
`urouter-ai` catalog. It closes the first vertical slice:

```text
request -> capability requirement -> admission -> deterministic tier selection
        -> capacity/deployment selection -> compatibility rewrite -> upstream execution
        -> retry/reselection/fallback -> usage/cost/evidence record
```

The route is configured in `gateway/route.json`:

| Tier | Catalog model | Local endpoint |
|---|---|---|
| `efficient` | `local-vllm/qwen3.5-4b` | `127.0.0.1:8087` |
| `capable` | `local-vllm-qwen38/qwen3.8-27b` | `127.0.0.1:19121/starvlm` |

## Run

```bash
cargo run -q -p urouter-gateway -- \
  --bind 127.0.0.1:8787 \
  --records /tmp/urouter-gateway-decisions.jsonl
```

Endpoints:

- `GET /health`
- `GET /health/live`
- `GET /health/ready`
- `GET /openapi.json`
- `GET /v1/models`
- `POST /v1/explain`
- `POST /v1/chat/completions`
- `POST /v1/adapters/{aionui|workbuddy}/chat/completions`
- `GET /v1/decisions`
- `GET /v1/decisions/{id}`
- `DELETE /v1/decisions/{id}`
- `DELETE /v1/tasks/{id}/records`
- `DELETE /v1/tenant/records`
- `POST /v1/feedback`
- `GET /v1/feedback/{turn}`
- `GET /v1/tiers`
- `GET /v1/catalog`
- `POST /v1/catalog/refresh`
- `POST /v1/catalog/rollback`
- `GET /v1/tasks/{id}/binding`
- `DELETE /v1/tasks/{id}/binding`
- `GET /v1/sessions/{conversation}/{branch}/binding`
- `DELETE /v1/sessions/{conversation}/{branch}/binding`
- `GET /metrics`

## P2/P3 Continuation

The current Gateway extends M0 without changing the OpenAI chat surface:

- typed error-specific fallback chains and Bad Request hard stops;
- atomic tenant quota and hard-budget reserve/settle semantics;
- dependency and control-revision readiness plus bounded graceful drain;
- OpenMetrics histograms with trace exemplars outside normal labels;
- a signed Catalog/Route control manifest, periodic validated hot reload,
  last-good/fail-closed policies and per-request immutable snapshots;
- RBAC/audit protected Catalog status, refresh and rollback;
- deployment retirement with task/session binding grace;
- conservative greeting, equation and realtime-weather semantic requirements.

Generate an initial manifest with `urouter-catalog sync control-manifest`. In
production, pass `--signing-key-env` to that command and the matching
`--control-signing-key-env` to the Gateway. `GET /v1/catalog` returns the active
revision as JSON and as an HTTP ETag. Refresh always reads server-configured
paths and never accepts credentials or file paths from a client request.

Realtime weather is a Host/Gateway contract, not an embedded weather feature.
The Host must disclose a `get_weather` function in the OpenAI `tools` array,
execute the emitted tool call, and send the tool result in the continuation.
Without that disclosure, the Gateway returns `missing_required_tool` before
contacting a model. Equations select a reasoning-capable model; unknown semantic
classes abstain and preserve the established routing policy.

Example request:

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'x-urouter-tenant-id: tenant-a' \
  -d '{
    "model":"urouter/auto",
    "messages":[{"role":"user","content":"Explain this failure"}],
    "urouter":{
      "contract_version":1,
      "task":{"id":"task-example"},
      "agent":{"harness":"aionui"},
      "call":{"role":"primary"},
      "trace":{"turn":"u:example"},
      "hint":{"value_class":"primary","difficulty":"hard"},
      "data_policy":{"recording":"metadata_only","retention_days":7}
    }
  }'
```

## M0 Selection Rules

Selection is deterministic and auditable:

| Input | Result |
|---|---|
| explicit catalog model | selected directly after capability admission |
| `preference.pin_tier` | selected tier |
| `preference.floor_tier` | tiers below the floor excluded |
| `difficulty=hard`, `workload=plan`, or bias `< -0.5` | highest eligible tier |
| `value_class=auxiliary/disposable` or bias `> 0.5` | lowest eligible tier |
| otherwise | lowest eligible tier |

Auto advertises the intersection of all configured tier capabilities. A request
outside that contract is rejected before an upstream call. Explicit model
selection remains available for capabilities that only one model supports.

M0 consumes the fields it understands and ignores reserved M1 or future fields.
Missing optional fields degrade routing precision but do not reject the request.
`contract_version`, when supplied, must be `1`.

## Compatibility And Disclosure

Before forwarding, the gateway removes the `urouter` object, replaces the
catalog model ID with the provider model ID, normalizes the max-token field, and
rewrites `developer` to `system` when catalog compatibility requires it.

Decisions are disclosed through:

- response headers `x-urouter-request-id`, `x-urouter-decision-id`, `tier`, `model`, `reason`, and `alternatives`;
- a top-level `urouter` object for non-streaming responses;
- a final `event: urouter.decision` SSE event before `data: [DONE]` for streams.

`POST /v1/chat/completions` accepts an optional tenant-scoped `Idempotency-Key` of
1-256 visible ASCII bytes. Reusing it with the same canonical request reuses the
logical `request_id`; using it with a different request returns HTTP 409. Only a
SHA-256 key is retained. This identity contract does not yet cache or replay responses.

Each successful execution creates a `DecisionRecord` containing the capability
requirement, full admission result, selected alternatives, catalog and pricing
hashes, usage, exact cost, upstream status, and latency. The latest 1,000 records
are available in memory; `--records` additionally appends JSONL.

## Reliability And Storage Continuation

The post-M0 continuation adds typed upstream execution behavior:

- configurable connect and request timeouts;
- retry only for transport, timeout, HTTP 429, and HTTP 5xx failures;
- no retry for deterministic HTTP 4xx failures;
- bounded exponential backoff with `Retry-After` precedence;
- a complete attempt list on both successful and exhausted requests;
- a failed `DecisionRecord` and `x-urouter-decision-id` when retries exhaust;
- no retry after a successful streaming response starts;
- dropping the downstream body drops the reqwest stream and cancels upstream work.

Decision and feedback JSONL writes use bounded background queues and never wait
for disk on the response path. Queue overflow and write failures are counted.
Files rotate to one `.1` generation at `--record-max-bytes`. Current files are
replayed on startup up to `--record-capacity`; list queries support `limit` and
`after`, and individual retained records are addressable by decision ID.
Expired records are removed on startup, on management reads, and by a 60-second
background sweep.

If `--records` is supplied without `--feedback-records`, feedback events use the
same path with a `.feedback` suffix.

## Feedback And Overrides

Feedback can arrive through `urouter.signals[]` on a later request or through
`POST /v1/feedback`. The neutral signal vocabulary and strength bounds are
validated. Repeating a signal with the same turn and kind replaces its previous
value; different kinds accumulate. Feedback has its own replayable JSONL event
stream and is joined to decision records by `trace.turn`.

A request with `preference.pin_tier` plus `trace.parent_turn` is treated as a
one-shot override. It becomes a paired sample only when the parent exists, both
turns share a non-empty `trace.id`, messages hashes match, tiers differ, and the
parent completed. Rejections retain a stable reason such as `trace_mismatch`,
`messages_mismatch`, or `parent_incomplete`.

`GET /metrics` exposes request, success, error, retry, feedback, paired,
stream-completion, fallback, record-drop, and writer-error counters in Prometheus format.

## Capacity And Fallback

Each tier may declare explicit deployments. An omitted deployment list keeps the
catalog-backed implicit deployment used by M0:

```json
{
  "tier": "efficient",
  "model": "local-vllm/qwen3.5-4b",
  "deployments": [
    {
      "id": "efficient-primary",
      "model": "local-vllm/qwen3.5-4b",
      "base_url": "http://127.0.0.1:8087/v1",
      "weight": 3,
      "order": 0
    },
    {
      "id": "efficient-backup",
      "model": "local-vllm/qwen3.5-4b",
      "base_url": "http://127.0.0.1:18087/v1",
      "weight": 1,
      "order": 1
    }
  ],
  "fallbacks": ["capable"]
}
```

Selection first removes excluded or cooling deployments, chooses the lowest
`order`, and then applies deterministic weighted selection among deployments at
that order. Retryable failures reselect another deployment when one is
available. A single-deployment tier is not opened by the circuit breaker, so a
transient failure cannot permanently remove its only route.

The circuit breaker uses a rolling failure window and exposes Closed, Open, and
Half-Open states. After cooldown, one request probes the deployment; concurrent
requests use another eligible deployment. A successful probe closes the circuit.
Deterministic request-shape errors classified as `BadRequest` do not retry or
cross a tier fallback. `--max-fallback-depth=0` disables fallback; positive values count
fallback edges from the selected tier. Route validation rejects missing fallback
targets and cycles.

`GET /v1/tiers` returns deployment configuration plus circuit state, rolling
success/failure counts, in-flight requests, and remaining cooldown. Every
attempt and final execution record includes tier, deployment, model, and
fallback depth. Runtime controls are available through `--cooldown-ms`,
`--cooldown-window-ms`, `--cooldown-failure-threshold-millis`, and
`--max-fallback-depth`.

When `--redis-url` is configured, capacity selection adds a shared circuit gate
outside the process-local in-flight and weighted-selection manager. Transport,
timeout, ordinary 500, and not-found failures affect deployment scope; 401/403
and 429 affect credential scope; 502/503/504 affect provider scope. Optional
`provider_scope` and `credential_scope` deployment fields override the catalog-ID
defaults. Invalid or control-character scope labels are rejected at startup.

Redis atomically checks all three scopes before a deployment is used. After a
cooldown expires, only one gateway receives the global Half-Open probe token;
stream cancellation abandons that token with a bounded Redis timeout fallback.
`GET /v1/tiers` merges local health with each shared scope's Closed/Open/Half-Open
state and remaining cooldown.

Tenant admission can additionally be bounded with `--tenant-max-in-flight` and
`--tenant-requests-per-minute`; `--tenant-tokens-per-minute` applies a third weighted
window, and all three limits default to `0` (unlimited). The concurrency limit acquires one
permit after routing governance and releases it after a non-stream response, completed
stream, error, or client cancellation. The latter counts each admitted request in a
rolling 60-second window and is deliberately not returned on completion or cancellation.
Memory mode is process-local. Redis mode uses tenant-scoped sorted sets and one Lua
check/prune/reserve operation, so multiple gateways share both limits. Exhaustion returns
HTTP 429 with `tenant_concurrency_exhausted`, `tenant_rate_limit_exhausted`, or
`tenant_token_limit_exhausted`; Redis
errors return the stable state-backend HTTP 503. In-flight permits have a configurable
`--quota-lease-ttl-seconds` safety expiry (default 86,400 seconds). Streams longer than
that TTL require a larger configured value until lease renewal is implemented.

TPM reserves estimated input for the configured retry allowance plus requested output.
Input currently uses serialized request bytes divided by four; absent output limits use
`--quota-default-max-output-tokens` (default 4,096). Successful non-stream and terminal
stream Usage replaces the reservation, with estimated input retained for earlier failed
attempts. Failure, cancellation, or unavailable Usage keeps the reservation until window
expiry. Settlement backend errors do not hide an already successful provider response;
the conservative reservation remains. Provider-tokenizer estimation and controlled
multi-instance Redis verification remain acceptance work.

## Task-Aware Continuity

Agent integrations can opt into exact task-level model continuity:

```json
{
  "urouter": {
    "contract_version": 1,
    "task": {"id": "opaque-task-id"},
    "agent": {"harness": "aionui"},
    "call": {
      "role": "primary",
      "migration_boundary": null
    },
    "trace": {"turn": "unique-turn-id"}
  }
}
```

A completed primary call creates a binding to the exact selected catalog model.
Later primary calls for the same task reuse that model even when per-call rules
would select a cheaper tier. `call.role=auxiliary` always bypasses the primary
binding and prefers the lowest eligible tier, including when a high-quality hint
is also present. Auxiliary results never update the primary binding.

Concurrent first calls use first-success-wins semantics. A stale in-flight
decision cannot overwrite the model established by an earlier successful call;
it increments `urouter_task_binding_conflict_total` instead. Only an authorized
migration reason may replace an existing model and increment its generation.

Typed migration boundaries are `new_task`, `after_compaction`,
`tool_round_completed`, `before_first_assistant_token`, `explicit_user_retry`,
and `terminal_provider_failure`. Explicit retry and terminal failure may upgrade
one eligible tier. If a bound model cannot satisfy a future call, migration
requires a declared safe boundary; otherwise the request is rejected before an
upstream call.

The five fields `task.id`, `agent.harness`, `call.role`, `trace.turn`, and
`data_policy` are the minimum complete contract. Older clients remain accepted but receive
`x-urouter-compatibility-mode: true`, do not create bindings, and are counted
separately. Task IDs are represented in records and the binding store only as a
SHA-256 key.

The binding store is bounded by `--task-binding-capacity` (default 10,000) and
is process-local by default. `GET /v1/tasks/{id}/binding` supports inspection and
`DELETE` releases a task binding.

For multiple gateway instances, configure the same `--redis-url` and
`--redis-prefix`. Redis stores each binding as a TTL hash and uses one Lua CAS
script for create, same-model refresh, authorized migration, and conflict. This
preserves first-success-wins across processes and increments generation only on
authorized migration. `--task-binding-ttl-seconds` defaults to seven days.
Tenant deletion atomically deletes the tenant's indexed binding keys. The
Auto model discovery response reports `task_binding.backend` as `memory` or
`redis`.

Contract v2 adds exact session continuity while contract v1 remains accepted for
legacy task-level integrations:

```json
{
  "urouter": {
    "contract_version": 2,
    "task": {"id": "opaque-task-id"},
    "agent": {
      "harness": "aionui",
      "prompt_profile_hash": "sha256:<64 hex>",
      "toolset_hash": "sha256:<64 hex>"
    },
    "call": {"role": "primary", "migration_boundary": null},
    "trace": {
      "conversation": "opaque-conversation-id",
      "branch": "main",
      "turn": "unique-turn-id"
    },
    "data_policy": {"recording": "metadata_only", "retention_days": 7}
  }
}
```

Both `trace.conversation` and `trace.branch` are required together. A v2 primary
call additionally requires the complete v1 governance identity plus both SHA-256
execution-profile hashes. The tenant/conversation/branch tuple selects one binding;
different branches do not share it. The binding pins model, provider, wire API,
prompt profile, and toolset. An identity change without a declared migration boundary
is rejected as `unsafe_session_migration` before any upstream call. A normal session
request retries only deployments of its exact model; cross-model fallback is enabled
only at `terminal_provider_failure`.

Session, task, and tenant identifiers are stored or disclosed only through SHA-256
scope keys. The session management endpoints inspect or delete one branch. Deleting a
task removes every session binding owned by that task; tenant deletion removes all of
its task and session bindings.

## AionUI And WorkBuddy Adapters

Set an OpenAI-compatible provider base path to `/v1/adapters/aionui` or
`/v1/adapters/workbuddy`. The normal chat body remains unchanged and must select
`model=urouter/auto`. Supply dynamic request headers:

| Header | Required | Meaning |
|---|---:|---|
| `x-urouter-tenant-id` | production | trusted tenant identity |
| `x-urouter-conversation-id` | yes | AionUI conversation/session ID |
| `x-urouter-turn-id` | yes | server-generated turn ID |
| `x-urouter-task-id` | no | defaults to conversation ID |
| `x-urouter-branch-id` | no | defaults to `main` |
| `x-urouter-call-kind` | no | `primary`, `plan`, `verify`, `title`, `summary`, or `compress`; defaults to `primary` |
| `x-urouter-migration-boundary` | no | one typed v2 safe boundary |

The adapter rejects an existing `urouter` body to avoid ambiguous ownership. It
derives `prompt_profile_hash` from system/developer messages and `toolset_hash` from
the tools array using canonical JSON, then emits the strict v2 contract. Title,
summary, and compression are auxiliary calls and never read or update the primary
binding. Adapter data policy defaults to metadata-only, no training/remote judge,
and seven-day retention.

AionCore currently exposes conversation and turn IDs but its provider record has no
per-request dynamic-header template. Direct production wiring therefore needs a small
AionCore request hook (or an authenticated sidecar) to inject these headers. A static
provider base URL alone is insufficient because a fixed conversation header would
merge unrelated sessions.

`GET /v1/models` reports `capabilities` as the baseline intersection and
`routable_capabilities` as the union reachable through capability admission. A request
requiring a routable-only capability is accepted, restricted to eligible models, and
records `reason=capability_required`. For example, 8087 cannot execute tool calls in
its current vLLM configuration, so agent requests with tools route only to the verified
19121 Qwen3.8-27B endpoint.

## Tenant And Data Governance

The gateway obtains tenant identity only from `x-urouter-tenant-id`; request JSON
cannot override it. Task bindings, feedback, records, and override pairing use a
tenant-scoped hash. Cross-tenant management reads return 404. Use
`--require-tenant-header` to reject missing tenant headers. Without that flag,
missing headers use a `local` compatibility tenant for existing loopback clients.

This is a trusted-header boundary, not end-user authentication. A non-loopback
deployment must place the gateway behind an authenticated proxy or sidecar that
removes incoming copies of the header and injects the authoritative tenant ID.

`data_policy.recording` supports `metadata_only` (default) and `none`.
Metadata-only records contain hashes, routing, execution, catalog evidence, and
governance fields, never message bodies. `none` skips both the DecisionRecord and
piggyback feedback. `allow_training` and `allow_remote_judge` become effective
only for retained, non-compatibility requests. Retention is bounded to 1-365 days.

Decision, task, and tenant deletion rewrite the active JSONL atomically, remove
the rotated generation, and delete unreferenced feedback. Task deletion also
removes the in-memory binding; tenant deletion removes all tenant bindings.
`POST /v1/explain` performs admission, selection, and task binding lookup without
calling an upstream model or writing a decision record.

Use `urouter-soak` as the routing-core release gate. It drives `/v1/explain` without
spending model tokens and fails its process exit code when the configured error-rate,
P95 latency, or RSS-growth threshold is exceeded:

```bash
cargo run -p urouter-soak -- --gateway http://127.0.0.1:8787 \
  --requests 5000 --concurrency 32 --max-p95-ms 250 \
  --max-error-rate 0.001 --max-rss-growth-mib 64
```

## Management RBAC And Audit

`--management-keyring <path>` enables management-plane authentication and also
requires `--management-audit <path>`. Tokens are never stored in the keyring:
store `sha256:<64 lowercase hex>` in `token_sha256` and send the original token as
`Authorization: Bearer <token>`. Generate a token digest without a trailing newline:

```bash
read -rsp 'Management token: ' UROUTER_MANAGEMENT_TOKEN
printf '%s' "$UROUTER_MANAGEMENT_TOKEN" | sha256sum
unset UROUTER_MANAGEMENT_TOKEN
```

The example at `gateway/management-keyring.example.json` deliberately contains
invalid placeholders so it cannot be enabled accidentally. `tenant_keys` contains
tenant-ID SHA-256 values or `"*"`; raw tenant IDs and tokens do not enter the keyring
or audit log.

| Endpoint | Minimum role |
|---|---|
| decision, feedback, task/session-binding reads; `/v1/tiers` | `reader` |
| decision, task-record, task/session-binding deletion | `operator` |
| tenant-wide deletion; `/metrics` | `admin` |

The audit writer appends one JSON event and calls `sync_data` before the operation
runs. Audit failure rejects the request with HTTP 503. Both allowed and denied
actions are recorded with hashed tenant/target keys. The keyring reloads every five
seconds by default; `--management-keyring-reload-seconds` changes the interval.
Put old and new keys in the file concurrently for a zero-downtime rotation, then
remove the old key after clients have switched. An invalid replacement is rejected,
the last valid keyring remains active, and the reload result is audited.

Without `--management-keyring`, loopback compatibility remains unchanged and
management endpoints do not require credentials. This compatibility mode is not a
production security boundary.

When Redis-backed binding or circuit state is configured, loss of Redis is
fail-closed for routing continuity: affected requests return HTTP 503 with
`state_backend_unavailable`. Raw Redis errors and connection details are not returned
to clients. All Redis repositories use a two-second response timeout, one-second
connection timeout, at most three reconnect retries, and a 500 ms maximum retry delay.
The connection manager reconnects after Redis returns, but the first request inside the
reconnect window may still receive 503 and should be retried by the caller. State
survival across Redis restart still depends on the deployment's AOF/RDB and HA policy.

Supplying `--redis-url` also makes Redis authoritative for DecisionRecord and feedback.
Records use a tenant-scoped sorted index, per-record retention TTL, and configured
capacity trimming. Feedback uses tenant/turn hashed keys, atomic per-kind upsert, and
retention TTL. Reads dynamically hydrate decision outcome signals from feedback, so
concurrent writers cannot overwrite another signal with a stale record snapshot.

Redis authoritative mode rejects `--records` and `--feedback-records`. Persisting an
independent JSONL copy on every gateway would leave stale personal data on instances
that did not execute a remote deletion. Existing JSONL data therefore requires an
explicit one-time migration before enabling Redis; the gateway does not silently merge
two authorities.

## Real Endpoint Verification

Verified on 2026-08-26 against the two local vLLM services. Qwen3.8-27B moved
from the original 8094 endpoint to `19121/starvlm` and was reverified there:

| Scenario | Observed result |
|---|---|
| auxiliary trivial Auto request | `efficient`, Qwen3.5-4B, `cost_preference` |
| hard planning Auto request | `capable`, Qwen3.8-27B, `quality_guard` |
| Auto request requiring tools | `capability_required`, capable Qwen3.8-27B with tool call |
| explicit Qwen3.8-27B tool request | successful `tool_calls` response |
| streaming hard request | usage captured; decision event emitted before `[DONE]` |
| audit query | admission, evidence hashes, usage, cost, and latency present |
| 4 concurrent Auto requests | all completed on Qwen3.5-4B with distinct records |
| real one-shot override | 4B parent + identical 27B redo produced `paired=true` |
| repeated standalone feedback | last `accepted` value won; other kinds accumulated |
| piggyback feedback | stored while the next streamed request executed |
| 503 fault injection | one retry, two recorded attempts, successful recovery |
| downstream stream drop | mock upstream body observed cancellation |
| primary deployment 503 | retry reselected backup and opened primary circuit |
| efficient tier exhausted | request fell back to capable with depth `1` |
| expired cooldown | one Half-Open probe; successful probe closed circuit |
| real post-capacity Auto requests | 8087 efficient and 8094 capable each completed and recorded their deployment |
| hard primary then normal primary in one task | both used 8094; second reason was `task_binding` |
| auxiliary call in the same task | used 8087 and left the 8094 primary binding unchanged |
| governed WorkBuddy hard request | used 8094 Qwen3.8-27B; training eligible with one-day TTL |
| cross-tenant decision read | HTTP 404 |
| `recording=none` with piggyback signal | no decision and no feedback retained |
| persistent decision deletion | HTTP 204; absent from memory, active JSONL, and rotation path |
| two gateways with shared Redis | 8788 established 27B binding; 8789 returned `task_binding` / 27B |
| same task ID under another tenant | 8789 returned `default_efficient` / 4B |
| instance A transport failure | retried unavailable deployment then real 8087; two attempts recorded |
| instance B with shared circuit | skipped unavailable deployment and called 8087 once |
| shared tier view | deployment Open while credential/provider remained Closed |
| management request without credential | HTTP 401 and denied audit event |
| Reader decision/tier read | HTTP 200 |
| Reader delete or cross-tenant read | HTTP 403 with identified-key audit |
| Operator metrics / Admin metrics | HTTP 403 / HTTP 200 |
| hard Auto after endpoint migration | 19121 Qwen3.8-27B, `capable`, `quality_guard`, HTTP 200 |
| gateway restart with shared task | new instance retained 27B `task_binding`, generation 1 |
| Redis AOF restart | existing gateway reconnected and retained 27B binding |
| Redis unavailable | HTTP 503 `state_backend_unavailable`, no low-level error disclosure |
| gateway + Redis restart with Open circuit | new instance retained Open and skipped failed deployment |
| A writes decision / B reads | B immediately returned A's 19121 Qwen3.8-27B record |
| concurrent cross-instance feedback | signal kinds merged atomically and hydrated on both instances |
| B deletes decision/task/tenant | A immediately observed 404/empty list; unrelated tenant retained |
| AOF restart after deletion | deleted records and feedback did not reappear |

Both local catalog prices are zero, so the observed cost breakdown is exactly
zero while still exercising the same fixed-point calculator used for paid models.

## Deferred After Capacity Continuation

- Object storage and retention beyond the implemented Redis/one-file-generation modes;
- end-user authentication and external identity-provider integration;
- dataset export and offline paired-sample quality evaluation;
- cross-provider wire translation beyond OpenAI-compatible chat;
- production SLO dashboards and cloud chaos exercises;
- production acceptance of learned routing/cache-affinity tuning and activation of a real remote Judge/Escalation model call.
