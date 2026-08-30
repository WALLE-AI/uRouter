# P4-P6 Execution Status

Updated: 2026-08-30

P4, P5 and P6 are `code_complete`. This means repository implementation gates
pass; it does not claim that production traffic, organizational approval, or a
multi-cloud release exercise happened locally.

## P4 Evaluation

| Task | Status | Evidence |
|---|---|---|
| P4-01 record quality | code_complete | `urouter-eval::build_dataset`; incomplete/cross-revision records quarantine |
| P4-02 privacy | code_complete | metadata-only/body-retained/redaction/consent gates and deterministic audit sample |
| P4-03 exploration | code_complete | default-off Gateway epsilon policy with explicit consent, budget cap, eligible set and propensity |
| P4-04 export | code_complete | tenant/task/time/deletion filters, manifest/hash and `urouter-lab dataset` |
| P4-05 benchmark | code_complete | daily/tool/math/code/long-context categories, quality/cost/latency summary |
| P4-06 counterfactual | code_complete | IPS/SNIPS/DR, variance, 95% CI and ESS |
| P4-07 support domain | code_complete | semantic/input/tool/sample support gates in eval and artifact infer |
| P4-08 feedback defense | code_complete | source allowlist/weight, rate, disagreement and value-bound quarantine |

External acceptance still requires an authorized one-week dataset, privacy
sampling review, and meaningful effective sample size from real exploration.

The first commercial-provider baseline is stored under `eval/real`: 5/5 reviewed
SiliconFlow Gateway cases passed, with 6,621.4 ms mean upstream latency, 3,227 ms P50,
21,275 ms P95 and 1,854,567 nano-USD total cost. Category summaries expose the math
long tail. It covers daily, tool and math only, so it is evidence for the E2E path but
deliberately does not satisfy the five-category release gate.

`urouter-lab gateway-benchmark` now executes schema-validated five-category suites
against a running Gateway. It requires explicit request and nano-USD caps, rejects
streaming and non-auto cases, writes machine-readable checks before returning a failed
gate, and retains no response body or provider credential. The initial reusable suite
is `eval/suites/siliconflow-five-category.json`.
The runner also has a local HTTP end-to-end test covering all five categories, decimal
USD-to-nano accounting, routing checks and response-body non-retention.

A live five-category SiliconFlow run initially passed 4/5 and exposed a real
long-context quality failure on the efficient model. A same-input capable control
passed. The Route now has a configurable input-token quality threshold (2,048 for the
SiliconFlow route); the no-hint policy retest selected capable, emitted
`structural_decider/long_context_quality`, and passed. The post-policy five-category
summary is 5/5, 8,979.6 ms mean, 5,697 ms P50, 21,821 ms P95 and 3,723,882 nano-USD.
This remains single-sample evidence, not production acceptance.

## P5 Artifact Release

| Task | Status | Evidence |
|---|---|---|
| P5-01/P5-02 schema/export | code_complete | revision-bound deterministic artifact, SHA-256, HMAC and seven gates; `urouter-lab train/verify` |
| P5-03 online infer | code_complete | six-operation integer policy, support-domain rejection and rule fallback |
| P5-04 slots | code_complete | active/candidate/last-good, startup verification, promote/rollback/kill APIs |
| P5-05/P5-06 rollout | code_complete | shadow evidence, stable tenant/task bucket, minimum samples, dynamic rollout API |
| P5-07 rollback | code_complete | quality/error/cost/latency threshold observation API and audited auto rollback |
| P5-08 drill | code_complete | rollout supports shadow, 100, 500 and 1000 basis-point stages with admin approval |
| P5-09 bounded step | code_complete | depth/call/cost/cancel and recursive-Judge guard |

External acceptance still requires real shadow and 1%/5%/10% observation
windows, independent approval, and rollback drills under production telemetry.

## P6 Protocol And Embedding

| Task | Status | Evidence |
|---|---|---|
| P6-01/P6-03 IR/handoff | code_complete | tool/image/reasoning/usage/finish IR, explicit loss report and reject policy |
| P6-02 transport | code_complete | OpenAI Chat plus non-stream Responses and Anthropic provider request/response adapters |
| P6-04 entries | code_complete | non-stream `/v1/responses` and `/v1/messages` reuse Chat governance/execution |
| P6-05 client | code_complete | replay-safe address failover; unsafe and started streams are never replayed |
| P6-06 embed | code_complete | Gateway `RouteConfig::decide` facade and corpus parity test |
| P6-07 release | code_complete | public API/semver policy, package READMEs and migration boundaries |

Streaming Responses/Anthropic provider handoff is deliberately rejected. It is
a documented compatibility boundary, not a silent semantic downgrade. External
acceptance requires publishing the crates and a downstream integration exercise.
