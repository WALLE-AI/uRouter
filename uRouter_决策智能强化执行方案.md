# uRouter 决策智能强化执行方案

> 制定日期：2026-09-04
> 起因：[`uRouter_对标开源路由器技术差距分析.md`](uRouter_对标开源路由器技术差距分析.md) 的结论——治理骨架领先，**决策依据是五种范式里最弱的一种**
> 范围：只强化"凭什么选这一档"，不动治理骨架、不动供给侧、不动执行链路
> 全部代码位置已核对

---

## 零、判定标准先立在前面

**这个方案的成功标准不是"实现了轨迹路由"，是"轨迹路由被证明改变了路由结果且没有变差"。**

只写代码不算完成。每个阶段的验收都必须是可观测的事实，而不是"功能已上线"。

同时必须先承认一个约束：

> **在模型深度从 9 补到每档 3+ 之前（对标分析 G3），任何路由算法的改进都测不出统计显著的效果。**

这不是推迟本方案的理由，而是决定它**形态**的理由：

```
S1–S3 以 shadow 形态上线 —— 提取、记录、进 trace，但不参与决策
                          ↓
                     G3 模型深度就位
                          ↓
              用积累的 shadow 数据跑 L2/L3，再决定开关
```

**代码先建成，数据先积累，开关留到有证据时再开。** 这样两条线可以并行，且不会出现"改了但不知道好不好"的状态。

---

## 一、现状定位

用对标分析的五范式，逐条标出当前层次与目标层次：

| 范式 | 现状 | 本方案目标 | 阶段 |
|---|---|---|---|
| **A 语义匹配** | 关键词匹配最后一条消息 | 不变（升级由意图模块负责） | — |
| **B 轨迹打分** | **消费侧已建、生产侧为零、符号有误** | **完整可用** | S0–S3 |
| **C 级联+自验证** | 无 | 不做（见 §6） | — |
| **D 离线监督** | 工件框架已建、无训练数据 | 接上轨迹特征 | S4 |
| **E 在线赌博机** | 有探索与 propensity、无质量臂 | 不动 | — |

**本方案 90% 的工作量集中在范式 B。** 理由很简单：它是唯一一个"消费侧已经建好、只差生产侧"的，性价比高出其余四个一个数量级。

---

## 二、四条不可让步的约束

任何实现细节都可以讨论，这四条不行。

### 约束 1：提取必须落在 `urouter-core::decide()` 内

**不能放在网关。** `crates/urouter-embed/src/lib.rs:74` 与网关走的是同一个入口：

```rust
let baseline = self.route.decide(&self.catalog, request)?;
```

提取一旦放在网关，`urouter-embed` 就拿不到，**gateway↔embed parity 当场破裂**。放在 `decide()` 里（`core:679` 的 `parse_contract` 之后），两侧自动一致——parity 由构造保证，不靠测试保证。

### 约束 2：提取必须是纯函数

输入 `&Value` + 配置，输出有界数值。无时钟、无随机、无 I/O。

否则决策不可重放，而可重放是 uRouter 唯一没有对手的性质（`tools/urouter-xtask/src/pure_crates.rs:11` 的 CI 门禁守着它）。

### 约束 3：调用方声明优先于网关提取

`SignalContract`（`core:333`）是公开契约，客户端已经可以声明信号。提取值**不得静默覆盖**声明值。

合并规则：**某个 kind 只要调用方声明了，就用声明值；没声明才用提取值。** 并且 trace 里必须记录每个维度的来源。

理由：调用方比网关更了解自己的轨迹（比如它知道某次工具报错是预期内的探测）。静默覆盖会让客户端失去表达能力，而且排障时无法区分"提取错了"和"客户端说错了"。

### 约束 4：全程带 kill switch，默认关闭

新增 `RouteConfig.trajectory`，**必须带 `skip_serializing_if`**——否则会作废所有已部署路由的 revision（`the_shipped_route_revision_is_pinned` 测试会拦住）。

默认 `mode: "off"`，可选 `"shadow"` / `"on"`。

---

## 三、阶段划分

