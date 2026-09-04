# uRouter 路由智能强化：合并执行方案

> 制定日期：2026-09-04
> 合并对象：[`uRouter_决策智能强化执行方案.md`](uRouter_决策智能强化执行方案.md)（轨迹信号）+ [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md)（意图标签）
> 定位：**这份是执行方案之记录**；那两份保留为设计细节的出处，不再单独排期
> 全部代码位置已核对

---

## 零、为什么必须一起做

原本的判断是"两者正交，可以分开推进"。核对 `select_tier` 的级联实现之后，这个判断需要修正：

> **两者不是正交的两件事，是同一个机制的两半。单独做轨迹会先让路由变差。**

### 0.1 级联的实际结构

`crates/urouter-contracts/src/tier.rs:227`：

```rust
let filtered = input.candidates.iter()
    .filter(|c| c.index >= input.floor_index)      // ← 意图标签设的下限
    .collect();
let rules: [&dyn TierRule; 3] = [&PinRule, &QualityRule, &DefaultRule];
```

两条规则的选法（`tier.rs:182` / `:206`）：

```rust
QualityRule:  high_quality → candidates.last()    // 最贵的
DefaultRule:                 candidates.first()   // floor 之上最便宜的
```

**`floor_index` 与 `high_quality` 是两个独立入口，作用在同一个级联上：**

| 谁 | 控制什么 |
|---|---|
| **意图标签** → `floor_index` | 候选集的**下界在哪** |
| **轨迹信号** → `high_quality` / `low_cost` | 在这个集合里**往哪个方向走** |

### 0.2 单独做轨迹，会放大一个已知缺陷

`high_quality` 一旦为真，走的是 `.last()`——**直接跳到最贵的那一档**，中间档不可达。

轨迹信号做的事情恰恰是**增加 `high_quality` 为真的途径**（severity、spinning、exploring 三个新入口）。所以：

```
单独上轨迹  →  更多请求触发 high_quality  →  更多请求直跳最贵档
            →  与第一性原理（保质量前提下用最优而非最贵）方向相反
```

而意图标签设的 `floor_index` 才是让 `.first()` 变成"**这类任务实测下限之上最便宜的那一档**"的那个机制——它是中间档变得可达的唯一途径。

**两者合起来才构成"抬到该抬的地方，不多抬"。分开做，先做的那一半单独看都是退步。**

### 0.3 现在还看不出来，是因为只有两档

```
gateway/route.json      → ['efficient', 'capable']
gateway/route.free.json → ['free', 'free-fast']
```

**两档时 `.first()` / `.last()` 覆盖全集**，二元跳跃没有代价。但 G3（模型深度 9 → 每档 3+）一落地就会出现第三档，缺陷当场显形。

**所以三件事必须同期落地**：模型深度、意图下限、轨迹方向。少任何一件，另外两件都不成立。

---

## 一、合并后的整体结构

```
        ┌──────────────── 意图标签 ────────────────┐
输入 ──▶│  这是什么类型的任务  →  查 L2 实测下限   │──▶ floor_index
        └──────────────────────────────────────────┘        │
        ┌──────────────── 轨迹信号 ────────────────┐        ▼
        │  这次对话进行得怎么样 → 加权有符号分数   │──▶ 方向 ──▶ 级联
        └──────────────────────────────────────────┘
```

两个信号源，一个消费点，**一次 L2 实验同时标定两者**。

这是合并最实在的收益：**L2 配对实验是整个计划里最贵的一步**（每类任务 × 每个档位各跑一遍真实调用）。分开做要跑两次，合并只跑一次——而且合并后的那次还更有信息量，因为它同时给出「按标签分组的档位质量成本表」，这张表既是 `by_task` 的来源，也是轨迹权重的标定基准。

---

## 二、五条不可让步的约束

前四条来自轨迹方案，第五条来自意图模块，全部保留。

| # | 约束 | 依据 |
|---|---|---|
| **1** | 提取/分类必须落在 `urouter-core::decide()` 内 | `urouter-embed/src/lib.rs:74` 与网关同入口，放网关则 parity 破裂 |
| **2** | 全部纯函数，无时钟/随机/IO | `tools/urouter-xtask/src/pure_crates.rs:11` 的 CI 门禁 |
| **3** | 调用方声明优先于网关推断 | `SignalContract`（`core:333`）是公开契约，不得静默覆盖 |
| **4** | `RouteConfig` 新字段必须 `skip_serializing_if` | 否则作废所有已部署 revision，`the_shipped_route_revision_is_pinned` 会拦 |
| **5** | 不改 `SemanticTask` 枚举 | 已序列化进 DecisionRecord v2，被 `urouter-eval` 读取；新增独立字段 |

