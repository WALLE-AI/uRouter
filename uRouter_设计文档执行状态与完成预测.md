# uRouter 设计文档执行状态与完成预测

> 更新日期：2026-08-31
> 口径：以 `uRouter_技术架构深度评审与优化方案.md` 的收敛里程碑为执行基线，
> 不把原设计中的全部设想误算为首个可发布版本。

## 当前结论

- `urouter-ai` 事实与计价内核 MVP：已完成。
- 单实例 Agent-aware Auto Gateway 工程核心：100%，SessionBinding、产品适配入口与可执行 SLO 门禁已完成。
- 优化后 M0 发布候选：uRouter 自身边界已完成；AionCore 动态头注入按当前决策不作为完成条件。
- P2 可靠性/控制面与 P3 Catalog 生命周期/语义路由：仓库内代码任务全部 `code_complete`，外部验收见下文。
- 原设计 M0-M4 的仓库内实现任务均达到 `code_complete`。这不等于生产 `accepted`；剩余工作是
  Redis HA/ACL/TLS、授权真实数据集、真实 shadow/canary 观察、跨云演练、crate 发布与凭证审批等外部验收。

这不是承诺日期。若要求多实例 Redis、训练评估、learned routing、canary 和嵌入形态全部达到
生产验收，必须按阶段取得真实流量和发布权限，不能靠代码实现单独宣告完成。

## 里程碑状态

| 里程碑 | 状态 | 已完成 | 主要剩余 |
|---|---:|---|---|
| `urouter-ai` MVP | 100% | catalog、能力、compat、endpoint、精确计价、证据、CLI | 云端付费 smoke 为外部可选验证 |
| M0 单实例核心 | 100% | Auto、stream、cost、record、retry、capacity、task/session continuity、tenant、TTL、delete、explain、RBAC/audit、adapter、soak/SLO | 无 uRouter 内部阻断项 |
| M1 多实例可靠性 | 约 95% | Redis binding/circuit/record/feedback/quota/budget、全局 Half-Open、控制 revision、AOF 重启与故障恢复 | Redis HA/ACL/TLS 生产演练 |
| M2 评估与画像 | code_complete | 网关记录规范化、数据集隔离/导出、五类 benchmark、IPS/SNIPS/DR、置信区间、ESS、支持域 | 一周授权真实数据验收 |
| M3 学习与发布 | code_complete | 签名 artifact、bounded infer、shadow、task canary、rollout/observe、自动/人工 rollback、kill switch | 真实 shadow 与 1%/5%/10% 观察窗口 |
| M4 嵌入形态 | code_complete | 规范化协议 IR、client failover、Embed/Gateway 同核 parity、公共 semver 契约 | crate 发布与下游接入验收 |

## 本轮完成

1. `data_policy`：`none/metadata_only`、训练/远程 judge 授权、1-365 天 TTL。
2. 可信 tenant header：task、feedback、decision、override 全链路隔离。
3. 启动、查询和后台 sweep 的 TTL 执行。
4. decision/task/tenant 持久删除，覆盖 JSONL、轮转副本、feedback 和 binding。
5. `POST /v1/explain` 无上游 dry-run。
6. P0 阶段基线曾包含 133 个可执行测试；当前全量基线为单条 `cargo test --workspace --no-fail-fast` 的 247 个通过、0 个失败、7 个 Redis 条件忽略，详见第 25 项及各阶段执行状态报告。
7. 真实验证 8087 Qwen3.5-4B 与 Qwen3.8-27B（当前为 19121/starvlm，原端口 8094）；27B Auto 请求、跨租户 404、
   `recording=none` 和删除语义均通过。
