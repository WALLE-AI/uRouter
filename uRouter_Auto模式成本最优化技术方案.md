# uRouter Auto 模式成本最优化技术方案

> 制定日期：2026-09-04
> 第一性原理：**在保证对话/Agent 任务质量的前提下，使用最优的模型，而不是一直使用最贵的模型。**
> 依据：对当前代码逐行核对 + [`uRouter_技术架构评审方案.md`](uRouter_技术架构评审方案.md)

---

## 零、把第一性原理写成可执行的形式

产品目标形式化后是一个**约束优化**问题：

```
minimize   预期成本
subject to 质量 ≥ 阈值
```

注意它既不是 `maximize 质量`，也不是 `minimize 成本`。这个区别决定了整套机制的形状：需要一个**可验证的质量下限**，以及在该下限之上**按成本排序**的能力。

当前实现两样都没有。

---

## 一、诊断：当前 Auto 模式为什么会"一直用最贵的"

### 1.1 tier 选择在结构上是二值的

```rust
// crates/urouter-contracts/src/tier.rs:182
impl TierRule for QualityRule {
    fn evaluate(...) {
        Ok((input.high_quality && !input.auxiliary).then(|| {
            (candidates.last().expect(...).index, "quality_guard")   // ← 无条件取最高 tier
        }))
    }
}

// tier.rs:206
impl TierRule for DefaultRule {
    fn evaluate(...) {
        Ok(Some((candidates.first().expect(...).index, ...)))        // ← 取最低 tier
    }
}
```

`candidates.last()` / `candidates.first()`。**只要 `high_quality` 为真就跳到最贵的那一档，否则用最便宜的。中间档位在结构上无法被选中。**

2 个 tier 时这看不出问题（最低=最高的补集）。一旦池子有真实梯度（本地 → 免费 → 低价 → 中端 → 前沿），中间所有档位都是死代码——而"最优模型"恰恰绝大多数时候在中间。

### 1.2 质量信号是调用方声明的，不是测量的

```rust
// crates/urouter-core/src/lib.rs:1310
let high_quality = contract.hint.difficulty.as_deref() == Some("hard")
    || contract.hint.workload.as_deref() == Some("plan")
    || preference_bias_millis(...) < -500
    || signal_quality
    || long_context_quality;
```

全部来自调用方的 `hint`。一个把 `difficulty: "hard"` 写死的 agent harness，会让**每一个请求**都命中 `quality_guard` 跳到最贵档。这正是"一直用最贵的"的直接机制。

而且这里存在激励错配：声明 `hard` 对调用方零成本、收益确定（更好的模型），所以理性的调用方会一直声明 `hard`。

### 1.3 成本从未进入 tier 选择

`tier.rs` 中 `cost` / `price` / `Money` 的提及次数为 3，且全部出现在文档注释里。**决策链上没有任何一处比较过候选之间的成本。** `calculate_actual_cost` 只在事后计费与预算预留时使用。

### 1.4 轨迹信号完全未被使用

`FeatureFrame` 只提取单次请求的静态特征：`message_count`、`input_text_bytes`、`available_tool_count`、`has_tool_result`、`contains_image`、`structured_output`、`reasoning`、`requested_max_output_tokens`。

`TaskBinding` 记录了 `tier` / `bound_at_turn` / `last_seen_turn`，但**只用于会话连续性（保持同一模型），不携带任何难度信息**。

于是 agent 轨迹里信息量最大的那类信号——工具循环、连续报错、用户纠正、上下文压缩——**一条都没有进入决策**。而这些恰恰是免费的、结构化的、不需要理解文本的。详见 Layer 2.2。

### 1.5 长上下文被当成了难度

`core:1308` 把"输入超过阈值"直接并入 `high_quality`，因此长上下文请求会跳到最贵档。但能力准入已经保证了窗口装得下——这是**为同一件事付两次钱**。详见 Layer 2.1。

### 1.6 成本比较未考虑 prompt 缓存

`Capabilities` 有 `PromptCacheSupport`、`CostRates` 有 `cache_read` / `cache_write`，`prompt_profile_hash` 的 cache-affinity 也已实现（`urouter_cache_affinity_hit_total`）。但这些**从未进入任何成本判断**。

在长上下文多轮场景里，缓存折扣通常超过档位价差——按标价切换到"更便宜"的模型，实际会更贵。详见 Layer 1.1。

### 1.7 副调用未被区分对待

`CallRole::Auxiliary` 已存在且绕过主任务绑定，但没有任何机制把副调用**主动**路由到更便宜的档位。而副调用（压缩历史、生成检索 query、抽取字段、起名、打分）在 agent 任务中占比很高，且换模型零风险。详见 Layer 3.4。

### 1.8 成本数字本身目前不可信

评审方案 P0-1：`urouter-core` 用 `characters/4`、`urouter-gateway` 用 `bytes/4`，同一段中文相差 3 倍（115 vs 345 token）。**在 3 倍误差的数字上做成本优化，比不做成本优化更糟** —— 它会系统性地误判哪个模型更便宜。