**合并带来的一条新约束**：

| **6** | 两个开关必须独立 | 证据可能只支持其中一个。`intelligence.trajectory.mode` 与 `intelligence.intent.mode` 分开，可独立灰度、独立回滚 |

---

## 三、阶段划分

| 阶段 | 内容 | G3 依赖 | 形态 |
|---|---|---|---|
| **P0** | 符号修复 + 共享 shadow 管道 | 否 | 直接生效 |
| **P1** | 两个信号的**生产侧**（并行） | 否 | shadow |
| **P2** | 消费侧**一次改到位** | 否 | shadow |
| **P3** | **一次 L2 实验**标定两者 | **是** | 离线 |
| **P4** | 独立开关灰度 | 是 | 正式 |

**P0–P2 完全不依赖 G3，可立即启动。** G3 与 P0–P2 并行推进。

---

## P0 — 地基（可立即开始）

### P0.1 修 `production_intensity` 符号

`crates/urouter-core/src/lib.rs:1303` 把它并入了升档条件，Switchyard 原式里它带减号（`opensource/Switchyard/crates/libsy/src/algorithms/util/stage.rs:363`）。

```rust
let signal_quality  = signal_score(contract, &["severity", "spinning", "exploring"]) > 500;
let signal_low_cost = signal_score(contract,
    &["cost_sensitive", "disposable", "production_intensity"]) > 500;
```

一行改动 + 一个**锁住符号**的测试。

### P0.2 共享 shadow 管道

**这是合并的第一个实在收益：只建一次。**

```rust
// RouteConfig 新增（必须 skip_serializing_if）
pub struct IntelligenceConfig {
    pub trajectory: SignalModeConfig,   // off | shadow | on
    pub intent:     SignalModeConfig,
}
```

配套一次性建好、两个模块共用：

- `DecisionRecord` 新增 `intelligence` 段（本地结构，无 `deny_unknown_fields`，可直接加，不升 schema）
- `/v1/explain` 输出推断结果与**来源**（`declared` / `extracted` / `rule` / `artifact`）
- `--dry-run` 加 `intelligence_config_consistency`，**18 → 19**（`dry_run.rs:73` 的 `[&str; 18]` 与两处 `len()` 断言同步改）

⚠️ **不要动 `FeatureFrame`**（`crates/urouter-contracts/src/features.rs:12`）——`deny_unknown_fields` + `schema_version`，加字段会作废所有已发布 artifact。留到 P3 之后。

### P0 验收

- `mode: off` 时 `decide()` 输出**逐字节不变**（现有 parity 语料回归）
- 钉死的 route revision 测试通过
- `xtask check-pure-crates` 通过

---

## P1 — 两个信号的生产侧（可并行）

两条子线互不依赖，可以由不同的人同时做。

### P1-A 轨迹提取

新建 `crates/urouter-core/src/trajectory.rs`，在 `decide()` 的 `parse_contract`（`core:679`）之后调用。

**输入形状是白拿的**：五个入站协议全部先归一化成 OpenAI-chat 再重入 `chat_completions`（`main.rs:4028`/`:4038`/`:4103`/`:4119`），`decide()` 永远只看到一种格式。

⚠️ **但归一化不是无损的**。`crates/urouter-protocol/src/lib.rs:743`：

```rust
.find(|part| part.get("type") == Some("tool_result"))   // ← 只取第一个
```

Anthropic 把 3 个 `tool_result` 塞进**一条**消息。按消息数计数：OpenAI 得 3，Anthropic 得 1。

**计数规则**：

```
工具结果载荷数 = Σ (role == "tool" 的消息) of max(1, content_parts)
```

**必须有的测试**：同一条逻辑轨迹分别从 `/v1/chat/completions` 与 `/v1/messages` 送入，断言四个维度**逐字段相等**。

严重度模式表、`spinning`/`exploring` 互斥、窗口化取最大等细节见[轨迹方案 §1.5–1.6](uRouter_决策智能强化执行方案.md)，此处不重复。

