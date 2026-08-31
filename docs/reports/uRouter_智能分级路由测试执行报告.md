# uRouter 智能分级路由测试执行报告

## 1. 执行结论

测试日期：2026-08-29

测试环境：

- Gateway：`http://127.0.0.1:8787`
- Auto Model：`urouter/auto`
- Efficient：`siliconflow/qwen2.5-7b-instruct`
- Capable：`siliconflow/deepseek-r1-pro`
- 状态后端：单实例内存模式

总体结论：

- uRouter 的能力准入、Hint、Preference、辅助调用、Fallback、任务绑定和治理规则真实生效。
- 简单问候稳定使用 Qwen2.5-7B，数学、规划和标准 Tool Call 稳定使用 DeepSeek-R1。
- 关键 OpenAI SDK 用例连续执行 3 轮，15/15 路由断言通过。
- 纯文本语义不会自动触发模型升级。“武汉天气”和“解方程”在没有 Tools/Hint 时仍选择小模型，尚未满足全自动语义路由目标。

## 2. 测试配置变更

新增或更新：

- `catalog/catalog.json`
  - 新增 SiliconFlow Provider。
  - 新增 Qwen2.5-7B Efficient 模型。
  - 新增 DeepSeek-R1 Pro Capable 模型。
- `catalog/manifest.json`
  - 刷新 Catalog 内容、价格、能力和 Compat Hash。
- `gateway/route.siliconflow.json`
  - 配置 `efficient -> capable` 双 Tier Route。
- `gateway/route.siliconflow-fallback-test.json`
  - R-013 专用故障注入 Route。
- `tools/routing-e2e.cjs`
  - OpenAI SDK 路由、流式和 Tool Call 闭环测试脚本。
- `crates/urouter-ai/tests/catalog_mvp.rs`
  - 更新 Catalog 数量及能力过滤断言。

商业 API Key 仅通过 Gateway 进程环境变量注入，没有写入上述文件。

## 3. L0 配置与回归测试

### 3.1 Catalog

结果：通过。

```text
valid: 6 provider(s), 7 model(s)
content hash: sha256:c044fc98d46c05dbe9f3b401973e4ada06dc34b11fcf4a05b84e4849c92b90c8
```

### 3.2 Workspace

命令：

```powershell
cargo test --workspace --quiet -j 1
```

结果：

- 84 项通过。
- 4 项按设计忽略。
- 0 项失败。
- Rust doctest 通过。
- `cargo fmt --all -- --check` 通过。

## 4. 模型事实探测

SiliconFlow `/v1/models` 确认以下模型对当前账户可用：

- `Qwen/Qwen2.5-7B-Instruct`
- `Pro/deepseek-ai/DeepSeek-R1`

Qwen2.5-7B Tool Call 探测没有返回标准 OpenAI `tool_calls`，而是把伪函数调用放入普通文本，因此 Catalog 按 OpenAI Tool Call 语义记录为 `tool_calling=false`。

## 5. L1 Explain 路由矩阵

结果：当前实现预期 11/11 通过。

| ID | 场景 | Tier | Model | Reason | 结果 |
|---|---|---|---|---|---|
| R-001 | 简单问候 | efficient | Qwen2.5-7B | default_efficient | PASS |
| R-003 | 纯文本武汉天气 | efficient | Qwen2.5-7B | default_efficient | CURRENT PASS / TARGET FAIL |
| R-004 | Weather Tool | capable | DeepSeek-R1 | capability_required | PASS |
| R-005 | Tools + Hard + Plan | capable | DeepSeek-R1 | capability_required | PASS |
| R-006 | 纯文本二元方程 | efficient | Qwen2.5-7B | default_efficient | CURRENT PASS / TARGET FAIL |
| R-007 | 方程 + Hard | capable | DeepSeek-R1 | quality_guard | PASS |
| R-008 | 实施方案 + Plan | capable | DeepSeek-R1 | quality_guard | PASS |
| R-009 | Auxiliary + Hard | efficient | Qwen2.5-7B | cost_preference | PASS |
| R-010 | Pin Capable | capable | DeepSeek-R1 | preference_pin | PASS |
| R-011 | Floor Capable | capable | DeepSeek-R1 | default_efficient | PASS |
| R-012 | 显式 R1 模型 | pinned | DeepSeek-R1 | explicit_model | PASS |

R-003 和 R-006 证明当前实现不分析 Prompt 语义。它们符合当前代码规则，但不符合最终业务目标。

## 6. L2/L3 OpenAI SDK 端到端测试

执行命令：

```powershell
node .\tools\routing-e2e.cjs
```

连续执行 3 轮，所有路由断言通过。

