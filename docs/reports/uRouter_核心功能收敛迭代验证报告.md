# uRouter 核心功能收敛迭代验证报告

> 日期：2026-08-26  
> 范围：只验收 uRouter 自身核心，不把 AionCore/WorkBuddy 宿主改造计入完成条件。

## 结论

uRouter M0 内部核心已闭环。本轮完成共享状态批量读取、并发删除代际屏障和可执行
soak/SLO 发布门禁，并通过本地 Qwen3.8-27B 真实代理验证。

## 实现

1. `RedisSharedState::feedback_many` 使用单次 Redis pipeline 批量读取 turn feedback，
   DecisionRecord 列表不再按 record 逐条往返 Redis。
2. TaskBinding repository 增加 tenant/task generation。请求入场读取 generation；删除先原子
   递增 generation，再清理删除点前的数据；旧在途 binding 与 DecisionRecord 写入返回稳定
   503，不会在删除完成后复活。删除点之后入场的请求使用新 generation，可正常保留。
3. 新增 `urouter-soak`：默认压测 `/v1/explain`，可配置请求量、并发度、错误率、P95 和 RSS
   增长阈值，任一阈值不满足即返回失败退出码。
4. `/metrics` 增加 `process_resident_memory_bytes`，供内存增长门禁直接采样。

## 真实结果

| 检查 | 结果 |
|---|---|
| workspace tests | 82 passed，4 个 Redis 显式用例另行通过，1 rustdoc passed |
| Clippy | workspace/all-targets，`-D warnings` 通过 |
| 核心 soak | 5000/5000 成功，concurrency=32，0 错误 |
| 延迟与吞吐 | P50=2 ms，P95=3 ms，P99=4 ms，约 6896 req/s |
| RSS | 18 MiB -> 20 MiB，增长 2 MiB，阈值 64 MiB |
| 27B 真实调用 | `19121/starvlm`，Qwen3.8-27B，`capability_required`，返回 `get_status` tool call |

以上性能数字是当前机器上的短时回归基线，不等价于 24 小时生产容量结论。工具已经具备
长时运行所需的阈值与退出码，可直接接入 CI 或发布流水线。

## 剩余边界

- 单条 decision 删除与单条 session binding 删除仍是对象级操作；本轮强删除代际覆盖的是
  GDPR/运维常用的 task 与 tenant 批量删除边界。
- standalone feedback 与 tenant 删除在完全相同 turn 上的极窄并发窗口仍采用幂等覆盖语义；
  binding 和 DecisionRecord 不会复活。若法规口径要求 feedback 也严格线性化，应给 feedback
  写入同样携带 tenant generation。
- Redis HA、ACL、TLS、备份恢复属于部署环境验证，不属于本轮 uRouter 代码完成条件。