### P1-B 意图规则层

`TaskIntent` + `intent_rules`，**只做规则层，不建分类器**。

理由（意图模块 §八的 I0）：**先用 L2 测出各类任务的最优档位确实不同，再投入建分类器。** 反过来做（先建分类器再找用途）是常见的浪费。

规则层此刻的作用不是路由，是**给 P3 的 L2 实验提供分组口径**——没有标签就没法按类别切分实验。

标签集（意图模块 §4.2）：`conversation` / `code_write` / `code_explain` / `data_extract` / `translate` / `summarize` / `plan` / `math` / `tool_use` / **`unknown`（默认）**。

### P1 验收

| # | 验收项 |
|---|---|
| 1 | 跨协议一致性测试通过 |
| 2 | 严重度三条性质各有测试 |
| 3 | shadow 下决策结果不变，`/v1/explain` 出现两组推断 |
| 4 | shadow 数据里统计工具名 `Other` 占比与标签 `unknown` 占比——**两个都是"名单不匹配"的早期警报** |

---

## P2 — 消费侧一次改到位

**这一阶段是合并的核心。分开做会先劣化（§0.2），所以两个改动必须在同一个 PR 序列里。**

### P2.1 `select_tier` 改形

现在（`core:1310`）是一组 `||`：任何一条成立就翻转，无权重、无置信度。

改成：

```rust
// ① 下限：意图标签 → by_task 查表（P3 之前恒为空，即不参与）
let floor_index = max(caller_floor, intent_floor, trajectory_hard_floor);

// ② 方向：轨迹加权，全整数
score_millis = w_sev*severity + w_spin*spinning + w_expl*exploring
             - w_prod*production_intensity;          // ← 减号，P0.1 的结构化版本

if score_millis.abs() < confidence_threshold_millis {
    // 弃权 —— 不表态，回落其余判据
}
```

**`tanh` 换成整数阈值比较**：`tanh` 只是为了把分数挤进 (-1,1) 便于设阈值，整数域直接比绝对值等价且可重放。这是对 Switchyard 的改进,不是简化。

**弃权路径必须存在**。这与意图模块"标签体系必须有 `unknown` 且是默认值"是同一条原则的两个应用。Switchyard 的 fall-open 第 4 级是调 LLM 分类器——**uRouter 不做**（热路径外部调用违反约束 2），弃权即回落。

### P2.2 ⚠️ `QualityRule` 必须同期改

**这是 §0.2 的直接后果，也是分开做最危险的地方。**

`tier.rs:182` 现在是 `candidates.last()`——`high_quality` 直跳最贵档。轨迹信号会显著增加 `high_quality` 为真的频率。

改为**抬一档而非跳到顶**：

```rust
// high_quality 的语义从"要最好的"改成"当前这一档不够，往上抬"
QualityRule: candidates.get(1).or(candidates.last())
```

**为什么现在改是安全的**：目前只有两档（`route.json` → `['efficient','capable']`），`get(1)` 与 `last()` **结果完全相同**——这次改动在今天是**行为等价**的，可以零风险合入，等第三档出现时自动生效。

**错过这个窗口的代价**：G3 加了第三档之后再改，就是一次真实的行为变更，需要重新走灰度。

### P2.3 结果证据降档

需要修正 [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) §2.5 的禁令。原文：

> 轨迹信号只抬高下限，从不降低。

Switchyard 的 `should_deescalate`（`stage.rs:386`）表明这划得过宽：

```rust
tests_passed && (recent_write + recent_edit) >= 1 && severity <= 0.0
```

这不是"看起来简单所以降档"，是**"这一轮已经收尾了所以降档"**——用的是**结果证据**。

**禁令改写为**：预测性信号只抬高下限；**结果性信号**（测试通过、任务完成标记、无报错连续段）可以降低下限。

规则顺序照抄（`stage.rs:408` 注释明说理由）：**升档检查在降档之前**，这样"测试通过但同时出了 critical 错误"仍然升档。

### P2 验收

- 表驱动测试：`(floor, 方向, 档位数)` → 期望档位
- **`QualityRule` 改动在两档配置下行为逐字节等价**（这是它能零风险合入的证据）
- 三档合成配置下，`high_quality` 落在中间档而非最贵档
- 弃权测试：低置信度时输出与无信号时完全一致
- **顺利轨迹不发生升档**——这条比"困难轨迹发生升档"更重要,过敏的判据会系统性推高成本