| 用例 | 预期模型 | 3 轮结果 | 延迟范围 |
|---|---|---|---|
| R-001 简单问候 | Qwen2.5-7B | 3/3 PASS | 0.289–0.948 秒 |
| R-007 Hard 方程 | DeepSeek-R1 | 3/3 PASS | 28.274–34.968 秒 |
| R-009 Auxiliary | Qwen2.5-7B | 3/3 PASS | 0.973–1.785 秒 |
| R-008 流式 Plan | DeepSeek-R1 | 3/3 PASS | 9.100–20.490 秒 |
| R-004 Weather Tool 闭环 | DeepSeek-R1 | 3/3 PASS | 45.840–203.227 秒 |

Weather Tool 闭环每轮均完成：

1. Gateway 根据 Tools 能力选择 R1。
2. R1 返回 1 个标准 OpenAI Tool Call。
3. 测试客户端执行 Mock `get_weather`。
4. Tool Result 回传 R1。
5. R1 生成最终天气回答。

第三轮 Tool 闭环出现 1 次重试，总耗时约 203 秒。最终成功，但需要纳入上游尾延迟和重试监控。

## 7. L4 可靠性与治理

### 7.1 R-013 Fallback

故障注入：Efficient Deployment 指向不可用的 `127.0.0.1:1`。

结果：通过。

```text
attempts: efficient, efficient, capable
error kinds: timeout, timeout, success
final tier: capable
final model: siliconflow/deepseek-r1-pro
fallback depth: 1
reason: fallback_degraded
HTTP status: 200
```

### 7.2 R-014 Bad Request

向上游发送非法 `max_tokens=-1`。

结果：不重试、不 Fallback 的核心要求通过。

```text
attempts: 1
error kind: bad_request
fallback depth: 0
selected tier: efficient
downstream HTTP status: 502
```

注意：上游确定性 400 被 Gateway 映射成下游 502。虽然没有错误重试或降级，但状态码映射值得单独评审。

### 7.3 Task Binding

第一次测试没有租户 Header，Gateway 进入兼容模式，按设计不创建 Binding。

加入 `x-urouter-tenant-id: routing-test` 后：

```text
turn 1: capable / DeepSeek-R1 / quality_guard
turn 2: capable / DeepSeek-R1 / task_binding
binding generation: 1
```

结果：通过。Binding 删除后再次读取返回 404。

### 7.4 Recording None

请求成功并返回 Decision ID，但 `GET /v1/decisions/{id}` 返回 404。

结果：通过。

### 7.5 Secret Scan

仓库内 API Key 文件命中数：0。

结果：通过。

## 8. 成本与性能证据

代表性 DecisionRecord：

| Model | Input | Output | Gateway Cost | Upstream Latency |
|---|---:|---:|---:|---:|
| Qwen2.5-7B | 34 | 2 | `$0` | 326 ms |
| DeepSeek-R1 | 17 | 243 | `$0.000583387` | 9,804 ms |
| DeepSeek-R1 | 10 | 241 | `$0.000574539` | 11,348 ms |
| DeepSeek-R1 | 17 | 405 | `$0.000965626` | 16,052 ms |

Qwen 模型按 SiliconFlow 免费模型记录为 0。R1 成本使用 Catalog 中人民币价格按固定汇率换算的近似 USD，不等同于 SiliconFlow 原币种账单。

## 9. 发现的问题

### P1：缺少 Prompt 语义分类

纯文本“武汉天气”和“解方程”不会自动升级。当前必须由 Tools、Hard、Plan 或调用方策略显式触发。

建议：增加独立、可审计的 Intent/Complexity Classifier，并把分类结果转为现有 Hint，不要把语义分类混入 Catalog 模型事实。

### P1：Weather Tool 尾延迟过高

第三轮闭环达到 203 秒且发生重试。需要分别记录首 token、Tool Call、Tool Result 后生成和 Retry 延迟，并为 Agent 工作流设置总时限。

### P2：上游 400 映射为下游 502

当前分类正确，但客户端收到的状态码不能直接区分确定性请求错误与网关/上游故障。

### P2：无租户 Header 会禁用 Binding

这是兼容模式设计行为，但 SDK 接入文档和测试客户端必须明确注入可信租户 Header。

### P3：Reasoning Usage 精度

SiliconFlow 返回 `reasoning_content`，但当前 Usage 记录没有独立统计 reasoning tokens，可能影响推理成本分析。

## 10. 最终判定

| 验收项 | 判定 |
|---|---|
| 能力驱动分级 | PASS |
| Hint/Preference 分级 | PASS |
| 小模型日常问答 | PASS |
| 大模型数学与规划 | PASS |
| 标准 Weather Tool 闭环 | PASS，存在尾延迟风险 |
| Retry/Fallback | PASS |
| Bad Request 不重试 | PASS |
| Task Binding | PASS，需要租户 Header |
| Recording Policy | PASS |
| Secret 不落盘 | PASS |
| 纯文本语义自动分级 | FAIL，功能尚未实现 |

当前版本可以作为“显式信号和能力驱动”的分级路由投入后续验证，但不能宣称已经实现无需 Hint 的自然语言语义自动路由。