| 阶段 | 内容 | 改动量 | 依赖 | 形态 |
|---|---|---|---|---|
| **S0** | 修 `production_intensity` 符号 | **1 行 + 1 测试** | — | 直接生效 |
| **S1** | 轨迹信号提取（纯函数） | 一个模块 + 模式表 | S0 | **shadow** |
| **S2** | 有符号加权 + 置信度弃权 | `select_tier` 改形 | S1 | shadow |
| **S3** | 结果证据降档 | 小 | S2 | shadow |
| **S4** | 特征进工件，权重可训练 | 中 | S1 + G3 | 影子工件 |
| **S5** | 开关打开（分租户灰度） | 配置 | S2–S4 + G3 + L2 证据 | **正式** |

S0 独立可发；S1–S3 建议合并成一个 PR 序列；S4 依赖模型深度；S5 是决策点不是工程量。

---

## S0 — 修符号（先做，一行）

### 问题

`crates/urouter-core/src/lib.rs:1303`：

```rust
let signal_quality = signal_score(
    contract,
    &["severity", "spinning", "exploring", "production_intensity"],
) > 500;
```

`production_intensity`（智能体正在稳定产出代码）被当成**升档**信号。Switchyard 原式里它带减号，是**降档**信号（`opensource/Switchyard/crates/libsy/src/algorithms/util/stage.rs:363`）。

今天影响为零（没人产生信号），但 S1 一落地就立刻生效，方向与成本优化的第一性原理相反。

### 改动

```rust
let signal_quality = signal_score(
    contract,
    &["severity", "spinning", "exploring"],          // ← 移除
) > 500;
let signal_low_cost = signal_score(
    contract,
    &["cost_sensitive", "disposable", "production_intensity"],   // ← 移入
) > 500;
```

### 验收

一个断言"高 `production_intensity` 不触发 `high_quality`、且触发 `low_cost`"的单测。这个测试的作用是**锁住符号**，防止将来重构再丢一次。

---

## S1 — 轨迹信号提取（核心）

### 1.1 放在哪

新建 `crates/urouter-core/src/trajectory.rs`，在 `decide()` 里 `parse_contract`（`core:679`）之后调用。

**为什么不放 `urouter-contracts`**：提取要读 OpenAI-chat 形状的 `messages`，那是请求格式知识，属于 `core` 的职责范围；`contracts` 应当只持有与格式无关的决策原语。

**为什么不放 `urouter-protocol`**：`decide()` 拿到的是原始 `&Value`，不是 IR；引 protocol 依赖只为了转一次 IR 不值得，且会让 `core` 的依赖面变大。

### 1.2 输入形状：已经被归一化了

**这是一个白拿的好处。** 五个入站协议全部先转成 OpenAI-chat 再重入 `chat_completions`：

| 入口 | 转换 | 汇入点 |
|---|---|---|
| `/v1/messages` | `from_anthropic_messages` (`main.rs:4028`) | `main.rs:4038` |
| `/v1/responses` | — | `main.rs:4012` |
| Ollama `/api/chat` | `from_ollama_chat` (`main.rs:4103`) | `main.rs:4119` |
| Gemini | — | 同上模式 |

所以 `decide()` **永远**看到 OpenAI-chat 形状。提取函数只需要认一种格式。

### 1.3 ⚠️ 但归一化不是无损的：Anthropic 批量 tool_result 会坍缩

`crates/urouter-protocol/src/lib.rs:743`：

```rust
let tool_call_id = blocks
    .into_iter().flatten()
    .find(|part| part.get("type") == Some("tool_result"))   // ← 只取第一个
    .and_then(|part| part.get("tool_use_id"))
```

Anthropic 把 3 个 `tool_result` 塞进**一条** user 消息；转换后是**一条** `Message`，带 3 个 `ContentPart::Json`，但只保留**第一个** `tool_use_id`。

**后果**：同一条逻辑轨迹，从 `/v1/chat/completions` 进来是 3 条 `role:"tool"` 消息，从 `/v1/messages` 进来是 1 条。**按消息数计数会得到 3 vs 1。**

这正是 Switchyard 自己承认没解决的问题（`tool_signals.rs:236` 注释："Message-count proxy for turn depth. Wire-format dependent…"）。uRouter 可以做得更好，但必须显式处理。

