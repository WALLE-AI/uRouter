# uRouter 双实例故障、重启恢复与一致性迭代验证报告

> 日期：2026-08-26  
> 任务：GW-540  
> Redis：`127.0.0.1:16380`，Redis 6.0，AOF enabled

## 结论

GW-540 已完成。TaskBinding 和三作用域 circuit 均可由新建 repository/网关实例恢复；
Redis AOF 重启后，运行中的 ConnectionManager 能最终自动重连，binding 与未过期 Open
circuit 保持一致。重连窗口内允许请求返回 503。Redis 不可用时网关 fail-closed，返回
脱敏的稳定 HTTP 503，而不是继续执行可能破坏 task continuity 的无状态路由。

## 代码与契约调整

1. Redis binding 集成测试新增全新 repository 连接，验证 generation 2 的迁移状态可恢复。
2. Redis circuit 集成测试改为两个独立 repository 连接，不再共享同一个 clone 连接管理器。
3. binding/circuit 后端运行故障统一映射为 HTTP 503：
   `state_backend_unavailable` / `the shared state backend is temporarily unavailable`。
4. 客户端响应不包含 Redis URL、认证信息、`broken pipe` 等底层错误。
5. binding、circuit、record/feedback 连接统一使用 2 秒响应超时、1 秒连接超时、最多
   3 次重连重试和 500 ms 最大重试延迟，避免后端网络半开导致请求无限等待。

## 真实 Binding 恢复

| 步骤 | 观测 |
|---|---|
| 网关 A / 8788 hard primary | 19121/starvlm Qwen3.8-27B，HTTP 200，输出 `GW540_BOUND` |
| 网关 B / 8789 normal explain | `reason=task_binding`，capable/27B，generation 1 |
| 重启网关 B | 仍返回同一个 27B binding |
| 停止 Redis | 最新网关返回 HTTP 503 `state_backend_unavailable` |
| 从 AOF 重启 Redis | 原网关最终自动重连，短暂 503 后仍返回 27B binding |

## 真实 Circuit 恢复

使用 `gateway/route.redis-circuit-test.json`，首 deployment 指向未监听的 18087，
备 deployment 指向真实 8087，cooldown 为 120 秒。

| 步骤 | 观测 |
|---|---|
| 网关 C / 8790 首次请求 | HTTP 200；attempt 1 transport failure，attempt 2 `efficient-live` |
| C 的 tier view | `efficient-unavailable` deployment scope 为 Open |
| 停止 C 和 Redis | AOF 正常 fsync 后退出 |
| 从 AOF 重启 Redis并启动全新 D / 8791 | tier view 仍为 Open，剩余 cooldown 约 59 秒 |
| D 实际请求 | HTTP 200；只有 1 个 attempt，直接使用 `efficient-live` |

这证明 Open 状态不是网关本地残留，而是 Redis 持久状态；网关与 Redis 同时重启后仍能
避免重复撞击已知故障 deployment。

## 自动化门禁

- 常规 Rust 测试：73 个通过，1 个 rustdoc 通过。
- 显式 Redis 集成测试：3 个通过。
- `cargo fmt --check`、Clippy `-D warnings`、catalog check、cargo doc 均纳入最终门禁。

## 剩余边界

- 临时 Redis 是单节点验证，不代表生产 HA；生产仍需 TLS、ACL、主从/集群和备份恢复演练。
- GW-550 已补齐 DecisionRecord/feedback 的 Redis 权威状态、跨实例查询与删除一致性。
- GW-600 SessionBinding 与 GW-610 adapter 已完成；下一步为宿主接线与 soak/SLO。