这使 P0-1 从"正确性缺陷"升级为**本方案的硬前置**。

### 1.9 已经具备但未接入闭环的部件

必须说明：uRouter **已经有**实现这套机制所需的几乎全部零件，它们只是没被连成回路，且大多默认关闭。

| 部件 | 位置 | 现状 |
|---|---|---|
| 精确定点计价 | `urouter-ai::pricing`，nano-USD | ✅ 可用 |
| 硬能力准入 | `urouter-ai::admission` | ✅ 可用，是天然的质量下限 |
| 反事实估计 IPS/SNIPS/DR + ESS | `urouter-eval:568` | ✅ 已实现，离线 |
| 确定性探索（propensity 可复算） | `apply_controlled_exploration` | ✅ 默认关闭 |
| 策略 artifact + canary/rollback | `urouter-artifact` | ✅ 默认关闭，决策语义是二值升级 |
| 反馈入口 | `POST /v1/feedback` | ✅ 有，未回流到路由 |
| 升级边界 | `MigrationBoundary::BeforeFirstAssistantToken` | ✅ 已定义，未用于成本升级 |
| 决策证据链 | DecisionRecord v2 | ✅ 可用 |
| 单次请求特征 | `FeatureFrame` | ✅ 有，缺轨迹维度 |
| 任务/会话状态 | `TaskBinding`（Redis/内存） | ✅ 有，仅用于连续性 |
| 迁移边界 | `MigrationBoundary` 六个变体 | ✅ 有，未用于成本升级 |
| 副调用角色 | `CallRole::Auxiliary` | ✅ 有，未主动降档 |
| 任务语义分类 | `SemanticTask`（4 值，含弃权） | ⚠️ 只做需求抽取，键不足以支撑 `by_task` |
| prompt 缓存事实与费率 | `PromptCacheSupport` / `cache_read` | ✅ 有，未进成本比较 |

**结论：这不是一个"要新建能力"的方案，而是一个"改决策语义 + 接闭环"的方案。**

---

## 二、方案总纲：从"预测要多贵"改为"从足够便宜处起步，按可验证信号升级"

预测式路由（先判断难度、再选档位）的根本问题是：**难度只有调用方知道，而调用方没有说实话的激励。**

改为：

```
floor = max(能力准入, 轨迹信号, 任务档案经验下限, 调用方声明)
起点 = 通过准入的候选中，floor 之上成本最低的那个
      ↓ 执行
     廉价结构化验证
      ↓ 通过 → 结束（绝大多数请求在此终止）
      ↓ 不通过且仍在可升级边界内 → 升级到下一档，重试
      ↓ 全程写入 DecisionRecord
      ↓ 离线反事实评估 → 更新经验下限 → 回到起点
```

三条设计约束（不可让步）：

1. **升级只发生在首个 assistant token 之前。** 已经发出的 token 不能撤回；`MigrationBoundary::BeforeFirstAssistantToken` 就是这条边界。
2. **会话绑定优先于成本。** 多轮对话中途换模型会导致行为跳变，那是 uRouter 的核心卖点。绑定生效时只允许在绑定宽限期内升级，且升级后重新绑定。
3. **不引入随机性。** 探索继续用哈希派生，保持 propensity 可复算——否则反事实评估失效，整个闭环塌掉。
4. **轨迹信号只抬高下限，从不降低。** 结构信号能证明"这里出问题了"，不能证明"这里很简单"；降低下限只交给有 ESS 门禁的离线评估。
5. **主线绑定优先于成本，副调用自由。** 一个任务里会用到不同模型，但收益来自"主线稳定 + 副调用各取所需"，而不是主线逐轮切换。

---

## 三、分层设计

### Layer 0（前置）统一 token 估算 —— 对应评审 P0-1

不做这一层，后面全部无效。

- 两处估算合并为 `urouter-contracts::tokens::estimate_input_tokens`（纯函数）
- 按脚本分段：ASCII 段 `chars/4`，CJK 段 `chars/1.5`，其余保守取 `chars/2`
- `ModelSpec` 增 `tokenizer: Option<TokenizerHint>`（`cl100k` / `o200k` / `sentencepiece` / `unknown`），为后续接真 tokenizer 留槽，但**不在本层引入依赖**

**验收**：中英文两类 prompt 的准入估算与配额估算之差 < 20%；`/v1/explain` 暴露 `estimated_input_tokens` 便于核对。

---

### Layer 1 成本成为一等决策输入

**新增纯函数**（`urouter-contracts::tier`）：

```rust
/// 一个候选在本次请求下的预估成本，nano-USD。
pub struct TierCostEstimate {
    pub index: usize,
    pub estimated_nano_usd: u64,
}

/// 在给定下限之上，选成本最低的候选。
///
/// 成本相等时取 index 较小者，保证同输入同结果——确定性优先于最优性，
/// 这是决策可重放的前提。
pub fn select_cheapest_above_floor(
    candidates: &[&TierCandidate],
    costs: &[TierCostEstimate],
    floor_index: usize,
) -> Option<(usize, String)>;
```

