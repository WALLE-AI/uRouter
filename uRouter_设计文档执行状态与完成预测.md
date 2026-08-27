# uRouter 设计文档执行状态与完成预测

> 更新日期：2026-08-26  
> 口径：以 `uRouter_技术架构深度评审与优化方案.md` 的收敛里程碑为执行基线，
> 不把原设计中的全部设想误算为首个可发布版本。

## 当前结论

- `urouter-ai` 事实与计价内核 MVP：已完成。
- 单实例 Agent-aware Auto Gateway 工程核心：100%，SessionBinding、产品适配入口与可执行 SLO 门禁已完成。
- 优化后 M0 发布候选：uRouter 自身边界已完成；AionCore 动态头注入按当前决策不作为完成条件。
- 原设计 M0-M4 全部任务：约 30-35%。按 1 名熟悉 Rust 的工程师连续投入，预计还需
  14-18 周，即约 2026 年 12 月至 2027 年 1 月完成；外部接入、生产流量和凭证审批不计入净工期。

这不是承诺日期。若要求多实例 Redis、训练评估、learned routing、canary 和嵌入形态全部达到
生产验收，必须按阶段取得真实流量和发布权限，不能靠代码实现单独宣告完成。

## 里程碑状态

| 里程碑 | 状态 | 已完成 | 主要剩余 |
|---|---:|---|---|
| `urouter-ai` MVP | 100% | catalog、能力、compat、endpoint、精确计价、证据、CLI | 云端付费 smoke 为外部可选验证 |
| M0 单实例核心 | 100% | Auto、stream、cost、record、retry、capacity、task/session continuity、tenant、TTL、delete、explain、RBAC/audit、adapter、soak/SLO | 无 uRouter 内部阻断项 |
| M1 多实例可靠性 | 约 85% | Redis binding/circuit/record/feedback、批量读取、删除代际、全局 Half-Open、RBAC/audit、AOF 重启与故障恢复 | Redis HA/ACL/TLS 生产演练 |
| M2 评估与画像 | 约 5% | feedback/paired 数据入口 | benchmark、任务基线、置信区间、支持域、数据集导出 |
| M3 学习与发布 | 0% | 无 | learned router、shadow、task canary、rollback、kill switch |
| M4 嵌入形态 | 0% | 目录与部分决策逻辑可复用 | 稳定纯决策 API、双形态一致性、示例与发布 |

## 本轮完成

1. `data_policy`：`none/metadata_only`、训练/远程 judge 授权、1-365 天 TTL。
2. 可信 tenant header：task、feedback、decision、override 全链路隔离。
3. 启动、查询和后台 sweep 的 TTL 执行。
4. decision/task/tenant 持久删除，覆盖 JSONL、轮转副本、feedback 和 binding。
5. `POST /v1/explain` 无上游 dry-run。
6. 66 个 Rust 测试和 1 个 rustdoc 测试通过。
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

## 下一关键路径

```text
M0 RC: Session contract -> AionUI/WorkBuddy adapter -> soak/SLO
M1:    state port -> Redis CAS binding -> shared circuit -> two-instance chaos -> shared records/feedback -> HA drill
M2:    task dataset export -> benchmark/baseline -> support-domain gate
M3:    shadow learned router -> task-level canary -> rollback drill
M4:    pure decision facade -> gateway/embed parity -> release
```

uRouter 自身 M0 关键路径已闭环。外部宿主 wiring 暂不纳入完成口径；M1 剩余项是部署环境侧的
Redis HA/ACL/TLS 演练。