**计数规则（必须写成测试）**：

```
工具结果载荷数 = Σ over messages where role == "tool" of max(1, content_parts)
```

OpenAI 路径：3 条消息 × 1 part = 3。Anthropic 路径：1 条消息 × 3 parts = 3。**一致。**

**必须有的测试**：构造一条逻辑相同的轨迹，分别从 `/v1/chat/completions` 与 `/v1/messages` 送入，断言提取出的四个维度**逐字段相等**。这个测试比提取逻辑本身更重要——它是"多协议入口不影响路由"的唯一保证。

### 1.4 接口

```rust
/// 从 OpenAI-chat 形状的请求中提取轨迹信号。纯函数：无时钟、无随机、无 I/O。
pub struct TrajectoryConfig {
    pub recent_window: u8,          // 最近窗口大小，默认 3
    pub stall_min_turn_depth: u32,  // 判定 spinning/exploring 的最小深度，默认 8
}

pub struct TrajectorySignals {
    pub severity_millis: u16,          // 0 / 300 / 700 / 1000
    pub spinning: bool,
    pub exploring: bool,
    pub production_intensity_millis: u16,
    pub tests_passed: bool,
    pub tool_result_count: u32,        // 见 §1.3 的计数规则
    pub compacted: bool,
    pub no_error_streak: u32,
}

pub fn extract_trajectory(request: &Value, config: &TrajectoryConfig) -> TrajectorySignals;
```

**全部用整数**（`_millis`），与 `urouter-artifact` 的整数推理一致，避免浮点在重放中的不确定性。

### 1.5 严重度模式表

直接采用 Switchyard 已标定的那组（`opensource/Switchyard/crates/libsy/src/algorithms/util/tool_signals.rs:32`），保留分级：

| 级别 | 值 | 模式 |
|---|---:|---|
| CRITICAL | 1000 | `out of memory` / `memoryerror` / `connection refused` / `econnrefused` |
| HARD | 700 | `traceback (most recent call last)` / `modulenotfounderror:` / `command not found` / `assertionerror` / `valueerror:` / `syntaxerror:` / `timed out` / `no such file or directory` / `file does not exist` |
| SOFT | 300 | `exit code 1` / `exit code 2` / `returned non-zero` / `exited with code` |

**三条必须原样保留的性质**（每条一个测试）：

1. **窗口化取最大**——错误在恢复轮里持续存在，不是下一轮就清零
2. **锚定匹配**——`file does not exist` 而非裸的 `does not exist`（后者在 `ls` 输出上误触发；Switchyard 在 1006 条轨迹上量到 22 真阳 / 2 假阳）
3. **多模式取最大**——`exit_nonzero`(SOFT) + `traceback`(HARD) → HARD

**必须加的一条 uRouter 特有的**：模式表要有中文条目吗？**暂不加**。理由：工具输出（编译器、解释器、shell）绝大多数是英文；加中文模式而没有标定数据，等于凭空引入假阳性。等 shadow 数据积累后再按实际分布决定。**这条要写进代码注释，避免以后被当成疏漏补上。**

### 1.6 `spinning` / `exploring` 的互斥

照抄 Switchyard（`stage.rs:295`），互斥是刻意的——避免在产出轴上重复计数：

```
deep_enough     = tool_result_count >= stall_min_turn_depth
no_production   = recent_write == 0 && recent_edit == 0
investigating   = recent_read >= 1 || recent_plan >= 1

spinning  = deep_enough && no_production && !investigating   // 卡住了
exploring = deep_enough && no_production &&  investigating   // 在调研
```

工具名分类表需要覆盖 uRouter 实际面对的智能体（AiOnUI 等），不能只抄 Switchyard 的 Claude Code 名单。**S1 阶段先抄，shadow 数据里统计 `Other` 类占比**——占比过高说明名单不匹配，届时再补。这个统计本身要作为 shadow 的一项输出。

### 1.7 合并进 contract（约束 3 的落地）