**改造 `QualityRule`**：从"取最高 tier"改为"取**不低于质量下限**的最便宜 tier"。

```
旧：high_quality → candidates.last()
新：floor = quality_floor(input)      // 见 Layer 2
    → select_cheapest_above_floor(candidates, costs, floor)
    → reason = "cheapest_above_floor"
```

`DefaultRule` 保持不变（floor = 0 时它就是最便宜的那个），因此这不是新增规则，而是**让现有规则不再退化成二值**。

**为什么放在纯函数核**：成本比较是决策，不是 I/O。运行时只负责把 `TierCostEstimate` 作为快照注入——与 `CapacitySnapshot` 完全同构。gateway↔embed parity 因此免费保持。

#### 1.1 必须用带缓存状态的**边际**成本，不是标价

这是多轮场景里最容易算错的一处。考虑一个 10 万 token 上下文的任务：

```
继续用已缓存的贵模型：  100k × cache_read 单价   （常见为 input 的 10%）
换用"更便宜"的模型：    100k × 该模型 input 全价 + 一次 cache_write
```

**在长上下文多轮场景里，缓存折扣往往超过档位之间的价差。** 按标价选"更便宜"的模型，实际会更贵——而且贵在一个不会出现在任何单价表里的地方。

目录里所需数据已经齐备：

```rust
// urouter-ai/src/capabilities.rs:35
PromptCacheSupport { enabled, min_cacheable_tokens, retention, explicit_control, session_affinity_header }
// urouter-ai/src/pricing.rs:10
CostRates { input, output, cache_read, cache_write }
```

因此 `TierCostEstimate` 必须携带缓存状态：

```rust
pub struct TierCostEstimate {
    pub index: usize,
    /// 本次请求在该候选上的边际成本。命中缓存的部分按 cache_read 计价，
    /// 换到未缓存的候选则计入全额 input 与一次 cache_write。
    pub estimated_nano_usd: u64,
    /// 该候选当前是否持有本会话的前缀缓存。
    pub cache_warm: bool,
}
```

`cache_warm` 由运行时注入（沿用既有的 `prompt_profile_hash` cache-affinity 状态，`urouter_cache_affinity_hit_total` 已在 metrics 中），纯函数只做比较。

**这条给了绑定稳定性一个成本上的理由**，而不只是行为一致性上的理由——两者刚好同向：留在原处既更稳、也更省。

**验收**：三个以上 tier 的 route 下，中间档位可被选中；`plan_capacity_lease` 的确定性测试扩展到成本相等的场景；一条"缓存命中的贵档边际成本低于未缓存的便宜档"的用例。

---

### Layer 2 质量下限的四个来源

#### 2.0 先纠正一个前提：难度不是输入文本的属性

同一句话可以很简单也可以很难，取决于三件与文本无关的事：

- **有没有工具** —— 有计算器时算术是机械操作，没有时才是推理
- **上下文里有没有答案** —— 检索命中时是抽取，不命中才是推理
- **答案要用来干什么** —— 草稿 vs 交付

因此"从用户输入判断难度"这个问法本身会把设计引向关键词、长度、疑问词一类启发式，而那些在真实流量上基本无效。现有语义分类器只保留 3 条窄规则、其余一律 `confidence 0` 显式弃权——**这个克制是对的，不应往里加更多猜测**。

可回答的是另外四个问题，按可靠度排序，**取四者的最大值**作为 floor：

| 来源 | 问的是 | 性质 | 成本 | 可否被调用方绕过 |
|---|---|---|---|---|
| ① 硬能力准入 | 这次调用**要求**什么 | 布尔排除（已有） | 免费 | 否 |
| ② 轨迹信号 | 轨迹**显示**了什么 | 结构判据（新） | 免费 | 否 |
| ③ 任务档案经验下限 | 历史上**这类**任务需要什么 | 离线评估得出（需意图标签作索引键） | 免费（查表） | 否 |
| ④ 调用方声明 | 调用方**说**它要什么 | `difficulty` / `pin_tier` | 免费 | 是（但语义改变） |

关键点：**这四者只决定起点与下限，不做最终判断**。最终判断交给 Layer 3 的廉价验证与一次升级——如果验证便宜且可升级，就不需要把难度预测准。

#### 2.1 修正：长上下文是能力需求，不是难度信号

```rust
// crates/urouter-core/src/lib.rs:1308
let long_context_quality = route.long_context_quality_threshold_tokens > 0
    && estimated_input_tokens(request) >= threshold;
let high_quality = ... || long_context_quality;      // ← 长上下文直接跳最贵档
```

但能力准入**已经**保证了窗口装得下（`urouter-ai/src/admission.rs:47` 的 `context_window_too_small`）。一个 20 万 token 的检索上下文加"根据以上文档回答"，是**抽取任务**而非难任务——它只需要一个大窗口，而那已由准入保证。在此之上再跳到前沿档，是为同一件事付两次钱。

