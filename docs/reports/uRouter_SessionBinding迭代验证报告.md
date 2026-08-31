# uRouter SessionBinding 迭代验证报告

> 日期：2026-08-26  
> 任务：GW-600  
> 真实模型：`127.0.0.1:19121/starvlm`，Qwen3.8-27B

## 结论

GW-600 已完成。`urouter` contract v2 使用 tenant、`trace.conversation` 和
`trace.branch` 定位 SessionBinding，精确固定 model、provider、wire API、prompt profile
和 toolset。v1 task binding 保持兼容；辅助调用继续旁路主会话绑定。

## 契约与安全边界

- v2 主调用必须提供 task、harness、call role、turn、conversation、branch、data policy，
  以及格式严格的 prompt/toolset SHA-256。
- conversation 与 branch 必须成对出现；不同 branch 使用独立 binding。
- prompt/toolset 或目录中的 provider/API 身份变化，在无迁移边界时于上游调用前返回
  `unsafe_session_migration`。
- 安全迁移通过 Redis/内存 CAS 更新 binding 并递增 generation；并发首次成功仍为
  first-success-wins。
- 普通会话请求仅重试相同精确模型的 deployment；只有
  `terminal_provider_failure` 明确允许跨模型 fallback。
- task 删除覆盖其全部 session binding，tenant 删除覆盖全部 task/session binding。

## 真实验证

Gateway 使用 8788，Redis 使用 16380，全新 prefix `urouter-gw600-fixed`。

| 场景 | 观测 |
|---|---|
| 第一轮 hard 主调用 | HTTP 200，capable/27B，输出 `GW600_SESSION` |
| 同 conversation/branch 第二轮普通调用 | HTTP 200，仍为 capable/27B，`reason=session_binding`，输出 `GW600_PINNED` |
| compatibility | 两轮均 `x-urouter-compatibility-mode=false` |
| 管理查询 | HTTP 200；model/provider/API 与两个执行画像 hash 完整，generation=1 |
| 隐私 | task/conversation/branch 均只返回 `sha256:` scope key，不返回原始 ID |

真实验证期间发现并修复 Redis 物理 key 误用 task owner key 的问题。测试已加强为
session 写入后必须从第二条独立 Redis 连接按 session key 读取成功，避免仅验证删除而产生
假阳性。

## 自动化覆盖

- 常规 Rust 测试 76 个通过，1 个 rustdoc 通过。
- 显式 Redis 集成测试 4 个通过。
- `cargo fmt --check`、Clippy `-D warnings`、catalog check 和 cargo doc 通过。
- v2 完整身份与哈希格式校验。
- 同分支精确 pin、不同分支隔离。
- 非安全身份变化拒绝、安全迁移 generation 递增。
- task 批量删除 session binding。
- Redis 独立连接写读、CAS、迁移、task/tenant 删除。

## 剩余边界

- 当前 provider/API 身份来自不可变 catalog snapshot；动态目录热更新和 retired model
  handoff 尚未实现。
- 跨 provider 或跨 wire API 的 `HandoffPlan` 尚未实现，因此只能在显式安全边界重新选择，
  不承诺 reasoning/tool block 无损转换。
- GW-610 已完成 uRouter adapter 映射；AionCore 动态 header hook 与 WorkBuddy 宿主接线仍待各自仓库完成。
