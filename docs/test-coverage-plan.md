# Test Coverage Plan

Updated: 2026-09-01

Measured with `cargo llvm-cov --workspace`. Test *counts* are not used to rank
work here: they mislead. `urouter-infer` has zero tests of its own but 84.29%
line coverage through `urouter-lab`, while `urouter-protocol` has three tests and
291 uncovered lines. Only measured coverage and risk rank the work below.

## Baseline

**77.93%** (27729 lines, 6121 uncovered), measured with Redis available and with
`urouter-smoke`/`urouter-soak` excluded. **All phases are complete.**

| After | Total | Gate | Tests |
|---|---:|---:|---:|
| Phase 0 (measurement only) | 73.40% | 72 | 247 |
| Phase 1 (`urouter-infer`, transport guard) | 73.88% | 72 | 270 |
| Phase 2 (`urouter-protocol`) | 75.13% | 74 | 289 |
| Phase 3 (gateway request path) | 77.12% | 76 | 300 |
| Phase 4 (governance and retention) | 77.60% | 77 | 311 |
| Phase 5 (`provider_sync` error paths) | **77.93%** | **78** | **319** |

### Why the first measurement was wrong

Measured without Redis the workspace reads 66.50%, because the seven Redis
contracts are `#[ignore]`d by default and the whole shared-state layer goes
unexercised:

| File | Without Redis | With Redis |
|---|---:|---:|
| `gateway/src/shared_state.rs` | 0.00% | 87.03% |
| `gateway/src/quota.rs` | 77.87% | 93.32% |
| `gateway/src/idempotency.rs` | 85.71% | 93.51% |
| **workspace total** | **66.50%** | **73.40%** |

`shared_state.rs` at 0.00% was a measurement artifact, not a gap. Any ranking
built on the Redis-less numbers is wrong, which is why Phase 0 came first.

### Uncovered lines by risk

| File | Uncovered | Coverage | Risk |
|---|---:|---:|---|
| `urouter-gateway/src/main.rs` | 2471 | 64.19% | request path |
| `tools/urouter-catalog/src/provider_sync.rs` | 1457 | 34.81% | offline tool behind human review |
| `tools/urouter-lab/src/main.rs` | 608 | 35.25% | offline tool |
| `tools/urouter-smoke/src/main.rs` | 523 | 6.94% | CLI against real endpoints |
| `tools/urouter-soak/src/main.rs` | 453 | 5.62% | CLI against real endpoints |
| `urouter-gateway/src/shared_state.rs` | 424 | 0.00% | measurement artifact |
| `urouter-protocol/src/lib.rs` | 291 | 73.50% | cross-provider semantics |
| `tools/urouter-xtask/src/chaos.rs` | 193 | 31.32% | Docker orchestration |
| `urouter-artifact/src/lib.rs` | 181 | 81.51% | signed policy |
| `urouter-gateway/src/persistence.rs` | 166 | 49.85% | governance and retention |

Well covered, no work planned: `urouter-contracts` 96.75%, `urouter-embed`
94.99%, `urouter-gateway/src/management_auth.rs` 94.10%, `urouter-types/money.rs`
93.01%, `urouter-core` 91.00%, `urouter-eval` 90.01%.

## Phase 0 — Make coverage trustworthy

Every later priority depends on this.

1. Add a `coverage` CI job with the same Redis service as `redis-contracts`, and
   run `cargo llvm-cov --workspace --include-ignored` under
   `UROUTER_TEST_REDIS_URL`.
2. Set `--fail-under-lines` to the *measured* baseline minus one point. The gate
   exists to prevent regression, not to force test-writing. A high threshold set
   up front produces tests written for the counter, which is worse than no test:
   it turns the gate green without finding defects. Raise it by hand as each
   phase lands.
3. Exclude `urouter-smoke` and `urouter-soak` from the measurement. Their value
   is exercising real endpoints; their low coverage is correct, and leaving them
   in permanently depresses the total and masks real regressions.