**处置**：把 `long_context_quality` 从 `high_quality` 中摘除。真正该抬高下限的是"上下文刚被压缩过"（模型丢了细节），而不是"上下文长"——后者进 ② 的轨迹信号。

#### 2.2 轨迹信号（Agent 场景的主要信号源）

**在 agent 轨迹里，绝大多数 turn 不难。** 难的 turn 由轨迹形状识别，不由文本识别：

```
发一个符合 schema 的工具调用       → 机械
总结一次成功的工具输出             → 简单
按计划执行下一步                   → 简单
──────────────────────────────────
连续第 3 次工具报错，重新想办法     → 难
用户说"不对，还是错的"             → 难
长用户消息后的首次规划             → 难
刚做完上下文压缩                   → 难
```

判据全部免费、结构化，不需要理解一个字：

| 信号 | 判据 | 方向 |
|---|---|---|
| 工具循环 | 同一 tool + 相同 args 连续 ≥ 2 次 | 抬高 |
| 错误密度 | 最近 N 条 tool result 中错误占比超阈值 | 抬高 |
| 用户纠正 | user 消息紧跟 assistant 且命中否定窄模式 | 抬高 |
| 轨迹超长 | turn 数超过该任务档案的历史分位 | 抬高 |
| 上下文压缩 | 本轮 input 显著小于上轮 | 抬高 |
| 机械轮次 | `has_tool_result && 无错误 && max_tokens 小` | 不抬高（可用最低档） |

`FeatureFrame.content.has_tool_result` 已经存在，它就是"本轮在解读工具输出"的直接信号，也是其中最有用的一个。

**新增结构**（`urouter-contracts::features`，纯函数，可穷举测试）：

```rust
pub struct TrajectoryFeatures {
    pub turn_index: u32,
    pub consecutive_tool_errors: u32,
    pub repeated_tool_call: bool,
    pub user_correction: bool,
    pub context_shrunk: bool,
    /// 来自 TaskBinding：该任务此前升级到过的下限。
    pub prior_escalation_floor: Option<String>,
}
```

前五项从 `messages` 数组直接算得，第六项从既有绑定读取。**没有一项需要调用模型。**

#### 2.3 难度在任务内是黏的

如果某任务在第 7 轮升过一次级，它的下限应保持抬高，而不是第 8 轮又从最便宜档重新试探。

`TaskBinding` 已记录 `tier` / `bound_at_turn` / `last_seen_turn`，增加一个 `escalated_floor: Option<String>` 即可。**成本接近于零，却挡掉了后续全部重复升级开销**——这是本层性价比最高的一条。

#### 2.4 Agent 轨迹与单轮对话的不对称

单轮对话里选便宜了，最多答得差一点；**agent 轨迹里选便宜了，会发出一个错的工具调用 → 报错 → 重试 → 更多轮次，污染整条轨迹**。

因此：

> Agent 轨迹使用**更高的起始下限**与**更敏感的升级触发**；单轮对话可以更激进地从便宜档起步。

这不是对第一性原理的妥协，恰恰是它的正确应用——"最优"是**期望**成本最优，不是单次账面最便宜。区分依据用既有的 `contract.agent.harness` 是否存在，或 `available_tool_count > 0`。

#### 2.5 调用方声明的语义改动

从"就用这一档"降级为"**不低于**这一档"：

```
旧：difficulty: hard  →  锁定最贵档
新：difficulty: hard  →  floor = 中端档，在其之上仍选最便宜的
```

这修掉了 1.2 的激励错配：声明 `hard` 不再直接兑换成"最贵的模型"，只是抬高下限。`preference.pin_tier` 保留原有强制语义（显式意图不应被静默改写），但文档需写明它绕过成本优化。

#### 2.6 配置结构

`route.json`，随 Route 签名发布：

```json
"quality_floors": {
  "schema_version": 1,
  "default": "efficient",
  "agent_default": "cheap",
  "by_task": {
    "conversation":  "efficient",
    "data_extract":  "cheap",
    "code_write":    "mid",
    "plan":          "mid",
    "math":          "capable"
  },
  "by_trajectory": [
    {"when": {"consecutive_tool_errors_at_least": 2}, "floor": "capable"},
    {"when": {"repeated_tool_call": true},           "floor": "capable"},
    {"when": {"user_correction": true},              "floor": "mid"},
    {"when": {"context_shrunk": true},               "floor": "mid"}
  ]
}
```

放在 Route 里的理由与 `semantic_rules` 一致：签名 control manifest 已对它哈希，因此白拿 revision 绑定、热加载、`last_good` 与一键回滚，不需要第二条分发通道。

**轨迹信号只抬高下限，从不降低。** 结构信号能证明"这里出问题了"，不能证明"这里很简单"；降低下限的权力只交给 Layer 4 的离线反事实评估，因为那是唯一有统计支撑（ESS 门禁）的地方。

