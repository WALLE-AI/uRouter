# uRouter Task-aware Auto 迭代验证报告

> 验证日期：2026-08-26  
> 网关：`http://127.0.0.1:8787`  
> efficient：`Qwen3.5-4B` / `127.0.0.1:8087`  
> capable：`Qwen3.8-27B` / `127.0.0.1:8094`

## 结论

Task-aware M0.1 已跑通首个 AionUI/WorkBuddy 目标切片：主 Agent 在任务首轮选择精确模型后保持稳定，
标题、摘要等 auxiliary 调用可旁路到低成本模型，且不会污染主任务绑定。

2026-08-26 的治理续迭代进一步补齐 tenant、数据策略、TTL、持久删除与基础 explain，
并再次通过 8094 上的 Qwen3.8-27B 真实请求。

## 真实顺序验证

| 步骤 | 请求标记 | 预期 | 实际 |
|---|---|---|---|
| 1 | primary + hard/plan | 选 capable | 8094 `Qwen3.8-27B`，`reason=quality_guard` |
| 2 | 同 task primary + normal/execute | 保持绑定 | 8094 `Qwen3.8-27B`，`reason=task_binding` |
| 3 | 同 task auxiliary + hard/plan | 旁路 efficient | 8087 `Qwen3.5-4B`，`reason=cost_preference` |
| 4 | 查询绑定 | 仍为 capable | `local-vllm-qwen38/qwen3.8-27b`，generation `1` |

三次请求均为 HTTP 200，`compatibility_mode=false`，无 retry、fallback 或记录写入错误。

## 隐私与运行边界

- 绑定与 DecisionRecord 不保存原始 `task.id`，只保存 SHA-256 键。
- 绑定在成功解析非流响应或完整流结束后才提交。
- 绑定表有容量上限，但当前不持久化、不跨实例共享。
- 并发首调用采用 first-success-wins，陈旧的在途决策不能覆盖已成功建立的绑定。
- `x-urouter-tenant-id` 由可信上游注入，task、feedback、decision 均按 tenant hash 隔离。
- `--require-tenant-header` 可拒绝缺少 tenant 的请求；关闭时缺失 header 进入 local compatibility mode。
- `data_policy.recording=none` 不保存 DecisionRecord，也不保存捎带 feedback。
- `metadata_only` 只保存哈希、路由、执行与治理元数据，不保存消息正文。
- TTL 在启动、读取和 60 秒后台 sweep 中执行；decision/task/tenant 删除会原子重写当前 JSONL，移除轮转副本和关联 feedback。
- 可信 header 边界仍须由 loopback、sidecar 或会剥离外部同名 header 的认证代理保证。

## 治理真实验证

| 场景 | 实际结果 |
|---|---|
| `POST /v1/explain` hard/plan | 不调用上游，解释为 capable / Qwen3.8-27B / `quality_guard` |
| WorkBuddy primary hard/plan | 8094 返回 `Qwen3.8-27B` 和 `ROUTER_27B_OK` |
| `metadata_only + allow_training=true` | 记录 `training_eligible=true`、`remote_judge_eligible=false`、TTL=1 天 |
| 其他 tenant 读取该 decision | HTTP 404 |
| `recording=none` 请求及捎带 feedback | 请求成功；decision 列表为空，feedback 查询 HTTP 404 |
| 删除真实 decision | HTTP 204；随后查询 HTTP 404，JSONL 与轮转路径无测试 ID |

## 质量门禁

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- 66 个 Rust 测试通过
- 1 个 rustdoc 测试通过
- catalog 校验通过