---

## P3 — 一次 L2 实验，标定两者

**合并最大的收益在这里：最贵的一步只跑一次。**

### 前置

**G3 完成**：模型深度从 9 补到每档 3+。

建议先补**最小可测集合**（efficient / mid / capable 各 3 个真实可用、价格已知的模型），而不是把 44 个 provider 全铺开。**L2 需要的是深度不是宽度。**

### 实验设计

按 P1-B 的标签切分类别，每类在每档各跑一遍（`urouter-lab gateway-benchmark --suite ... pin_tier`），产出：

```
(标签 × 档位) → (质量, 成本, 延迟)
```

**这一张表同时回答三个问题**：

| 问题 | 读法 | 用途 |
|---|---|---|
| 各标签的最优档位是否不同？ | 逐标签取"质量与最高档差距 ≤ ε 的最便宜档" | **意图模块的 I0 决策点**——全都相同则停止建分类器 |
| `by_task` 每条填什么？ | 同上 | 意图下限表 |
| 轨迹权重怎么标？ | 按轨迹维度分层再看同一张表 | 轨迹加权的基准 |

**增列一项**（对标分析 G8，来自 AutoMix 的 `HOPELESS` 洞见）：不只看"最便宜的达标档",还要看**"最高档是否达标"**。最高档都不达标的类别应当**降档**（反正做不成,别花贵的钱),而不是升档。

### P3 验收

- 每个标签在报告里都有出处；**没有实测支撑的 `by_task` 条目不得进入配置**
- I0 判定明确写下：各标签最优档位是否显著不同
- `HOPELESS` 类别单独列出

---

## P4 — 独立开关灰度

**两个开关独立**（约束 6）。证据支持哪个开哪个。

| 开关 | 打开条件 |
|---|---|
| `intent.mode = on` | I0 判定为"各标签最优档位显著不同" + `by_task` 每条有出处 |
| `trajectory.mode = on` | L2 显示轨迹能改变的那部分请求上成本下降且质量不降 + L3 的 IPS/SNIPS/DR **三个估计量同向**且 ESS 过门禁 |

**任何一条不满足就不开。** 三个估计量分歧时的正确动作是继续采样，不是取平均。

灰度用现有的稳定哈希分桶，不新建机制。

---

## 四、改动清单

| 文件 | 改动 | 阶段 |
|---|---|---|
| `crates/urouter-core/src/lib.rs:1303` | `production_intensity` 移入 `signal_low_cost` | P0 |
| `crates/urouter-core/src/lib.rs` `RouteConfig` | `intelligence` 配置块（**`skip_serializing_if`**） | P0 |
| `crates/urouter-gateway/src/main.rs` | DecisionRecord `intelligence` 段 + `/v1/explain` | P0 |
| `crates/urouter-gateway/src/dry_run.rs:73` | **18 → 19**，两处 `len()` 断言同步 | P0 |
| `crates/urouter-core/src/trajectory.rs` | **新建**：提取 + 模式表 | P1-A |
| `crates/urouter-core/src/intent.rs` | **新建**：`TaskIntent` + `intent_rules` | P1-B |
| `crates/urouter-core/src/lib.rs:679` 后 | 调用两者、合并进 contract、记来源 | P1 |
| `crates/urouter-core/src/lib.rs:1310` `select_tier` | `\|\|` → floor + 有符号加权 + 弃权 | P2 |
| **`crates/urouter-contracts/src/tier.rs:182`** | **`QualityRule` `.last()` → 抬一档**（两档下等价） | **P2** |
| 同上 | 硬升档 / 硬降档 / 打分器 / 弃权 四级 | P2 |
| `gateway/route.json` | 补第三档（G3 之后） | P3 |
| `catalog/catalog.json` | 模型深度 9 → 每档 3+ | **G3，并行** |
| `crates/urouter-contracts/src/features.rs:12` | `FeatureFrame` 加维度，**升 schema_version** | P3 后 |
| `crates/urouter-artifact/src/lib.rs` | `task_label` + `signal_weights` | P3 后 |
| `uRouter_Auto模式成本最优化技术方案.md` §2.5 | 改写降档禁令 | P2 |