```rust
// 调用方声明优先；未声明的 kind 才用提取值
for (kind, value) in extracted.as_signal_pairs() {
    if !contract.signals.iter().any(|s| s.kind.as_deref() == Some(kind)) {
        contract.signals.push(SignalContract { turn: None, kind: Some(kind.into()), strength: Some(value) });
        trace.signal_sources.insert(kind, "extracted");
    } else {
        trace.signal_sources.insert(kind, "declared");
    }
}
```

### 1.8 shadow 形态

`mode: "shadow"` 时：**提取、记录进 `DecisionRecord`、进 `/v1/explain`，但不写入 `contract.signals`**——即不影响 `select_tier`。

`DecisionRecord` 无 `deny_unknown_fields` 且已用 `skip_serializing_if`，可以直接加字段，不需要升 schema 版本。

⚠️ **`FeatureFrame` 不要动**（`contracts/src/features.rs:12`）——它是 `deny_unknown_fields` + `schema_version`，加字段会使所有已发布 artifact 失效。轨迹特征进工件放到 S4。

### 1.9 验收

| # | 验收项 |
|---|---|
| 1 | 跨协议一致性测试（§1.3）通过 |
| 2 | 严重度三条性质各有测试 |
| 3 | `mode: "off"` 时 `decide()` 输出**逐字节**不变（用现有 parity 语料回归） |
| 4 | `mode: "shadow"` 时决策结果不变，但 `/v1/explain` 出现四个维度 |
| 5 | `xtask check-pure-crates` 仍通过 |
| 6 | 钉死的 route revision 测试通过（新字段带 `skip_serializing_if`） |

---

## S2 — 有符号加权 + 置信度弃权

### 问题

现在的 `select_tier`（`core:1310`）是一组 `||`：

```rust
let high_quality = hint.difficulty == "hard" || ... || signal_quality || long_context_quality;
```

**任何一条成立就翻转，没有权重、没有加总、没有置信度。** 三个弱信号叠加，与一个强信号，效果完全相同。

### 改动

引入 Switchyard 式的有符号打分，但**全整数**（保可重放）：

```rust
// 全部 millis，避免浮点
score_millis = w_sev * severity_millis / 1000
             + w_spin * spinning_millis
             + w_expl * exploring_millis
             - w_prod * production_intensity_millis      // ← 减号，S0 的结构化版本

if score_millis.abs() < confidence_threshold_millis {
    // 弃权：不表态，交给其余的 hint/preference 判据
} else if score_millis > 0 { high_quality = true } else { low_cost = true }
```

**`tanh` 换成直接的整数阈值比较**——`tanh` 只是为了把分数挤进 (-1,1) 便于设阈值，而整数域里直接比较绝对值等价且可重放。这是对 Switchyard 的一处改进，不是简化。

### 弃权路径必须存在

置信度不足时**不是选一个次优答案，是不表态**。这与 [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) 里"标签体系必须有 `unknown` 且是默认值"是同一条原则的两个应用。

Switchyard 的 fall-open 是交给 LLM 分类器；uRouter **不做这一步**（热路径 LLM 调用违反约束 2），弃权即回落到其余判据。

### 权重放哪

`RouteConfig.trajectory.weights`，默认值用 Switchyard 已标定的那组。**这与 [`uRouter_设计文档.md`](uRouter_设计文档.md):1050 的原设计一致**——那里写的是"权重从 `RouterArtifact.calibration.signal_weights` 读取，让它可训练"。

S2 先放 route（可配、可签名、可回滚），S4 再让 artifact 覆盖它。**两级：route 给默认，artifact 给学出来的。**

### 验收

- 表驱动测试：权重 × 维度组合 → 期望档位
- 弃权测试：低置信度时 `select_tier` 的输出与不带信号时**完全一致**
- 与 S1 的 shadow 数据对比：新旧两套逻辑在同一批请求上的分歧率，写进报告

---

## S3 — 结果证据降档

### 需要修正一条我此前写下的禁令