#### 2.7 ③ 的落地依赖：任务意图标签

`by_task` 需要一个**索引键**，而现有的 `SemanticTask` 只有四个值：

```rust
// crates/urouter-core/src/lib.rs:400
pub enum SemanticTask { Greeting, RealtimeWeather, EquationSolving, General }
```

其中一个还是"不知道"。**经验下限表最多只能有 3 个有效条目**，覆盖不了真实流量。

因此 ③ 的落地依赖一个独立模块，设计见 [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md)。这里只记录与本方案接口相关的三点：

**（a）标签是索引键，不是难度判断。** 标签本身不含难易信息；`by_task` 的下限值全部来自 L2 配对实验的实测数据。这条是 2.0 判断（难度不是输入文本的属性）的直接延续——**允许按任务类型区分路由，不允许按文本猜难度**。

**（b）`by_task` 的每一条都必须能在 L2 报告里指出出处。** 没有实测支撑的条目不得进入配置，否则与"猜难度"无异。

**（c）先测差异，再建分类器。** 若 L2 显示各类任务的最优档位没有显著差异，说明区分任务类型对路由没有价值，③ 这一来源应当**留空**而非硬填——此时 floor 由 ①②④ 三者决定，本方案其余部分不受影响。

在意图模块落地前，③ 保持为空是**合法且安全的**状态：`quality_floors.by_task` 缺省即不参与 `max()`。

**验收**：`/v1/explain` 输出 `quality_floor` 及其生效来源（四者中哪一个）；四个来源各有单测；轨迹判据在合成轨迹上有表驱动测试；`prior_escalation_floor` 有跨轮次测试；`by_task` 为空时 floor 仍由其余三者正确合成。

### Layer 3 运行时升级（本方案的核心）

#### 3.1 廉价结构化验证器

**廉价结构化验证器**（`urouter-contracts::verify`，纯函数，零 I/O）：

```rust
pub enum VerifyVerdict {
    Sufficient,
    Insufficient { reason: InsufficientReason },
    Unknown,          // 无法判断 → 视同 Sufficient，绝不因不确定而升级
}

pub enum InsufficientReason {
    EmptyCompletion,        // 空回复
    TruncatedByLength,      // finish_reason == "length" 且未达客户端 max_tokens
    MalformedToolCall,      // 声明了 tools 但 tool_calls 结构不合法
    SchemaViolation,        // 声明了 response_format 但输出不符
    ExplicitRefusal,        // 明确拒答（窄模式匹配，宁可漏判）
}
```

四条设计原则：

1. **只用免费信号。** 不默认启用 judge 模型——judge 的成本会吃掉节省，那是把成本从 A 挪到 B。
2. **`Unknown` 视同通过。** 验证器的失败模式必须是"不升级"，否则不确定性会系统性推高成本，直接违背第一性原理。
3. **窄模式，宁可漏判。** 与语义分类同样的取舍：误判升级的代价（多花一次调用）高于漏判（一次质量稍差的回答）。
4. **纯函数。** 验证器接收已解析的响应，不做 I/O，可穷举测试。

#### 3.2 升级执行

**升级执行**（`urouter-gateway::execute_routed_upstream`）：

```
verdict == Insufficient
  && escalations_used < max_escalations          // 默认 1
  && 首 token 未发出                              // 流式已开始则不升级
  && (无会话绑定 || 在 binding_grace 内)
  && 预算允许下一档的最坏成本
  → 选择 floor 之上的下一档，重试，两次尝试都写入 attempts[]
```

**硬护栏**（缺一不可）：

- `--auto-max-escalations`（默认 **1**）：最多升一级。允许无限升级等于回到"总是用最贵的"，只是绕了一圈。
- 预算预留必须按 `(escalations+1)` 跳的最坏链路计算——不能出现"够走第一跳、升级后超支"。
- 升级后如任务需要绑定，绑定到**升级后**的模型。
- 升级次数与原因进 DecisionRecord 与 metrics。

**验收**：空回复触发一次升级并成功返回；`Unknown` 不触发升级；流式首 token 后不升级；升级后 `in_flight` 与配额租约无泄漏（这条尤其重要，评审 P0-2 相关路径已有前车之鉴）。

#### 3.3 绑定与迁移边界：一个任务里什么时候可以换模型

升级会导致任务中途换模型，这与会话绑定直接冲突。uRouter 已有正确的答案结构——`MigrationBoundary`（`urouter-core/src/lib.rs:259`）：

```rust
pub enum MigrationBoundary {
    NewTask, AfterCompaction, ToolRoundCompleted,
    BeforeFirstAssistantToken, ExplicitUserRetry, TerminalProviderFailure,
}
```

不在这些点上换模型，`apply_task_binding` 返回 `BoundModelIneligible` → `unsafe_task_migration` 直接拒绝。**默认是绑定优先于成本，这条不改。**

成本升级允许使用的边界：