8. 完成 GW-500/510：内存/Redis 共享状态端口与原子 CAS；两个真实网关实例验证跨实例 27B continuity。
9. 完成 GW-520/521：共享 deployment/credential/provider circuit、全局 Half-Open 单探针和双实例故障部署跳过。
10. 完成 GW-530：Reader/Operator/Admin 管理端点授权、tenant allowlist、同步脱敏审计和重叠密钥热轮换。
11. 完成 GW-540：双网关与 Redis AOF 重启恢复、连接自动重连、共享 circuit 保留和稳定 503 故障契约。
12. 完成 GW-550：共享 DecisionRecord/feedback 权威状态、TTL、容量裁剪、跨实例反馈合并和 decision/task/tenant 删除。
13. 完成 GW-600：v2 `conversation + branch` 精确 SessionBinding、身份哈希、安全迁移和真实 19121/27B 连续性验证。
14. 完成 GW-610：AionUI/WorkBuddy adapter、自动执行画像哈希、工具能力驱动 27B 路由、主调用连续性和 4B 标题旁路。
15. 完成 GW-620/630：feedback pipeline 批量水合；tenant/task 删除代际阻止旧在途 binding 与 DecisionRecord 复活。
16. 完成 GW-640：`urouter-soak` 5000 请求门禁，0 错误、P95 3 ms、P99 4 ms、RSS 增长 2 MiB；真实 19121/27B 工具调用通过。
17. 完成 P2-01 代码闭环：租户级 max in-flight、滚动 RPM 与加权 TPM 已具备 Memory/Redis 组合原子准入、类型化稳定 429、估算预留、普通/流式 Usage 结算、取消保守记账和指标；逐 Provider tokenizer 校准、Redis 双实例与超长流压力验证归入真实环境验收，不阻塞仓库 `code_complete`。
18. 完成 P2-02 代码闭环：按 fallback/retry 最坏成本进行固定点预算预授权，请求 ID 去重，成功 Usage 结算，缺失 Usage/取消保守记账，Memory/Redis 状态端口及稳定 `402 tenant_budget_exhausted`；真实 Redis/provider 账单对账仍属外部验收。
19. 完成 P2-03～P2-06：完整部署过滤原因、四种 Picker、按错误类型的 fallback、Bad Request 硬停止、流式取消与部分失败保守记账均有自动化测试。
20. 完成 P2-07～P2-08：`/health/live`、`/health/ready`、有界 drain；请求/上游/TTFT/成本/fallback OpenMetrics 直方图和 trace exemplar，不使用 tenant/task/request 常规标签。
21. 完成 P2-09～P2-11 代码闭环：Redis 重启/超时/断流混沌门禁、一致性故障矩阵、HMAC 控制 manifest、required revision readiness、last-good/fail-closed、原子快照和回滚。混沌门禁已从 PowerShell 改写为 `urouter-xtask chaos`；当前主机无 Docker，仍留给 CI/Redis 环境执行。CI 另新增 `redis-contracts` job（带 redis service），使 7 个 Redis 条件测试首次真正在 CI 中执行——此前它们始终被静默忽略。
22. 完成 P3-01～P3-05：SiliconFlow/OpenAI、百炼分页、火山 reviewed-static 统一驱动，显式限速与原始证据；字段级人工证据、费用受限能力探测、候选隔离 JSON Schema、control-last 发布和一键回滚。
23. 完成 P3-06～P3-09：Catalog/Route revision 与 ETag、Gateway 热加载、RBAC/audit 管理端点、OAuth bearer-file/ambient credential、两阶段部署退役和绑定宽限。
24. 完成 P3-10～P3-12：规则优先语义分类、置信度与 abstain；“你好”保持 efficient，方程升级 reasoning-capable，武汉天气强制要求 Host `get_weather` 工具，无工具稳定拒绝，tool-result continuation 保持 capable。
25. 2026-08-31 最新门禁：单条 `cargo test --workspace --no-fail-fast` 得到 247 passed/0 failed/7 Redis 条件忽略；Gateway binary 103 passed、Gateway library 11 passed、xtask 22 passed。全 workspace `cargo fmt --check`、Clippy `-D warnings`、`cargo deny check`、纯 crate 边界、Secret 扫描、定价 fixture 与部署清单门禁全部通过。此前需要分拆执行的原因是 Windows 应用控制缓存封禁旧测试 exe；门禁脚本改写为跨平台的 `urouter-xtask` 后不再需要分拆。
26. 当前完成版运行于 `127.0.0.1:8790`：readiness 200；问候选择 efficient，方程与带 `get_weather` 的天气请求选择 capable，无工具天气请求返回 400 `missing_required_tool`；Catalog ETag 与活动 revision 一致。原 8787 服务未中断。
27. 完成 P4-01～P6-07 仓库内实现：`urouter-eval`、`urouter-artifact`、`urouter-protocol`、`urouter-client`、`urouter-embed` 与 `urouter-lab`；Gateway 增加显式授权 epsilon 探索、签名 artifact 在线控制、Responses/Anthropic 入口及 provider transport。逐项证据和外部验收边界见 `docs/p4-p6-execution-status.md`。
28. 工程基建补齐（此前未列入矩阵的缺口）：
    - 结构化日志。Gateway 此前只有 metrics 和 DecisionRecord，没有任何 `tracing`/`log` 调用，单请求故障无排查依据。现每个请求有 `chat` span（request_id / decision_id / tenant_key / tier / model），每个被拒请求有一行带稳定错误码的日志；请求体与上游错误正文不进日志。
    - 交付形态。新增 distroless `Dockerfile`、双网关 + Redis 的 `docker-compose.yml`、含 drain 一致时序的 `deploy/kubernetes/gateway.yaml`。`urouter-xtask check-deploy` 用 Gateway 自身的 `--dry-run` 校验每一组清单参数。
    - 供应链门禁。新增 `deny.toml` 与 CI job；首次运行即发现 RUSTSEC-2021-0073（`prost-types 0.6.1`），经确认仅经 `onnx-pb` 进入离线工具、不在 Gateway 依赖树中，已用 `wrappers` ban 锁定范围。
    - 发布可行性。`urouter-ai` 的内部依赖缺少 version 要求，`cargo package` 会无条件失败——M4 的 crate 发布此前是硬阻塞的。已补齐，并为 8 个不在发布面上的 crate 标注 `publish = false`。
    - `main.rs` 从 11250 行降至 6646 行，拆出 `dry_run`、`protocol_translation`、`persistence` 三个模块，测试移入 `tests.rs`。
