# Iteration Verification Reports

Point-in-time records of what was exercised, against which endpoint, on a given
date. They are archived evidence, not status.

**These reports do not track completion.** The authoritative execution status is
[`docs/requirements-traceability.md`](../requirements-traceability.md); a report
here describes one run and is never updated afterwards. Where a report and the
traceability matrix disagree, the matrix wins.

Each report names the endpoint and date it was produced against, so a claim in
one can be re-checked or found stale. Several were produced against local vLLM
endpoints whose ports have since moved.

| Report | Subject |
|---|---|
| `uRouter_核心功能收敛迭代验证报告.md` | M0 core convergence |
| `uRouter_Task-aware_Auto迭代验证报告.md` | Task-aware Auto routing |
| `uRouter_SessionBinding迭代验证报告.md` | Contract v2 conversation/branch binding |
| `uRouter_AionUI_WorkBuddy适配器迭代验证报告.md` | Agent product adapters |
| `uRouter_Redis多实例状态迭代验证报告.md` | Redis multi-instance state |
| `uRouter_Redis共享熔断迭代验证报告.md` | Shared circuit state |
| `uRouter_共享记录反馈状态迭代验证报告.md` | Shared DecisionRecord and feedback |
| `uRouter_双实例故障重启一致性迭代验证报告.md` | Two-instance failure and restart consistency |
| `uRouter_管理面RBAC审计与密钥轮换迭代验证报告.md` | Management RBAC, audit and key rotation |
| `uRouter_本地Qwen_vLLM真实端点测试报告.md` | Local Qwen3.5-4B vLLM endpoint |
| `uRouter_本地Qwen3.8-27B_vLLM真实端点测试报告.md` | Local Qwen3.8-27B vLLM endpoint |
| `uRouter_SiliconFlow真实模型路由验证报告.md` | SiliconFlow commercial routing |
| `uRouter_智能分级路由测试执行报告.md` | Tiered routing test execution |
