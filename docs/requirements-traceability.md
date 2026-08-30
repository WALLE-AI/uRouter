# uRouter Requirements Traceability

Status values:

- `accepted`: implementation and external acceptance evidence exist.
- `code_complete`: implementation gates pass; production exercise is outstanding.
- `partial`: part of the requirement exists, but its exit gate is not met.
- `planned`: no accepted implementation exists.

This matrix is the authoritative execution status. A design document statement or
the existence of code alone is not completion evidence.

| Requirement | Design reference | Status | Current evidence | Next task |
|---|---|---|---|---|
| Catalog facts and pricing | Design sections 5.1-5.5 | code_complete | `crates/urouter-ai`, catalog tests | P3-02 |
| API-key endpoint planning | Design section 5.6 | code_complete | `urouter-ai` auth and endpoint plans | P15-02 |
| OAuth and ambient credentials | Design section 5.6 | code_complete | externally rotated OAuth bearer-file and ambient-env plans; per-request reread and redacted errors | cloud identity integration |
| Compatibility facts | Design section 5.7 | code_complete | `Compat`, catalog validation | P6-02 |
| Catalog discovery | Design section 5.8 | code_complete | OpenAI, Bailian and reviewed-static drivers share immutable evidence, auth and throttling contracts | controlled non-SiliconFlow discovery |
| Catalog candidate/review/publish | Design section 5.8 | code_complete | field-evidence review, quarantine schemas, control-last publish, HMAC and rollback tests | real provider-owner review |
| Catalog hot refresh | Design sections 5.8, 15.9 | code_complete | revision/ETag, periodic validation, atomic per-request snapshot and last-good/fail-closed policies | multi-Gateway rollout drill |
| Provider-neutral deployments | Design section 5.9 | code_complete | typed provider/credential scope, region/residency/tenant/quota/lifecycle fields, API family from Catalog, route revision binding and filter tests | multi-cloud production metadata review |
| Cross-provider message handoff | Design section 5.10 | code_complete | `urouter-protocol` normalized IR, explicit loss policy, OpenAI Responses/Anthropic non-stream transports and conversion tests | cross-cloud staging drill |
| Versioned FeatureFrame | Design section 6 | code_complete | `urouter-contracts` v1 is shared by Gateway records, eval, artifact infer and Embed | schema evolution review |
| Lazy semantic features | Design section 6.3 | code_complete | rule-first greeting/weather/equation classifier with confidence and abstention | multilingual evaluation expansion |
| Feature normalization parity | Design section 6.4 | code_complete | Gateway and Embed call the same `FeatureFrame::from_openai_chat`; corpus parity test | production corpus expansion |
| Rule and signal decision cascade | Design section 7 | code_complete | pin/signal/quality/cost/default layers emit selected/abstain trace; signal confidence test | learned signal-weight calibration |
| Model/Judge/Escalation deciders | Design section 7.2 | code_complete | signed linear model decider plus bounded depth/call/cost/cancel/recursive-Judge step contract and rule fallback | remote Judge remains disabled until approved |
| Cascade abstention and trace | Design sections 7.1-7.4 | code_complete | Explain is explicitly summary; persisted v2 trace is full with cascade, candidate, policy, artifact/exploration and runtime deployment evidence | trace backend integration |
| Capability filter | Design section 8 | code_complete | catalog admission and tier selection | P1-05 |
| Quota/budget/policy filters | Design sections 8, 11 | code_complete | atomic Memory/Redis quota and budget plus credential, region, residency, tenant and deployment lifecycle reasons | production policy review |
| Weighted picker | Design section 8.4 | code_complete | pure deterministic `CapacityLeasePlan` combines availability, Half-Open probe reservation and weighted selection; CapacityManager parity tests pass | P1-05 |
| Latency/load/quota pickers | Design sections 8.4-8.5 | code_complete | configurable deterministic weighted, least-loaded, latency and quota-usage tests | production signal tuning |
| Typed retry | Design section 9.2 | code_complete | transient-only directives, bounded backoff, complete attempts and stable error taxonomy | provider mapping review |
| Circuit and Half-Open | Design sections 9.3-9.4 | code_complete | pure local cooldown/lease directives plus local and Redis circuit repositories | P2-09 |
| Typed fallback chains | Design section 9.5 | code_complete | per-error chains, depth/cycle checks and timeout-specific integration test | provider mapping review |
| RouterArtifact schema | Design section 10 | code_complete | `urouter-artifact`: revision binding, content hash, HMAC, seven export gates and bounded integer infer tests | signer/HSM integration |
| Artifact shadow/canary/rollback | Design section 10.4 | code_complete | active/candidate, shadow evidence, task-stable canary, rollout API, kill switch, hard-threshold auto/manual rollback tests | staged production observation windows |
| Exact actual cost | Design section 11 | code_complete | fixed-point pricing and DecisionRecord cost | P2-02 |
| Budget reserve and settlement | Design section 11.2 | code_complete | fixed-point worst-case fallback/retry reservation, request-ID deduplication, actual Usage settlement, conservative missing-usage accounting, Memory tests and conditional Redis cross-instance contract | external Redis/provider exercise |
| Cache-affinity routing | Design section 11.3 | code_complete | bounded prompt-profile to successful-deployment affinity reorders eligible candidates, emits `cache_affinity_hit`, and exact session binding preserves model/API identity | provider cache telemetry tuning |
| Memory and Redis binding state | Design section 12 | code_complete | binding/circuit/idempotency/DecisionRecord/quota ports with memory/Redis adapters | P1-08 |
| Shared circuit state | Design section 12 | code_complete | Redis circuit gate | P2-10 |
| Per-state failure matrix | Design section 12.3 | code_complete | correctness domains fail closed; metrics/latency domains fail open; documented matrix and contract test | Redis HA drill |
| DecisionRecord and feedback | Design section 13 | code_complete | v2 context plus created time, semantic task, redaction profile, artifact/exploration evidence; memory/Redis authority and normalization tests | production retention audit |
| Exploration propensity | Design section 13 | code_complete | default-off epsilon exploration requires recording, training consent, explicit authorization and bounded budget; records eligible set and propensity | tenant-authorized traffic exercise |
| Step/Driver model-call unload | Design section 14.1 | code_complete | `urouter-artifact::StepBudget` bounds depth, calls, cost, cancellation and recursive Judge; rule fallback tests | enable a real Judge only after approval |
| OpenAI chat translation | Design section 14.2 | code_complete | request rewrite and streaming proxy | P6-01 |
| Responses/Anthropic translation | Design section 14.2 | code_complete | normalized request IR, `/v1/responses`, `/v1/messages`, provider-side non-stream adapters and output/tool/usage tests | demand-led streaming adapters |
| Cancellation propagation | Design sections 14.3, 15.7 | code_complete | execution lease and stream cancellation tests | P2-06 |
| Startup dry-run checks | Design section 16.1 | code_complete | stable 16-item report now validates configured artifact signature/schema/revisions/tiers and exploration constraints | deployment config regression |
| Dataset build/export | Design section 17.1 | code_complete | `urouter-eval` deterministic export, revisions/privacy/deletion/feedback quarantine and `urouter-lab normalize/dataset` | one-week authorized dataset acceptance |
| Counterfactual evaluation | Design section 17.2 | code_complete | IPS, SNIPS, DR, variance, 95% CI and ESS tests plus CLI | real propensity sample acceptance |
| Artifact export gates | Design section 17.3 | code_complete | deterministic `urouter-lab train`, benchmark/support/privacy/counterfactual gates, HMAC build/verify | independent reproduction and signer review |
| Basic Prometheus metrics | Design section 18.1 | code_complete | counters plus OpenMetrics cumulative histograms | dashboard provisioning |
| Full routing/cost/SLO metrics | Design sections 18.1-18.2 | code_complete | duration, upstream, TTFT, cost and fallback histograms; trace exemplar and low-cardinality test | dashboard and chaos acceptance |
| Health endpoint | Design section 18.3 | code_complete | liveness, dependency/revision readiness and bounded drain tests | orchestrator exercise |
| Catalog/artifact management API | Design section 18.3 | code_complete | RBAC/audit status, promote, rollback, kill, rollout and observation endpoints | operator runbook exercise |
| Control revision coordination | Design sections 15.9, 18.3 | code_complete | signed Catalog/Route manifest, required revision readiness, last-good/fail-closed, rollback | distribution outage drill |
| Semantic tool requirement and Host continuation | Design sections 7, 14 | code_complete | weather requires disclosed Host tool; missing tool rejects; tool-result continuation tested | Agent Host end-to-end run |
| OpenAI-compatible Gateway | Design milestone M0 | accepted | chat, stream, models, explain and smoke evidence | regression gate |
| Multi-instance state | Design milestone M1 | code_complete | Redis continuity and restart evidence | X-02 |
| Evaluation and model profiles | Design milestone M2 | code_complete | governed dataset pipeline, five-category benchmark, IPS/SNIPS/DR and support-domain gates | real authorized dataset acceptance |
| Learned router release | Design milestone M3 | code_complete | signed artifacts, bounded inference, shadow/canary/rollback/kill and management APIs | staged production canary acceptance |
| Embeddable decision facade | Design milestone M4 | code_complete | `urouter-client`, `urouter-embed`, normalized protocol crate and exact core parity test | crate publication and downstream adoption |

## Update Rule

Every status change must add a code reference, an automated test reference, and
acceptance evidence. Production-only requirements remain `code_complete` until the
documented exercise succeeds.