29. 修复的真实缺陷：
    - `credential.rs` 跨进程 token 刷新缺少锁内二次检查，导致并发实例重复铸 token（对"重新签发即吊销旧 token"的 provider 会互相踢下线）。
    - `catalog/manifest.json` 与 `catalog.json` 哈希漂移，使主干 CI 与 `--dry-run` 均为失败状态。
    - 三处 `reqwest` 客户端未设 `no_proxy`，在设置了 `http_proxy` 的机器上导致测试失败或无限挂起。
    - 两个依赖固定 `sleep` 的 flaky 测试改为带截止时间的轮询（压测 60/60 通过）。

P2/P3 的逐项证据与外部验收边界见 `docs/p2-p3-execution-status.md`。`code_complete` 不等于生产 `accepted`：至少一个非 SiliconFlow 云端真实发现、付费 probe、Redis HA/ACL/TLS、多 Gateway 分发故障和 Agent Host 真实天气工具仍需对应权限与环境。

## 下一关键路径

```text
P0:    baseline/security -> schema migration -> idempotency -> API contract
P1:    FeatureFrame/Trace -> DecisionRecord v2 -> pure policy boundary
P1.5:  Provider/Deployment metadata -> revision binding
P2:    quota/budget settlement -> typed reliability -> metrics/readiness
P3:    Catalog publish/hot reload -> semantic/tool-requirement routing
P4:    dataset export -> benchmark/counterfactual evaluation -> support-domain gate
P5:    shadow learned router -> task canary -> rollback drill
P6:    protocol adapters -> gateway/embed parity -> release
```

优化口径下的 Gateway M0 仍保持闭环，但不再用它代表原始设计完成度。后续唯一任务状态以
`docs/requirements-traceability.md` 为准，执行顺序和退出门禁以 `uRouter_后续技术执行方案.md` v1.1 为准。
Redis HA/ACL/TLS、商用凭据轮换和 Agent Host 工具执行属于需要外部环境证据的验收项。