| 场景 | 可否换 | 边界 | 理由 |
|---|---|---|---|
| 首轮，尚未绑定 | ✅ 自由 | — | 无历史可破坏 |
| 验证不通过、首 token 未发出 | ✅ | `BeforeFirstAssistantToken` | 本轮尚未产出任何内容 |
| 工具轮次干净收尾 | ✅ | `ToolRoundCompleted` | 天然接缝 |
| 上下文刚压缩 | ✅ | `AfterCompaction` | 历史本来就被改写了 |
| **工具调用循环中途** | ❌ | — | A 发出的 tool_call id 约定与 parallel 语义，B 未必续得上 |
| **流式已吐 token** | ❌ | — | 撤不回来 |
| **推理链中途** | ❌ | — | `reasoning_content` 不跨模型 |

**升级必须单向且黏性。** 升上去就不再降回来（Layer 2.3 的 `escalated_floor`）。在便宜档与贵档之间来回抖比一直用贵的更糟——多花的钱一分没省，还多了一堆失败轮次。

#### 3.4 主线保持绑定，副调用自由路由

**在多轮 agent 任务里拿到成本收益的正确方式，不是逐轮切换主模型，而是把副调用路由到便宜档。**

uRouter 已有 `CallRole::Auxiliary`（`urouter-core/src/lib.rs:252`），且它**绕过主任务绑定**。一个 agent 任务里除主对话线外还有大量副调用：

```
主线（绑定，不动）
    决定下一步 / 生成面向用户的回答 / 发出工具调用
────────────────────────────────────────────────
副调用（自由路由，每次独立选最便宜的足够档位）
    压缩历史 / 生成检索 query / 抽取结构化字段
    给文件或分支起名 / 判断是否需要调工具 / 给中间结果打分
```

副调用**不进入对话历史**，因此换模型不造成行为跳变，也不影响主线的 prompt cache；它们通常还短、机械、结构可验证——正是 Layer 3.1 验证器最有效的场景。

这条**几乎零风险且已实现**，缺的是产品侧让 agent harness 真的去标注 `call.role = auxiliary`。应作为接入文档的一等要求，而非可选项。

#### 3.5 三层策略汇总

```
主线首轮      → 按 floor 选最便宜的足够档位，绑定
主线后续轮    → 保持绑定；只在 3.3 的边界上、单向、黏性地升级
              → 成本比较使用 1.1 的带缓存边际成本
副调用        → 完全自由，每次独立选最便宜的足够档位
```

一个任务里**会**出现不同模型——但不是主线在抖，而是**主线稳定 + 副调用各取所需**。

**验收**：工具循环中途的升级被拒绝并记录原因；`ToolRoundCompleted` 边界上的升级被接受；副调用不受主绑定约束且可落在更低档；升级后绑定指向升级后的模型。

---

### Layer 4 闭环学习

**记录**（扩展 DecisionRecord v2，gateway 本地结构，非 `deny_unknown_fields`，纯附加）：

```
quality_floor          本次生效的下限及其来源
tier_cost_estimates    各候选的预估成本
verify_verdict         每次 attempt 的验证结论
escalation             是否升级、从哪档到哪档、原因
```

**离线评估**（`urouter-eval`，已有估计量直接可用）：

反事实问题正是 IPS/SNIPS/DR 擅长的形式：

> 在历史流量上，如果把 `math` 的下限从 `capable` 降到 `mid`，
> 质量指标下降多少、成本下降多少？ESS 是否足以支撑这个结论？

这里必须坚持既有的**支持域门禁**：样本支持不足的区域不允许下调下限。这是 `urouter-eval` 已经实现的机制。

**策略语义改造**（`urouter-artifact`）：

artifact 的输出从"是否升级（二值）"改为"**该任务的质量下限是哪一档**"。三道守卫（kill switch / 操作预算 / 支持域）与 canary 分桶全部保留不变。

**探索**（`apply_controlled_exploration`）：

保持哈希派生、保持五个前置条件、保持默认关闭。它在本方案中的作用是采集"更便宜的档位本来能不能行"的反事实样本——这正是它被设计出来的用途，只是此前没有消费者。

**验收**：`urouter-eval` 能输出"下限从 X 降到 Y 的成本/质量权衡 + ESS"；支持域不足时明确拒绝而非给出结论。

---

### Layer 5 池子：第一性原理的物理前提

**"选最优模型"在 2 个候选之间是没有意义的。** 当前 44 provider / 9 模型、36 家无模型，这不再只是排期问题，而是**本方案的物理前提**。

需要的不是"更多模型"，而是**成本梯度**：

| 档位 | 典型成本 | 来源 |
|---|---|---|
| `local` | 0（自有算力） | vLLM / SGLang，已有 2 个 |
| `free` | 0 | AI Horde 等，已有 |
| `cheap` | $0.1–0.5 / 1M | OVH、开源模型托管 |
| `mid` | $1–5 / 1M | 主流中端 |
| `frontier` | $10+ / 1M | 前沿 |

每档至少 2 个部署以获得容错。这需要凭据，不需要代码。

