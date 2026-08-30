# Real Evaluation Evidence

This directory contains machine-readable summaries derived only from completed
commercial-provider calls. It must not contain credentials, raw retained prompts, or
provider response bodies.

`siliconflow-2026-08-30-cases.json` is derived from
`uRouter_SiliconFlow真实模型路由验证报告.md`. Quality is a manually reviewed binary
task-success score: `1_000_000` means the observed route and answer/tool behavior met
the case expectation. It is not a judge-model score.

The initial baseline covers daily, math, and tool cases. It does not satisfy the
five-category artifact release gate because code and long-context cases are absent, and
it has no repeated samples for percentile or confidence-interval estimates.

The `siliconflow-five-category-live-*` files retain the first live five-category run,
including its long-context failure. `siliconflow-long-context-capable-*` is the
same-input capable control. `siliconflow-long-context-policy-*` proves the configurable
automatic threshold, and `siliconflow-five-category-policy-*` is the post-policy 5/5
summary. These are single-sample functional artifacts and must not be treated as a
production confidence interval.

Regenerate the summary with:

```powershell
cargo run -q -p urouter-lab -- benchmark `
  --cases eval/real/siliconflow-2026-08-30-cases.json `
  --output eval/real/siliconflow-2026-08-30-summary.json
```

Run the five-category suite against an already started Gateway with an explicit spend
cap. The runner never reads the provider credential and does not retain response bodies:

```powershell
cargo run -q -p urouter-lab -- gateway-benchmark `
  --suite eval/suites/siliconflow-five-category.json `
  --base-url http://127.0.0.1:8787/v1 `
  --max-cost-nano-usd 10000000 `
  --cases-output eval/real/latest-cases.json `
  --report-output eval/real/latest-run.json
```