---

## 五、合并的收益与风险

### 收益

| # | 收益 | 量级 |
|---|---|---|
| 1 | **L2 实验只跑一次** | 最贵的一步省一半 |
| 2 | shadow 管道、`/v1/explain`、DecisionRecord 字段、dry-run 检查只建一次 | 中 |
| 3 | **`QualityRule` 在两档窗口期内零风险改掉** | 错过就要重走灰度 |
| 4 | 避免"单独上轨迹先劣化"（§0.2） | **这是合并的主因** |
| 5 | `select_tier` 只改一次形，不是连着改两次 | 中 |

### 风险与处置

| 风险 | 处置 |
|---|---|
| 一半被阻塞会拖住另一半 | **两个开关独立**（约束 6）；P1-A / P1-B 两条子线无相互依赖，可并行 |
| 范围变大，PR 变难评审 | P0 / P1-A / P1-B / P2 各自独立可合入；只有 P2 内部的两个改动必须同 PR |
| `QualityRule` 改动被误判为无关重构 | 提交信息与测试里写明"两档下行为等价，为第三档预留"，并附行为等价测试 |
| G3 迟迟不到位 | P0–P2 全部不依赖它；shadow 数据持续积累，不浪费 |

---

## 六、明确不做

| 不做 | 理由 |
|---|---|
| 热路径 LLM 分类器 / 嵌入调用 | 摧毁可重放性——uRouter 唯一没有对手的性质 |
| P3 之前建意图分类器 | I0 未判定,可能整个标签体系没有信息量 |
| AutoMix 式每请求自验证 | 延迟与调用数翻倍。只在 L2 增列"最高档是否达标"取其洞见 |
| `tanh` 与浮点打分 | 整数阈值等价且可重放 |
| 中文错误模式 | 无标定数据,凭空引入假阳性。等 shadow 数据再定 |
| 静默覆盖调用方声明 | 客户端失去表达能力,排障无法区分"推断错"与"声明错" |
| P3 之前动 `FeatureFrame` | 会作废所有已发布 artifact |
| 无实测支撑地填 `by_task` | 与"猜难度"无异 |

---

## 七、里程碑

| 里程碑 | 判定 | G3 依赖 |
|---|---|---|
| **M0** | P0 合入，`mode: off` 下决策逐字节不变 | 否 |
| **M1** | P1-A + P1-B 合入，`/v1/explain` 可见两组推断 | 否 |
| **M2** | shadow 开启，`Other` 工具占比与 `unknown` 标签占比有统计 | 否 |
| **M3** | P2 合入，`QualityRule` 行为等价测试通过 | 否 |
| **M4** | **G3 完成**（每档 3+ 模型，route 补第三档） | — |
| **M5** | P3 一次 L2 出表，I0 判定写下 | M3+M4 |
| **M6** | 两个开关按各自证据独立灰度 | M5 |

**M0–M3 全部不依赖 G3。G3 与它们并行推进。**

---

## 八、一句话总结

> **意图标签决定"下限在哪"，轨迹信号决定"往哪个方向走"——它们是同一个级联的两个入口。单独上轨迹只会让更多请求直跳最贵档，与第一性原理相反；两者合起来才是"抬到该抬的地方，不多抬"。**

立刻可以开始的三件事，互不依赖：

1. **P0.1** — 一行 + 锁符号的测试
2. **P2.2** — `QualityRule` 改抬一档（**两档下行为等价，现在改零风险；G3 之后再改就要重走灰度**）
3. **G3** — 补最小可测集合（每档 3 个真实可用、价格已知的模型）

---

## 相关文档

- [`uRouter_决策智能强化执行方案.md`](uRouter_决策智能强化执行方案.md) — 轨迹侧设计细节（模式表、互斥判据、跨协议一致性）
- [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) — 意图侧设计细节（标签体系、三层结构、I0 决策点）
- [`uRouter_对标开源路由器技术差距分析.md`](uRouter_对标开源路由器技术差距分析.md) — G1–G8 差距清单
- [`开源LLM路由项目技术框架对比分析.md`](开源LLM路由项目技术框架对比分析.md) — 五范式分类
- [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) — §2.5 待 P2 改写
- [`uRouter_智能路由评测方案.md`](uRouter_智能路由评测方案.md) — P3 的 L2/L3 依据