Exit: CI publishes a Redis-inclusive figure, `shared_state.rs` is no longer
0.00%, and the regression gate is active. **Done 2026-09-01.** The `coverage` job
runs `cargo llvm-cov` against a Redis service with `--fail-under-lines 72`
against a measured 73.40% baseline, and uploads `lcov.info`.

## Phase 1 — Safety assertions with no test

Code that makes a safety decision or parses untrusted input, with nothing
proving it works.

### 1.1 `provider_request` transport guard and error mapping

An earlier draft claimed `reject_non_chat_stream` was untested, based on a
literal name search of the test file. That was wrong: function-level coverage
shows `count=3`. It is reached through `provider_request`, which
`provider_transports_convert_non_stream_requests_and_responses_without_guessing`
exercises. Grepping for a name is not a coverage measurement.

What is genuinely uncovered in that function is its four error-mapping closures,
all `count=0`:

| Test | Assertion |
|---|---|
| `responses_transport_rejects_streaming_before_conversion` | the existing test only reaches the guard on the Anthropic branch; the Responses branch is unexercised |
| `provider_request_maps_a_conversion_failure_to_protocol_conversion_failed` | malformed Chat input, both non-Chat transports |
| `provider_request_maps_semantic_loss_to_protocol_semantic_loss` | content the target cannot carry under `LossPolicy::Reject` |

The third matters most: `to_openai_responses`/`to_anthropic_messages` are called
with `LossPolicy::Reject`, so this is the boundary where a request that would
silently lose tools or images is refused instead.

**Done 2026-09-01.** All three tests are in `gateway/src/tests.rs`. The loss test
drives a message carrying `reasoning_content`, which neither target can represent,
and asserts `protocol_semantic_loss` with HTTP 400 on both transports.

### 1.2 `urouter-infer` error variants

`crates/urouter-infer/src/lib.rs` parses ONNX protobuf — an external artifact
that warrants defensive validation — and defines seven error variants, none
asserted. The only existing path is a happy-path `from_model` call in
`urouter-lab`. `OnnxRouter::load` is the only public function in the workspace
that no test ever executes.

One test per variant: `MissingGraph`, `UnsupportedGraph`, `MissingTensor`,
`InvalidTensor` (wrong type, and dims disagreeing with value count),
`InvalidShape`, `NumericOverflow`, `Decode`. Plus `zero_hidden_units_do_not_panic`
— a zero-length `hidden_bias` currently passes validation and degrades `score`
to returning `output_bias`; that behaviour should be decided and pinned rather
than left incidental.

Exit: `urouter-infer` ≥ 95% lines; every error variant has a named test.
**Done 2026-09-01: 98.86%**, 13 tests. `OnnxRouter` gained `Debug`/`Clone`/`PartialEq`
so error assertions can use `unwrap_err`. `zero_hidden_units_degrade_to_the_output_bias_without_panicking`
pins the previously incidental zero-width behaviour.

## Phase 2 — `urouter-protocol`

The weakest library crate and the only implementation of cross-provider
translation. A defect here presents as silent semantic loss: the request is sent,
the tool definitions are gone, model behaviour changes, and nothing errors.

Function-level coverage shows unexecuted branches in `parse_openai_tools`,
`parse_responses_tools`, `from_anthropic_messages`, `anthropic_content`,
`anthropic_messages`, `to_anthropic_messages`, `to_openai_responses` and
`from_openai_responses`. The Anthropic direction has essentially one happy path.

1. **Conversion matrix.** One shared corpus (plain text, multi-turn,
   system/user/assistant, tool definitions, tool calls, tool results, image
   parts, reasoning, structured output, extension fields) driven through
   `from → to → from` for all three protocols, asserting IR equality.
2. **Loss is explicit.** One test per `LossKind`; a strict policy rejects rather
   than drops; and — most importantly — a permissive policy still *reports* what
   it tolerated. Loss that is dropped without a report leaves no evidence on the
   DecisionRecord and cannot be attributed afterwards.
3. One test per `ProtocolError` variant.

Exit: ≥ 90% lines; every `LossKind` and `ProtocolError` has a named test.
**Done 2026-09-01: 96.88% lines** (73.50% before), 3 tests to 22.