[`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) 里写的是：

> **轨迹信号只抬高下限，从不降低。** 结构信号能证明"这里出问题了"，不能证明"这里很简单"。

Switchyard 的 `should_deescalate`（`stage.rs:386`）表明这条划得过宽：

```rust
signal.tests_passed
    && (signal.recent_write_count + signal.recent_edit_count) >= 1
    && signal.severity <= 0.0
```

这不是"看起来简单所以降档"，是**"这一轮已经收尾了所以降档"**——用的是**结果证据**（测试真的通过、代码真的写出来），不是难度猜测。

**禁令改写为**：

> 轨迹信号中的**预测性信号**（spinning、exploring、文本特征）只抬高下限；**结果性信号**（测试通过、任务完成标记、无报错连续段）可以降低下限。

原禁令想拦的"用文本特征猜简单"仍然被拦住。

### 规则顺序（照抄，理由充分）

`stage.rs:408` 的注释明说：**升档检查在降档之前**，这样"测试通过但同时出了 critical 错误"的一轮仍然升档。

```
1. 硬升档   critical 错误 / 上下文被压缩
2. 硬降档   测试通过 + 有产出 + 无报错
3. 打分器   S2 的加权分数
4. 弃权     回落其余判据
```

### 同步更新

Auto 方案 §2.5 的那句禁令要改写。**这属于本方案的交付物**，不是后续工作。

### 验收

- 顺利轨迹**不发生**升档（这条比"困难轨迹发生升档"更重要——过敏的验证器会系统性推高成本）
- critical + tests_passed 同时成立时升档
- `compacted` 自锁存：一旦压缩过，后续轮次持续升档（Switchyard `tool_signals.rs:240` 指出压缩会抹掉累积信号）

---

## S4 — 特征进工件（依赖 G3）

### 内容

1. `FeatureFrame` 加轨迹维度 → **`schema_version` 必须升版**，所有已发布 artifact 作废，需重训
2. `RouterArtifact.calibration.signal_weights` 覆盖 route 默认权重
3. `ArtifactSupportDomain` 加 `requires_trajectory: bool`——没有轨迹的单轮请求不进支持域

### 为什么必须等 G3

工件训练需要足够的臂。9 个模型（其中若干是本地 fixture）分摊到 3–4 档，每档 2–3 个，**有效臂太少，ESS 撑不起任何结论**。

### 验收

- shadow 工件与规则决策的分歧率
- `urouter-lab verify` 通过
- 支持域门禁：无轨迹请求走规则路径，`fallback_reason` 正确

---

## S5 — 开关打开（决策点）

**这不是工程阶段，是判定。**

前置全部满足才能进入：

- [ ] S0–S3 完成，shadow 数据积累 ≥ 2 周
- [ ] G3 完成（每档 3+ 模型）
- [ ] L2 配对实验显示：轨迹信号能改变的那部分请求上，**成本下降且质量不降**
- [ ] L3 反事实：IPS/SNIPS/DR **三个估计量同向**且 ESS 过门禁

**任何一条不满足就不开。** 分歧时的正确动作是继续采样，不是取平均。

打开方式：按租户灰度，用现有的稳定哈希分桶（不新建机制）。

---

## 四、前置依赖：G3 模型深度

**这是唯一的硬阻塞，且不在本方案范围内。**

当前 **44 provider / 9 model**——宽度铺开了，深度是空的。44 个 provider 大多没有对应模型条目，因为缺 `max_output_tokens` / `cost` / `compat` 等无法从 freellmapi 导出中获得的字段。

**补模型深度是数据工作不是代码工作**，可以与 S0–S3 完全并行。但 S4/S5 必须等它。

建议：先补**一档三模型**的最小可测集合（efficient / mid / capable 各 3 个真实可用、价格已知的模型），而不是把 44 个 provider 全铺开。L2 需要的是深度不是宽度。

---

## 五、改动清单汇总

| 文件 | 改动 | 阶段 |
|---|---|---|
| `crates/urouter-core/src/lib.rs:1303` | `production_intensity` 移入 `signal_low_cost` | S0 |
| `crates/urouter-core/src/trajectory.rs` | **新建**：提取 + 模式表 | S1 |
| `crates/urouter-core/src/lib.rs:679` 后 | 调用提取、合并进 contract | S1 |
| `crates/urouter-core/src/lib.rs` `RouteConfig` | 加 `trajectory`（**必须 `skip_serializing_if`**） | S1 |
| `crates/urouter-gateway/src/main.rs` DecisionRecord | 加轨迹字段与 `signal_sources` | S1 |
| `crates/urouter-gateway/src/main.rs` `/v1/explain` | 输出四维度与来源 | S1 |
| `crates/urouter-core/src/lib.rs:1310` `select_tier` | `||` → 有符号整数加权 + 弃权 | S2 |
| 同上 | 硬升档 / 硬降档 / 打分器 / 弃权 四级 | S3 |
| `crates/urouter-contracts/src/features.rs:12` | `FeatureFrame` 加维度，**升 schema_version** | S4 |
| `crates/urouter-artifact/src/lib.rs` | `signal_weights` 覆盖 route 默认 | S4 |
| `crates/urouter-gateway/src/dry_run.rs` | 加 `trajectory_config_consistency`，**18 → 19**（两处 `len()` 断言同步改） | S1 |
| `uRouter_Auto模式成本最优化技术方案.md` §2.5 | 改写降档禁令 | S3 |

---

## 六、明确不做

| 不做 | 理由 |
|---|---|
| **热路径 LLM 分类器**（Switchyard 的 fall-open 第 4 级） | 每请求一次外部调用，摧毁可重放性。弃权即回落其余判据 |
| **热路径嵌入调用**（LiteLLM AutoRouter） | 同上 |
| **AutoMix 式每请求自验证** | 延迟与调用数都翻倍。只在 L2 报告里增列"最高档是否达标"来识别 `HOPELESS` 类别 |
| **`tanh` 与浮点打分** | 整数阈值比较等价且可重放 |
| **中文错误模式** | 没有标定数据，凭空引入假阳性。等 shadow 数据再决定（§1.5） |
| **静默覆盖调用方声明的信号** | 客户端会失去表达能力，排障时无法区分"提取错"与"声明错" |
| **在 G3 之前打开开关** | 测不出效果的改动等于没有改动 |
| **动 `FeatureFrame`（S4 之前）** | `deny_unknown_fields` + `schema_version`，会作废所有已发布 artifact |

---

## 七、里程碑

| 里程碑 | 判定 | 阻塞 |
|---|---|---|
| **M0** | S0 合入，符号锁在测试里 | 无 |
| **M1** | S1 合入，`mode: "off"` 下决策逐字节不变 | 无 |
| **M2** | shadow 开启，`/v1/explain` 可见四维度，`Other` 工具占比有统计 | M1 |
| **M3** | S2+S3 合入，新旧逻辑分歧率有报告 | M2 |
| **M4** | G3 完成（每档 3+ 模型） | **本方案外** |
| **M5** | L2/L3 证据齐备 | M3 + M4 |
| **M6** | 灰度打开 | M5 |

**M0–M3 全部不依赖 G3，可立即启动。** M4 是外部依赖，建议并行推进。

---

## 八、一句话总结

> **强化决策智能这件事，90% 的价值集中在一个已经建好一半的模块上：轨迹信号的消费侧已在 `core:1303`，缺的是生产侧的一个纯函数和一处符号。先把它建成 shadow 形态积累数据，等模型深度就位再用证据决定开不开——而不是建成就开。**

立刻可以开始的两件事：

1. **S0**：一行 + 一个锁符号的测试
2. **G3**：补一档三模型的最小可测集合（数据工作，与 S1–S3 并行）

---

## 相关文档

- [`uRouter_对标开源路由器技术差距分析.md`](uRouter_对标开源路由器技术差距分析.md) — 本方案的依据，G1–G8 差距清单
- [`开源LLM路由项目技术框架对比分析.md`](开源LLM路由项目技术框架对比分析.md) — 五范式分类
- [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) — §2.5 降档禁令待 S3 改写
- [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) — 范式 A 的升级路径，与本方案正交
- [`uRouter_智能路由评测方案.md`](uRouter_智能路由评测方案.md) — S5 判定所依赖的 L2/L3
- [`Switchyard_深度技术解读报告.md`](Switchyard_深度技术解读报告.md) — S1–S3 的来源