**验收**：`route.json` 至少 4 个 tier 且成本单调递增；`urouter-catalog cost` 能给出各档单请求预估成本。

---

## 四、执行顺序与依赖

| 阶段 | 内容 | 前置 | 可并行 |
|---|---|---|---|
| **A0** | 统一 token 估算（评审 P0-1） | — | — |
| **A1** | `select_cheapest_above_floor` + `QualityRule` 改造 | A0 | 与 A2 并行 |
| **A2a** | `long_context_quality` 从 `high_quality` 摘除 | — | 独立，最小改动 |
| **A2b** | `TrajectoryFeatures` 提取（纯函数） | — | 与 A1/A3 并行 |
| **A2c** | `TaskBinding.escalated_floor` 黏性下限 | A2b | — |
| **A2d** | `quality_floors` 配置与四来源 floor 合成 | A2a,A2b | ③ 可留空 |
| **A2e** | ③ 经验下限：L2 配对实验 → `by_task` | A2d + 池子 3+ 档 | 见意图模块 I0 |
| **A1b** | 带缓存的边际成本（`cache_warm` 注入） | A1 | — |
| **A3** | 结构化验证器（纯函数 + 测试） | — | 与 A1/A2 并行 |
| **A3b** | 副调用主动降档（`CallRole::Auxiliary`） | A1 | **独立，零风险，最先可上** |
| **A4** | 运行时升级 + 迁移边界 + 护栏 | A1b,A2d,A3 | — |
| **A5** | DecisionRecord 扩展 + metrics | A4 | — |
| **A6** | `urouter-eval` 下限权衡报告 | A5 + 流量 | — |
| **A7** | artifact 语义改为"下限" | A6 | — |
| **B**  | 池子扩充到 4+ 档梯度 | 凭据 | 全程并行 |

A0 是唯一的硬阻塞。A1 / A2a / A2b / A3 相互独立，可并行开发。**B 应当立刻并行启动**——A1 做完若仍只有 2 个 tier，收益无法体现。

**三条最高性价比、且互不依赖**：

- **A3b 副调用降档** —— `CallRole::Auxiliary` 已实现且绕过绑定，只需在选档时对其应用最低 floor。零风险，可最先上线
- **A2a 长上下文摘除** —— 一处删除，长上下文不再跳最贵档
- **A2c 黏性下限** —— 绑定上加一个字段，挡掉全部重复升级开销

---

## 五、成败指标

优化必须以指标定义成功，否则无法判断是否达成第一性原理。

**主指标**

| 指标 | 方向 | 说明 |
|---|---|---|
| 每请求成本（nano-USD 中位数与 P90） | ↓ | 主要目标 |
| 质量代理指标 | 不劣化 | 验证器通过率、`/v1/feedback` 正向率、客户端重试率 |

**关键诊断指标：升级率（escalation rate）**

这是判断起点是否选对的唯一信号：

```
升级率 过高（> 20%）  → 起点太便宜，抬高 floor
升级率 过低（< 2%）   → 起点太贵，可以降 floor（前提是 ESS 足够）
```

**升级率必须按 agent 轨迹与单轮对话分别统计。** 两者的合理区间不同（轨迹场景起点更高、触发更敏感），合并观察会互相掩盖。

补充诊断：**主线切换率**——一个任务内主线模型变更的次数。健康值应接近 0；持续 > 1 说明升级判据过敏或 `escalated_floor` 未生效（在档位间抖动比一直用贵的更糟）。

补充诊断：**副调用占比与其平均档位**——副调用应占 agent 流量的相当比例且平均档位显著低于主线；若两者档位接近，说明 harness 没有标注 `call.role`，最容易拿的那部分收益没有拿到。

补充诊断：**缓存命中率与切换后的成本变化**——切换到"更便宜"档位后单请求成本反而上升，是缓存边际成本没算对的直接信号。

补充诊断：**轨迹信号命中率**——若某条判据（如"用户纠正"）几乎从不命中，说明模式过窄或该场景不存在，应删除而非放宽（放宽会引入误判，而误判升级的代价是真金白银）。

**护栏指标**（任一越界即回滚）

- P90 端到端延迟（升级会增加一次往返）
- 升级失败率（升级后仍 `Insufficient`）
- 预算超支次数（必须为 0）

**发布方式**：沿用既有 shadow → 1% → 5% → 10% canary，观察窗口内主指标不达标即 `last-good` 回滚。

---

## 六、明确不做的事

