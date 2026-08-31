# uRouter 共享 DecisionRecord / Feedback 状态迭代验证报告

> 日期：2026-08-26  
> 任务：GW-550  
> Redis：`127.0.0.1:16380`，Redis 6.0，AOF enabled

## 结论

GW-550 已完成。配置 `--redis-url` 后，TaskBinding、circuit、DecisionRecord 和
feedback 使用同一 Redis prefix 下的共享权威状态。两个网关可立即查询对方写入的记录，
feedback 按 signal kind 原子合并，decision/task/tenant 删除对其他实例立即可见，并在
Redis AOF 重启后保持删除结果。

## 数据模型与故障语义

| 状态 | Redis 结构 | 语义 |
|---|---|---|
| DecisionRecord | tenant sorted set + 独立 JSON value | 有序分页、容量裁剪、按记录 TTL |
| Feedback | tenant set + tenant/turn hash | signal kind 原子覆盖、按 data policy TTL |
| 资源键 | tenant/decision/turn 组合 SHA-256 | Redis key 不包含原始 turn 或 decision ID |
| 后端故障 | HTTP 503 `state_backend_unavailable` | fail-closed，不泄露 Redis 连接错误 |

查询 DecisionRecord 时动态读取共享 feedback 并填充 `outcome_signals`。因此两个实例并发
写入不同 signal kind 时，不会由较晚写回的旧 DecisionRecord 快照覆盖另一种 signal。

Redis 模式拒绝同时配置 `--records` 或 `--feedback-records`。否则未执行删除的实例仍可能
在本地 JSONL 留下旧副本，破坏 tenant 删除和数据不复活承诺。旧 JSONL 必须显式迁移，
网关不会隐式合并两个权威来源。

## 自动化验证

- 常规 Rust 测试：74 个通过，1 个 rustdoc 通过。
- 显式 Redis 测试：4 个通过，其中新增测试使用两个独立 AppState/连接。
- 新测试覆盖跨实例 record 写读、并发 feedback 三种 signal 合并、DecisionRecord 动态
  hydration、跨实例删除、1 秒 record/feedback TTL 过期。
- fmt、Clippy `-D warnings`、catalog check 和 cargo doc 通过。

## 真实双实例验证

Gateway A 为 8788，Gateway B 为 8789，Qwen3.8-27B 为
`127.0.0.1:19121/starvlm`。

| 场景 | 观测 |
|---|---|
| A hard primary | HTTP 200，capable/27B，输出 `GW550_SHARED` |
| B 查询 A 的 decision | list 与按 ID 查询均 HTTP 200 |
| tenant B 查询 tenant A decision | HTTP 404 |
| B 写 `accepted` feedback | A feedback 查询 200，A decision 出现 outcome signal |
| B 删除最后一个 decision | A decision 与无引用 feedback 均 HTTP 404 |
| B 删除 tenant A task | `deleted=2, feedback_deleted=1`；A retained=0 |
| tenant 隔离 | tenant B retained=1 且 feedback=200，不受 tenant A task 删除影响 |
| B 删除 tenant B | `deleted=1, feedback_deleted=1`；A retained=0、feedback=404 |
| Redis 与网关重启 | 两个 tenant retained=0，已删除 feedback 仍为 404 |
| Redis 停止 | 共享查询 HTTP 503，耗时约 1.3 ms，不泄露后端错误 |
| Redis 恢复 | 首次探测处于重连窗口返回 503；紧接着重试 HTTP 200，约 2.3 ms |

三个 Redis repository 统一配置 2 秒响应超时、1 秒连接超时、最多 3 次重连重试和
500 ms 最大重试延迟。该上限防止 Redis 黑洞或网络半开时管理查询无限挂起；自动重连是
最终恢复语义，不保证 Redis 恢复后的第一个请求零失败。

## 剩余边界

- 当前 list hydration 对每个带 turn 的 record 查询一次 feedback，正确性优先；高容量生产
  流量应增加 pipeline/batch 读取后再做性能门禁。
- 顺序删除已一致；与仍在执行的同 tenant/task 请求发生竞争时，尚未引入 deletion generation
  或 tombstone。需要在强 GDPR 并发语义上线前补齐。
- 生产 Redis 仍需 TLS、ACL、HA 和备份恢复演练。
- GW-600 SessionBinding 与 GW-610 AionUI/WorkBuddy adapter 已完成。
