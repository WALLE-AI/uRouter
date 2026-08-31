# P1 Execution Status

Updated: 2026-08-30

Overall status: `code_complete`, not production `accepted`.

| Task | Status | Evidence | External acceptance still required |
|---|---|---|---|
| P1-01 FeatureFrame and decision contracts | code_complete | Versioned `FeatureFrame` is shared by Gateway, eval, artifact and Embed; parity corpus test | production corpus expansion |
| P1-02 complete traces | code_complete | Persisted full trace includes cascade, candidates, policy/runtime filters and artifact/exploration evidence; Explain is explicitly summary | trace backend integration |
| P1-03 DecisionRecord v2 | code_complete | Four-section v2 context, revisions, idempotency, budget dedup, legacy isolation and deterministic dataset normalization/export | production retention audit |
| P1-04 privacy/exploration fields | code_complete | Recording/redaction/consent, eligible set/propensity and default-off budgeted epsilon exploration | tenant-authorized traffic exercise |
| P1-05 capacity policy extraction | code_complete | Pure capacity plan plus weighted/latency/load/quota pickers, policy filters and local/Redis circuit parity | production signal tuning |
| P1-06 reliability policy extraction | code_complete | Typed errors, bounded retry/cooldown, acyclic fallback plan and stable migration taxonomy remain pure | provider error mapping review |
| P1-07 DecisionCascade | code_complete | Pure pin/signal/quality/cost/default cascade, signed artifact decider and bounded Judge/Escalation step contract | remote Judge activation requires approval |
| P1-08 State ports | code_complete | Memory/Redis binding, circuit, idempotency, DecisionRecord, quota and budget adapters share repository contracts | Redis HA/ACL/TLS drill |
| P1-09 dependency boundary | code_complete | CI and `urouter-xtask check-pure-crates` prevent forbidden I/O dependencies in pure crates | extend the deny list as crates evolve |
| P1-10 configuration dry-run | code_complete | Stable 16-item report validates manifest/auth/Route, artifact signature/schema/revisions/tiers and exploration constraints without opening Redis/listener | deployment configuration regression |
| P1-11 property/fuzz/state tests | code_complete | Generated malformed Catalog/endpoint/cost properties, fallback graph, monotonic cooldown/cost, CAS and quota concurrency tests | expand corpus as policies evolve |

## Current Contract Behavior

`FeatureFrame::from_openai_chat` extracts deterministic request structure and semantic
signals. Tool availability remains distinct from natural-language intent: a weather
request cannot claim access to a weather tool unless the Host disclosed it.

The public Explain response remains an intentional `summary`. The persisted routing
trace is `full`: it retains cascade selection/abstention, Catalog and policy exclusions,
artifact/exploration evidence, per-attempt capacity selection and accumulated runtime
filters even when no upstream attempt is executable.

New DecisionRecords contain a v2 context. Old JSONL and Redis records deserialize with
no context and normalize to `training_eligible=false`; missing historical candidates or
propensity are never invented. `--dry-run` has stable
`pass/fail/warning/not_applicable/blocked` states. Catalog/Route input errors use a
structured `errors` array and configured artifact and exploration checks use the same
contract.

## Verification

- `cargo test -p urouter-contracts --lib`: 17 passed.
- `cargo test -p urouter-gateway --lib`: 21 passed, 2 Redis-conditional ignored.
- `cargo test -p urouter-gateway --bin urouter-gateway`: 81 passed,
  5 Redis-conditional ignored.
- Complete workspace set in a single command: 247 passed, 0 failed,
  7 Redis-conditional ignored. The affected packages are rebuilt separately because
  Windows Application Control blocks an older cached test executable on this host.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `urouter-xtask check-pure-crates`: passed.

## Quota Boundary

`--tenant-max-in-flight`, rolling RPM and weighted TPM use atomic tenant-scoped
admission. TPM reserves an estimate and settles from actual Usage when available;
missing Usage remains conservative. Only the concurrency permit is released.
Exhaustion uses typed stable HTTP 429 errors, while Redis admission failure fails closed
as HTTP 503. Provider tokenizer calibration, two-instance Redis exercise and long-stream
pressure validation are external acceptance work; the configured quota lease TTL must
exceed the longest expected request or stream.