| 不做 | 原因 |
|---|---|
| 默认启用 judge 模型验证 | judge 的成本会吃掉节省，把成本从 A 挪到 B 不是优化。可作为 opt-in |
| 无限升级 | 等价于"总是用最贵的"绕一圈，且延迟不可控 |
| 流式发出 token 后升级 | 已发出的内容不能撤回 |
| 为成本破坏会话绑定 | 中途换模型导致行为跳变，是 uRouter 的核心卖点 |
| 引入随机探索 | 破坏 propensity 可复算，反事实评估失效，闭环塌掉 |
| 利用率软降权护栏 | 与 `DeploymentPicker` 打架的第二套未声明策略，且让部署选择对固定快照非确定（评审已列） |
| 把 `pin_tier` 也改成 floor | 显式意图不应被静默改写；但需文档写明它绕过成本优化 |
| 用一个便宜模型当难度分类器 | 用模型决定用哪个模型。若它能可靠判断难度，多半已能回答问题；且分类调用的成本与延迟直接吃掉节省。可作为 opt-in，不作默认 |
| 关键词 / 长度 / 疑问词启发式判难度 | "为什么"比"是什么"难属于民间智慧，在真实流量上不成立。现有语义分类器只留 3 条窄规则正是因为这个 |
| 嵌入相似度匹配难度语料 | 需要标注语料、需要一次 embedding 调用、且随流量分布漂移 |
| 让轨迹信号降低下限 | 结构信号能证明"出问题了"，不能证明"很简单" |
| 无实测支撑地填写 `by_task` | 与"猜难度"无异；每条必须能在 L2 报告里指出出处 |
| 为省钱在非迁移边界上换主线模型 | 工具调用 id 约定、parallel 语义、推理链都不跨模型；`unsafe_task_migration` 存在就是为了挡这个 |
| 升级后再降回便宜档 | 档位间抖动多花的钱一分没省，还多了失败轮次。升级单向且黏 |
| 按标价而非边际成本比较候选 | 长上下文多轮场景下缓存折扣常超过档位价差，按标价选会更贵 |

---

## 七、与评审方案的关系

本方案把评审中的两条从"技术债"重新定位为"第一性原理的阻塞项"：

| 评审条目 | 原定级 | 本方案中的地位 |
|---|---|---|
| P0-1 token 估算 3 倍分歧 | 正确性缺陷 | **硬前置**——成本优化必须建立在可信的成本数字上 |
| P2-2 决策核复杂度超过配置 | 投入错配 | **物理前提**——2 个 tier 谈不上"最优" |

其余评审条目（P0-2 配额合并、P1-1 观测一致性、P1-2 死字段、P1-3 transport 特化、P2-1 `main.rs` 切分）与本方案正交，可独立推进。

---

## 八、一句话总结

当前 Auto 模式的 `QualityRule` 在结构上只能选 `candidates.last()` 或 `candidates.first()`，而触发它的 `high_quality` 完全由调用方声明——这两点合起来，就是"一直用最贵的模型"的机制性原因。

**不要试图从用户的问题判断难度**——难度不是输入文本的属性，它取决于有没有工具、上下文里有没有答案、答案要用来干什么。应当从三处取信号：这次调用**要求**什么（准入，确定）、轨迹**显示**什么（结构判据，免费）、便宜模型**是否真的做到了**（验证，事后确定）。前两者只决定起点与下限，最终判断交给第三者。

对 Agent 场景，最有价值的是**轨迹信号**：绝大多数 turn（发工具调用、总结工具输出、按计划执行）并不难，而难的 turn（工具循环、连续报错、用户纠正、上下文压缩）由轨迹形状识别，不需要理解一个字。且**难度在任务内是黏的**——记一次就省下后续所有重复试探。

一个任务里**会**用到不同模型，但收益不来自主线逐轮切换——那既破坏行为一致性，又因为丢掉 prompt 缓存而常常更贵。收益来自**主线保持绑定、副调用自由降档**：压缩历史、生成检索 query、抽取字段这类调用不进入对话历史，换模型零风险，而它们在 agent 流量中占比很高。`CallRole::Auxiliary` 已经实现，缺的只是让 harness 去标注它。

方案的核心是把决策从**预测式**（猜这个请求有多难 → 选对应档位）改为**验证式**（从足够便宜处起步 → 用免费的结构化信号判断是否够用 → 必要时升一级 → 把结果喂回离线评估以修正下限）。

uRouter 已经具备实现它所需的几乎全部零件——精确计价、能力准入、单次请求特征、任务绑定状态、确定性探索、反事实评估、canary 发布、决策证据链。缺的是把它们连成回路、给绑定加上难度记忆，以及一个有真实成本梯度的模型池。

---

## 相关文档

- [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) — Layer 2 ③ 的索引键来源
- [`uRouter_智能路由评测方案.md`](uRouter_智能路由评测方案.md) — L1–L4 评测体系，`by_task` 的数据来源
- [`uRouter_技术架构评审方案.md`](uRouter_技术架构评审方案.md) — 架构评审与 P0/P1/P2 分级
- [`uRouter_决策算法说明.md`](uRouter_决策算法说明.md) — 七层决策器现状
- [`uRouter_技术架构流程图.md`](uRouter_技术架构流程图.md) — 总体架构与请求流程
- `crates/urouter-contracts/src/tier.rs` — `QualityRule` / `DefaultRule` 所在
- `crates/urouter-eval/src/lib.rs` — IPS / SNIPS / DR 与 ESS
