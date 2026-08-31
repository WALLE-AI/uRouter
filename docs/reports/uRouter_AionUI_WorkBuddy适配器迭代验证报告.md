# uRouter AionUI / WorkBuddy 适配器迭代验证报告

> 日期：2026-08-26  
> 任务：GW-610  
> AionUI 参考源码：本机 AionUi + AionCore 运行实例  
> 模型：8087 Qwen3.5-4B；`19121/starvlm` Qwen3.8-27B

## 结论

GW-610 的 uRouter 侧已完成。新增 OpenAI-compatible adapter 路径：

- `/v1/adapters/aionui/chat/completions`
- `/v1/adapters/workbuddy/chat/completions`

adapter 将 AionUI 已有的 conversation ID、服务端 turn ID 和调用类型映射为 contract v2，
从 system/developer messages 与 tools 规范化计算执行画像哈希。产品不需要自行生成哈希，
标题/摘要/压缩调用确定性旁路主绑定。

## 能力修正

真实探测 8087：携带 tools 时，无论 `tool_choice=required` 还是默认 auto 都返回 HTTP 400，
错误明确指出服务未启用 tool parser/auto tool choice。因此目录继续诚实标记 4B
`tool_calling=false`。

路由改为同时公开：

- `capabilities`：所有默认候选保证的基线交集。
- `routable_capabilities`：至少一个可达候选支持的能力并集。

工具请求不再因基线交集为 false 被整体拒绝，而是 Capability Gate 排除 4B，只选择 27B，
并记录 `reason=capability_required`。这落实了深度评审 §3.3 的优化方案。

## 真实三步工作流

| 步骤 | 结果 |
|---|---|
| AionUI `plan` 主调用，携带 tool schema | HTTP 200，capable/27B，`capability_required`，返回真实 `get_status` tool call |
| 同 conversation 的第二轮 primary | HTTP 200，capable/27B，`session_binding` |
| 同 conversation 的 title 调用 | HTTP 200，efficient/4B，`cost_preference` |
| 查询主 SessionBinding | model 仍为 27B，generation=1，`last_seen_turn` 为第二轮主调用而非 title |
| compatibility | 三步均为 false |

## Adapter 契约

- 必需：`x-urouter-conversation-id`、`x-urouter-turn-id`。
- 可选：task、branch、call kind、migration boundary；task 默认 conversation，branch 默认 main。
- call kind 支持 primary/plan/verify/title/summary/compress。
- 请求必须选择 `urouter/auto`；已有 `urouter` body 会被拒绝，避免双重来源。
- canonical JSON 对对象字段顺序不敏感；测试覆盖等价 schema 产生相同哈希。
- 默认数据策略为 metadata-only、禁止训练/远程 judge、保留 7 天。

## 剩余接线边界

AionCore 源码确认 conversation 和 turn ID 已存在，但 provider 数据模型没有逐请求动态 header
模板。仅把 base URL 配为 adapter 路径无法安全注入不同会话 ID。生产接入仍需在 AionCore
请求构造处增加动态 header hook，或由可信 sidecar 注入并覆盖这些头；禁止使用固定
conversation header，否则会错误合并不同会话。

WorkBuddy 未在当前工作区提供源码或真实运行实例，因此完成的是相同中立 header 契约与
测试覆盖；产品内字段接线仍需其宿主仓库验证。

## 质量门禁

- 常规 Rust 测试 80 个通过，1 个 rustdoc 通过。
- 显式 Redis 集成测试 4 个通过。
- `cargo fmt --check`、Clippy `-D warnings`、catalog check 和 cargo doc 通过。
- 最终 8787 网关、8087 模型和 `19121/starvlm` 模型发现均 HTTP 200。