What the tests pinned beyond coverage:

- `allow_documented_still_reports_every_loss_it_tolerates` drives all five
  `LossKind`s through one request and asserts each is reported *and* that every
  loss carries a non-empty path and detail. A tolerated loss that is not
  reported leaves nothing on the DecisionRecord to attribute later.
- `anthropic_substitutes_a_default_max_tokens_when_the_request_omits_one` pins
  the invented `max_tokens: 1024`. Anthropic has no optional output bound, so
  the converter must supply one; that silently changes provider behaviour and
  was previously an untested implementation detail.
- `responses_tool_items_are_not_reparseable_and_say_so` documents a real
  boundary rather than papering over it: `to_openai_responses` emits
  `function_call`/`function_call_output` items that carry no `role`, so
  reparsing fails by design. Pinning it stops the asymmetry being mistaken for
  a defect later.
- `anthropic_skips_unknown_content_blocks_instead_of_failing` pins the opposite
  choice on the input side — Anthropic adds block types independently of
  uRouter, so unknown blocks are skipped rather than rejected.

A blanket `from → to → from` identity test was deliberately not written: it does
not hold, and asserting it would have required weakening it until it proved
nothing. The round trips are asserted per protocol against what each actually
preserves.

## Phase 3 — Gateway request path

Largest absolute gap, but coverage must not be chased blindly: much of the 6900
lines is CLI parsing and startup builders.

Classify unexecuted functions first, then write tests:

- **A, must cover**: error handling, admission, state transitions (rejection,
  degradation, cancellation, settlement).
- **B, optional**: data shuffling and serialization.
- **C, skip**: argument parsing and startup builders — already covered by
  `--dry-run` and the `check-deploy` gate.

Known A-class candidates: the management RBAC/audit matrix (2 of 67 integration
tests), artifact rollout and canary state transitions (1 of 67), and conservative
accounting for failures after the first streamed token.

Also in scope: a `warn` when `commit_task_binding` returns early because of
`compatibility_mode`. The behaviour is documented (`docs/gateway-m0.md`), but the
failure is silent — an integrator who omits `x-urouter-tenant-id` gets a Gateway
that looks healthy while session continuity does not hold. Verified by hand on
2026-08-31.

Exit: all A-class functions covered; `main.rs` ≥ 75%. 90% is not the target.
**Done 2026-09-01: 75.95%** (64.19% before).

Classification result: of 45 never-executed functions, 15 were C-class startup
builders (`main`, `serve_with_drain`, `build_*_repository`, `load_control`,
`spawn_control_reloader`) already covered by `--dry-run` and `check-deploy`, and
were deliberately left alone. The remaining 30 were A-class and are now covered:

- **Artifact management, 9 functions, all previously unexecuted.** This is the
  entire M3 rollout control surface — status, promote, rollback, kill switch,
  rollout update, observation. Tests cover the lifecycle, the Admin-only RBAC
  boundary (`status` is the only Reader-accessible endpoint), the two typed
  conflicts when there is no candidate and no last-good, an invalid rollout, and
  the 404 a Gateway started without `--artifact-active` must return instead of an
  empty status that reads healthy.
- **`/v1/responses` and `/v1/messages` entrypoints.** Only the inner
  `provider_request` was covered; the entrypoints themselves were unexecuted.
- **Feedback ingest** including every documented signal kind and all six typed
  rejections.
- **Binding and session lifecycle endpoints**, including that deleting an absent
  binding reports 404 rather than 204 — the endpoint distinguishes "removed
  something" from "there was nothing".
- **`/v1/tiers` and `/v1/stats`.**

`commit_task_binding`'s silent early return is now `binding_skip_reason`, which
names the cause. `compatibility_mode` logs at `warn` with the missing contract
fields spelled out; the other two reasons are deliberate caller choices and log
at `debug`.

## Phase 4 — Governance and retention

`persistence.rs` 49.85%, `record.rs` 56.06%, `urouter-artifact` 81.51%.

`persistence.rs` holds retention TTL enforcement, rotation and tenant/task
deletion. These are governance assertions — "a record must not outlive its tenant
TTL" — where a defect is a compliance failure rather than a functional one.

