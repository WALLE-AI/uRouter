# uRouter 任务意图识别模块设计

> 制定日期：2026-09-04
> 目的：为智能路由提供**任务标签**，作为经验下限表的索引键
> 依据：对 `urouter-core` 语义分类、`urouter-artifact` 推理、`deny.toml` 依赖边界逐行核对
> 关联：[`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) Layer 2 ③、[`uRouter_智能路由评测方案.md`](uRouter_智能路由评测方案.md)

---

## 零、这个模块解决的是什么问题

Auto 方案的 Layer 2 定义了质量下限的四个来源，其中 ③ 是**任务档案经验下限**——"历史上这类任务在哪个档位以上成功率达标"。

但它现在无法落地：`SemanticTask` 只有四个值。

```rust
// crates/urouter-core/src/lib.rs:400
pub enum SemanticTask { Greeting, RealtimeWeather, EquationSolving, General }
```

因此 `quality_floors.by_task` 最多只能有 4 个键，其中一个还是"不知道"。**经验下限表没有足够的键可用。**

本模块提供那些键。

---

## 一、先立一个区分，它决定整个设计的对错

意图识别用在路由上有两种用法，一种可靠、一种不可靠：

```
✅ 意图作为「键」，去查实测出来的经验下限
   "这是 code_write" → 查表 → 该类任务在 mid 档以上成功率 ≥ 95% → floor = mid
   下限来自 L2 配对实验的实测数据，意图只负责索引

❌ 意图直接推断难度，进而选档位
   "这看起来是复杂编程任务" → 用贵模型
   这是用文本猜难度，回到民间启发式
```

**本模块只做第一种。**

理由在 Auto 方案 Layer 2.0 已论证：**难度不是输入文本的属性**——同一句话的难易取决于有没有工具、上下文里有没有答案、答案要用来干什么。既有语义分类器只保留 3 条窄规则、其余显式弃权，正是基于这个判断，这份克制不应被打破。

模块的产物因此是一个**标签**，不是一个档位判断。标签本身不含"难"或"易"的语义；难度信息全部来自标签所索引到的实测数据。

---

## 二、载体：扩展 `urouter-artifact`，不新建子系统

`RouterArtifact::infer` 已具备热路径分类器所需的全部性质，逐条核对如下：

```rust
// crates/urouter-artifact/src/lib.rs:252
pub fn infer(&self, features: &FeatureFrame, semantic_task: &str, operation_limit: u32)
    -> Result<Inference, InferenceError>
```

| 性质 | 现状 | 为什么必需 |
|---|---|---|
| 全整数运算（MLP / KNN / Contrastive） | ✅ | 跨平台可重放 → 可签名发布 |
| 有界推理 | ✅ `operation_limit < required_operations` 即拒 | 分类器本身不能成为延迟来源 |
| 支持域门禁 | ✅ `ArtifactSupportDomain{semantic_tasks, maximum_input_text_bytes, tools_supported}` | 域外显式回退而非外推 |
| kill switch | ✅ | 操作员的无条件停止开关 |
| canary 分桶 | ✅ 稳定哈希 | 灰度且同任务不中途跳变 |
| shadow 模式 | ✅ | 只推理不生效，先观测 |
| revision 绑定 | ✅ `control_revision_changed` 自动回退 | 目录换挡后不拿旧模型推理 |
| last-good 回滚 | ✅ | — |

**这些性质从零重建一遍的代价，远高于给 `Inference` 加两个字段。**

### 2.1 唯一需要的改动

```rust
pub struct Inference {
    pub artifact_revision: String,
    pub tier: String,                   // 保留，语义不变
    pub task_label: Option<String>,     // 新增：任务标签，None = 弃权
    pub label_confidence_millis: u16,   // 新增：低于阈值即视为弃权
    pub score_millis: i64,
    pub operations: u32,
}
```

`ArtifactSupportDomain.semantic_tasks` 已经是 `BTreeSet<String>`，直接用作标签支持域，无需新字段。

### 2.2 明确不使用 `urouter-infer`

`urouter-infer`（ONNX）目前只存在于离线工具链，且它拖着 **RUSTSEC-2021-0073**（`prost-types` 0.6.1，无修复版本）。`deny.toml` 用一条 ban 把它挡在网关之外：

```toml
[[bans.deny]]
name = "onnx-pb"
wrappers = ["urouter-infer", "urouter-lab"]
```

该 advisory 被 ignore 的正当性**完全建立在"它不在请求路径上"**。把 ONNX 推理拉进网关会让这条 ignore 当场失效。

**结论：热路径分类只用 `urouter-artifact` 的整数推理。**

---

## 三、三层结构

```
第 1 层  规则（已有）        窄关键词 + 显式弃权          免费  确定  覆盖窄
第 2 层  learned 标签（新）  整数 artifact 推理            免费  确定  可泛化
第 3 层  离线 LLM 标注       仅用于生产训练集，不上热路径   有成本 离线
```

优先级：规则命中优先（可归因、可审计），规则弃权时才用 learned 标签，两者都弃权则 `unknown`。

### 3.1 冷启动路径

这是让模块真正可用的关键——没有标注数据，learned 层无从训练。

```
线上流量（recording != none, allow_training）
   ↓  urouter-lab dataset          隐私 / 留存 / 删除治理，已实现
   ↓  离线 LLM 打标签              一次性，成本可控，人工抽检校准
   ↓  urouter-lab train            训练整数 artifact，已实现
   ↓  urouter-lab verify           确定性与操作预算校验，已实现
   ↓  签名发布 → shadow → canary → active
   ↓
线上：只做整数推理，零网络调用
```

**用 LLM 做离线标注是合理的**：它不在请求路径上，成本一次性且可控，且可以人工抽检校准。

**用 LLM 做在线分类是不合理的**：用模型决定用哪个模型——若它可靠到能判断任务类型，多半已能回答问题；而分类调用的成本与延迟会直接吃掉节省。这与 Auto 方案「不做的事」中的同一条一致。

---

## 四、标签体系

### 4.1 三条硬约束

1. **必须有 `unknown` 且它是默认值。** 沿用既有 `abstained` 语义：分不出来时回退到既有策略，而不是猜一个。这是整个决策核"处处显式弃权"原则的延续。

2. **每个标签必须能在 L2 配对实验里被测量。** 一个测不出经验下限的标签是没用的标签——它索引不到任何数据。标签的存在理由是"它对应的最优档位与别的标签不同"。

3. **宁少勿多，十个左右是上限而非起点。** 每个标签都需要独立的实测样本；标签越多，每个标签的 ESS 越低，经验下限越不可靠。

### 4.2 建议起始集合

按**产出物形态**单一维度切分（不要混入难度、领域、语言等其他维度）：

| 标签 | 含义 | 预期区分度来源 |
|---|---|---|
| `conversation` | 闲聊、简单问答 | 最低档通常足够 |
| `code_write` | 生成代码 | 正确性可自动判定，档位差异明显 |
| `code_explain` | 阅读/解释代码 | 偏抽取，可能比 write 低 |
| `data_extract` | 从文本抽取结构化信息 | 强结构约束，低档可能足够 |
| `translate` | 翻译 | — |
| `summarize` | 摘要 | 偏抽取 |
| `plan` | 多步规划 | agent 场景主要难点 |
| `math` | 数学 / 逻辑推理 | 需 reasoning 能力 |
| `tool_use` | 工具调用编排 | 结构合规性可自动判定 |
| `unknown` | 显式弃权（默认） | — |

**这是起始集合，不是最终集合。** 上线后由 L2 数据决定：若两个标签的最优档位始终相同，应合并；若某标签长期零命中，应删除。

### 4.3 与现有 `SemanticTask` 的关系

两者**并存，不替换**：

| | `SemanticTask`（已有） | `task_label`（新增） |
|---|---|---|
| 回答 | 这次调用**必须具备**什么能力 | 这是**哪一类**任务 |
| 产物 | `requires_tools` / `requires_reasoning` / `required_tool` | 一个字符串标签 |
| 去向 | 硬准入（布尔排除） | 经验下限表的索引键 |
| 可否弃权 | 是（`General`, confidence 0） | 是（`unknown`） |

既有的三条规则继续负责**需求抽取**——`weather → required_tool` 这条尤其是安全断言（网关绝不伪造工具结果），与标签无关，不得合并。

---

## 五、接口与数据结构

### 5.1 决策核

```rust
// urouter-core：新增独立字段，不改 SemanticTask
pub struct TaskIntent {
    pub label: String,                  // 含 "unknown"
    pub confidence_millis: u16,         // 0 = 弃权
    /// 归因：`builtin:<rule>` / 配置规则 id / `artifact:<revision>`
    pub source: String,
    pub abstained: bool,
}
```

### 5.2 路由配置（`route.json`，随 Route 签名发布）

```json
"quality_floors": {
  "schema_version": 1,
  "default": "efficient",
  "agent_default": "cheap",
  "by_task": {
    "conversation": "efficient",
    "data_extract": "cheap",
    "code_write":   "mid",
    "plan":         "mid",
    "math":         "capable"
  }
}
```

`by_task` 的**每一条都必须能在 L2 报告里指出出处**。没有实测支撑的条目不允许进入配置——这是与"猜难度"的分界线。

### 5.3 意图规则外置

沿用既有 `semantic_rules` 的机制与语义（`mode: extend|replace`、`equals`/`contains_any`/`contains_all`、按声明顺序首个命中者胜），新增 `task_label` 字段：

```json
"intent_rules": {
  "schema_version": 1,
  "mode": "extend",
  "rules": [
    {"id": "sql_generation", "task_label": "code_write",
     "contains_any": ["写一段 sql", "生成建表语句", "write some sql"],
     "confidence_millis": 900}
  ]
}
```

放在 Route 里的理由与 `semantic_rules` 一致：签名 control manifest 已对它哈希，因此白拿 revision 绑定、热加载、`last_good` 与一键回滚，不需要第二条分发通道。

---

## 六、改动清单

| # | 改动 | 位置 | 风险 |
|---|---|---|---|
| 1 | `TaskIntent` 结构 + 规则层 | `urouter-core` | 低，纯新增 |
| 2 | `intent_rules` 配置与校验 | `urouter-core::RouteConfig::validate` | 低 |
| 3 | `Inference` 增 `task_label` / `label_confidence_millis` | `urouter-artifact` | 低，纯附加 |
| 4 | 支持域按标签门禁 | `urouter-artifact` | 无，复用 `semantic_tasks` |
| 5 | `quality_floors.by_task` 接受新标签 | `route.json` | ⚠️ 改 `RouteConfig` → 见下 |
| 6 | 标签进 DecisionRecord | `urouter-gateway` | 低，本地结构无 `deny_unknown_fields` |
| 7 | `/v1/explain` 暴露 `task_intent` | `urouter-gateway` | 低 |
| 8 | 离线标注与训练流程 | `tools/urouter-lab` | 中，新增子命令 |
| 9 | L1 套件标签用例族 | `eval/suites/` | 低 |

### ⚠️ 两处必须注意

**(a) 不要改 `SemanticTask` 枚举。**

它是已序列化的公共枚举，被持久化进 DecisionRecord v2 并由 `urouter-eval` 读取。改它是 wire-format 破坏。**新增独立字段而非扩展枚举**——与架构评审中"排除原因不要从 `Vec<String>` 迁成枚举"是同一条理由：字符串是 wire 格式，类型是解释。

**(b) `RouteConfig` 新增字段必须带 `skip_serializing_if`。**

`revision()` 哈希整个结构体的序列化，新字段若总是序列化，会作废所有已部署路由的 revision、artifact 绑定与签名 manifest。既有回归测试 `the_shipped_route_revision_is_pinned` 会拦住违规——失败时先检查 `skip_serializing_if`，再改常量。

---

## 七、评测

意图模块必须独立评测，且**不能只测分类准确率**。

| 层 | 验的是 | 判据 |
|---|---|---|
| **L1** | 标签稳定性 | 同输入同 revision 下标签逐字节一致（决策可重放） |
| **L1** | 弃权率 | `unknown` 占比。过低 = 过度自信；过高 = 模块无用 |
| **L2** | ★ **标签有效性** | **每个标签的最优档位确实不同** |
| **L3** | 下限可信度 | 每个标签的 ESS 是否支撑其下限；三估计量是否同向 |
| **L4** | 轨迹内一致性 | 同一任务内标签是否稳定；频繁跳变说明特征不足 |

### ★ L2 是判定本模块有没有价值的唯一标准

分类准确率再高，**若所有标签的最优档位都相同，这个标签体系就没有信息量**——它索引不到任何有用的差异，等于白做。

因此上线顺序必须是：**先做 L2 配对实验拿到"各类任务的最优档位"，确认它们确实不同，再投入建分类器。** 反过来做（先建分类器再找用途）是常见的浪费。

---

## 八、执行顺序

| 阶段 | 内容 | 前置 | 说明 |
|---|---|---|---|
| **I0** | L2 配对实验，按候选标签分类统计各档质量/成本 | 池子有 3+ 档 | **决定要不要做后续** |
| **I1** | `TaskIntent` + `intent_rules` 规则层 | I0 | 先规则，覆盖窄但确定 |
| **I2** | `quality_floors.by_task` 接入标签 | I1 + I0 数据 | 下限必须有出处 |
| **I3** | 离线标注 + 训练 + `verify` | I1 有线上标签数据 | 冷启动 |
| **I4** | `Inference` 输出标签，shadow 观测 | I3 | 只推理不生效 |
| **I5** | canary → active | I4 观测达标 | 沿用既有灰度 |

**I0 是决策点，不是准备工作。** 若 I0 显示各类任务的最优档位没有显著差异，**应当停止**——那说明区分任务类型对路由没有价值，规则层已经够用。

---

## 九、明确不做的

| 不做 | 原因 |
|---|---|
| 在线调用 LLM 做意图分类 | 用模型决定用哪个模型；成本与延迟直接吃掉节省。离线标注可以 |
| 把 ONNX 推理拉进网关 | `onnx-pb` 的 RUSTSEC ignore 正当性建立在"不在请求路径上" |
| 意图直接映射到档位 | 那是用文本猜难度，本模块的产物是索引键不是难度判断 |
| 扩展 `SemanticTask` 枚举 | 已序列化、进 DecisionRecord、被 `urouter-eval` 读取 |
| 把需求抽取合并进意图分类 | `weather → required_tool` 是安全断言（不伪造工具结果），与标签正交 |
| 无实测支撑地填写 `by_task` | 与"猜难度"无异；每条必须能在 L2 报告里指出出处 |
| 一次上二十个标签 | 每个标签需独立样本；标签越多 ESS 越低，下限越不可靠 |
| 引入随机性做标签平滑 | 破坏 propensity 可复算，反事实评估失效 |

---

## 十、一句话总结

这个模块提供的是**索引键，不是判断**。

意图标签本身不含难易信息；难度信息全部来自标签所索引到的 L2 实测数据。这样既拿到了"按任务类型区分路由"的能力，又不违背"难度不是输入文本的属性"这一判断。

载体已经在仓库里——`urouter-artifact` 的整数推理具备可重放、有界、可签名、可灰度、可回滚全部性质，**给 `Inference` 加两个字段，比重建一套分类子系统便宜得多**。

而是否值得建，由 I0 决定：**先测出各类任务的最优档位确实不同，再建分类器。**

---

## 相关文档

- [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) — Layer 2 ③ 经验下限，本模块的消费方
- [`uRouter_智能路由评测方案.md`](uRouter_智能路由评测方案.md) — L1–L4 评测体系
- [`uRouter_技术架构评审方案.md`](uRouter_技术架构评审方案.md) — 依赖边界与 wire-format 约束
- `crates/urouter-core/src/lib.rs:400` — `SemanticTask` 现状
- `crates/urouter-artifact/src/lib.rs:252` — `infer` 与支持域
- `deny.toml` — `onnx-pb` 依赖边界
