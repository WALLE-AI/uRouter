# P2/P3 Execution Status

Last updated: 2026-08-30

Status definitions follow `docs/requirements-traceability.md`. `code_complete`
means the repository gates pass; it does not claim production acceptance.

## P2 Reliability And Control Plane

| ID | Status | Implementation and evidence | External acceptance still required |
|---|---|---|---|
| P2-01 | code_complete | Memory/Redis atomic in-flight, RPM and weighted TPM quota; cancellation and settlement tests | Redis dual-instance limit exercise |
| P2-02 | code_complete | Fixed-point worst-case reserve, idempotent settle/release, stream and missing-Usage semantics | Paid provider invoice reconciliation |
| P2-03 | code_complete | Credential, lifecycle admission, region, residency, tenant and quota filters emit machine reasons | Production policy review |
| P2-04 | code_complete | Weighted, least-loaded, latency and quota-usage pickers with deterministic tests | Production signal tuning |
| P2-05 | code_complete | Error-typed fallback chains, cycle validation and real timeout fixture | Provider-specific error mapping review |
| P2-06 | code_complete | Downstream cancellation, partial-stream failure and conservative accounting tests | Network fault exercise |
| P2-07 | code_complete | `/health/live`, `/health/ready`, drain state and bounded graceful shutdown | Orchestrator termination exercise |
| P2-08 | code_complete | Low-cardinality cumulative histograms for duration, TTFT, cost and fallback; OpenMetrics trace exemplar | Dashboard and alert provisioning |
| P2-09 | code_complete | `urouter-xtask chaos` and the CI report artifact cover Redis restart, timeout and stream faults | Local run is pending because Docker is unavailable on this host |
| P2-10 | code_complete | Per-domain fail-open/fail-closed contract and `docs/redis-consistency-matrix.md` | Redis HA/ACL/TLS drill |
| P2-11 | code_complete | Signed Catalog/Route control manifest, required revision readiness, last-good/fail-closed, atomic snapshot and rollback tests | Multi-Gateway distribution outage drill |

## P3 Catalog Lifecycle And Semantic Routing

| ID | Status | Implementation and evidence | External acceptance still required |
|---|---|---|---|
| P3-01 | code_complete | One `ProviderDriver` contract covers OpenAI models, Bailian pagination and reviewed static inventories; auth, throttling and immutable raw evidence | Controlled non-SiliconFlow cloud discovery |
| P3-02 | code_complete | Reviewed projections require field-level pricing, capability, compatibility, lifecycle and source evidence | Provider-owner review of real facts |
| P3-03 | code_complete | Sequential bounded text/tool/JSON/reasoning probes require an explicit nano-USD budget and retain only hashes/status/latency | Deliberately authorized paid probe |
| P3-04 | code_complete | Immutable candidate hash, JSON Schemas, conflict/alias/retirement checks and quarantine-by-default test | Review workflow adoption |
| P3-05 | code_complete | Catalog, Catalog manifest and control manifest stage before control-last commit; previous bundle rollback and HMAC tests pass | Process-kill/filesystem fault injection |
| P3-06 | code_complete | Revision/ETag, periodic validated reload, per-request `Arc` snapshot, last-good and rollback tests | Multi-process rollout observation |
| P3-07 | code_complete | RBAC/audit protected status, refresh and rollback management APIs; stable conflict tests | Management client integration |
| P3-08 | code_complete | API-key env, externally rotated OAuth bearer file and ambient-token env auth plans; secrets reread per request and redacted errors | Cloud identity/token-agent integration |
| P3-09 | code_complete | Two-phase retirement: stop new requests, allow bound task/session until deadline, then Catalog retirement; boundary tests pass | Production grace-period policy |
| P3-10 | code_complete | Rule-first semantic classification with confidence and abstention; greeting/equation/unknown tests pass | Multilingual evaluation expansion |
| P3-11 | code_complete | Weather declares `requires_tools`, required host tool and capable-model requirement; missing tool is a stable pre-upstream error | Real weather tool registration |
| P3-12 | code_complete | Host tool disclosure and tool-result continuation retain capable routing; Gateway never executes the tool | Agent Host end-to-end run |

## Verified Gates

- Complete workspace test set in a single command: 247 passed,
  0 failed, 7 Redis-conditional ignored. A single workspace invocation on this
  host can select an older test executable blocked by Windows Application
  Control, so the affected packages are rebuilt and run separately.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- Gateway binary tests: 81 passed, 5 Redis-conditional ignored.
- Gateway library tests: 22 passed, 2 Redis-conditional ignored.
- Catalog/provider tests: 12 passed.
- Catalog control manifest bootstrap plus Gateway dry-run: pass, 7 models,
  2 tiers and 2 deployments.
- Provider registry validation: pass, 3 instances.
- Live no-upstream checks on the completed Gateway at `127.0.0.1:8790`: greeting
  selected efficient; equation and weather-with-tool selected capable; weather
  without the Host tool returned HTTP 400 `missing_required_tool`; Catalog ETag
  matched the active revision.

The ignored Redis contracts are executed by `urouter-xtask chaos` when Docker
and Redis are available. Commercial provider calls are intentionally not made by
the test suite because they require explicit spend authorization and credentials.