**Done 2026-09-01: `persistence.rs` 87.50%** (49.85% before), `record.rs` 89.39%
(56.06% before), 11 tests.

- Retention expiry is inclusive at the deadline, asserted at the epoch, at now,
  and at the 365-day maximum.
- Rotation keeps exactly one previous generation: at the size boundary it does
  not rotate, one byte over it does, and a second rotation replaces `.1` rather
  than accumulating `.2`. Without the single-generation rule a deleted record
  could survive in an arbitrarily old copy.
- Rewrites — the path by which deletion reaches disk — replace the live file and
  drop the rotated copy, so a deleted record is not still readable in the
  previous generation.
- Restart replay drops records that expired while the process was down *and*
  persists the pruned file, so they do not return on the next start.
- `normalize_replayed_record` fills only the two fields a pre-governance record
  legitimately lacks and invents nothing else; consent and an existing deadline
  are left exactly as written.
- Deletion is generation-guarded, so a request in flight when a tenant or task
  was deleted cannot resurrect it. Tenant scoping is asserted in both directions.
- `store_record` attaches the override verdict and counts `paired` versus
  `paired_rejected`. `evaluate_override` was already tested; the wiring that
  stores its verdict and feeds value attribution was not.

`spawn_retention_sweeper` ran its body on a 60-second timer, which meant the
periodic TTL sweep could only be exercised by waiting out the interval — in
practice, never. The body is now `sweep_expired_records`, asserted directly: it
prunes the expired record, tombstones the vector that record referenced, and
leaves the surviving record's vector readable.

## Phase 5 — `provider_sync.rs`

Largest single block (1457 uncovered) but lowest urgency: its output passes human
review, candidate quarantine and control-last publish before reaching the active
Catalog, so defence in depth already exists. Cover only driver error and
throttling paths — pagination interruption, rate-limit backoff, auth failure,
malformed response. No overall target.

**Done 2026-09-01: 12 tests to 20.** File coverage moved 34.81% to 44.50%, which
is the expected outcome: the untouched majority is CLI plumbing for the publish
and review workflow, and chasing it was explicitly out of scope.

The goal was that a provider behaving badly is refused with a typed reason rather
than yielding a partial inventory that reads as complete:

- 401 / 429 / 500 surface as `SyncError::Http` carrying the status and body. An
  empty inventory would read as "this provider has no models" and retire every
  model it publishes.
- A 200 whose body is not the documented shape is refused rather than parsed into
  an empty list. Same for malformed Bailian pages, asserted by shape.
- A configured credential that is absent fails *before* the request is sent, so
  no unauthenticated call reaches a paid endpoint.
- Pagination terminates: a provider that keeps claiming more pages hits
  `PaginationLimit`, and a short page stops early instead of spending the
  remaining page budget.
- The reviewed-static path reports missing and malformed files as typed errors.

The workspace forbids `unsafe_code` and `env::set_var` is unsafe in edition 2024,
so the credential tests borrow `PATH` rather than mutating the environment. Only
the presence of a credential matters to those assertions, not its value.

## Explicitly not doing

| Not doing | Reason |
|---|---|
| Unit tests for `urouter-smoke` / `urouter-soak` | Their purpose is hitting real endpoints; ~6% is correct. Exclude from measurement instead. |
| Tests for `xtask/chaos.rs` | Almost entirely Docker orchestration; the CI `p2-chaos` job verifies it end to end. |
| Weak tests written to reach a number | See Phase 0.2. |
| `main.rs` to 90% | Testing CLI and builder code is negative return. |

## Acceptance

Coverage is expected to move from 66.50% to roughly 78–82%, but the meaningful
criteria are:

1. Every safety and correctness assertion — the stream guard, seven
   `InferError` variants, five `LossKind`s, five `ProtocolError`s — has a named
   test.
2. Coverage is measured with Redis available; `shared_state.rs` is not 0.00%.
3. A regression gate exists and its threshold is raised as each phase lands.

Status is tracked in [`requirements-traceability.md`](requirements-traceability.md).
