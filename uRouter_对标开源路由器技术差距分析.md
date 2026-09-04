# uRouter 对标开源路由器：逐维度技术差距分析

> 制定日期：2026-09-04
> 对标对象：LiteLLM、Switchyard、LLMRouter、FreeLLMAPI（`opensource/` 下五个 LLM 路由项目中的四个，`pi` 作客户端参照、`role_model` 与本领域无关）
> 与 [`开源LLM路由项目技术框架对比分析.md`](开源LLM路由项目技术框架对比分析.md) 的分工：那份以**开源项目**为主体横向比较，本份以 **uRouter** 为主体，逐维度对标并给出可执行的差距清单
> 全部结论基于源码核对，代码位置随文标注

---

## 零、核心结论

在对标过程中发现了一件此前没意识到的事：

> **uRouter 的档位选择器已经在消费 Switchyard 那四个轨迹维度，名字一字不差——但方向错了一个，而且没有任何代码在生产这些信号。**

`crates/urouter-core/src/lib.rs:1303`：

```rust
let signal_quality = signal_score(
    contract,
    &["severity", "spinning", "exploring", "production_intensity"],
) > 500;
```

这直接改变了差距的性质。原本以为"轨迹路由"是一个待建的子系统，实际情况是：**消费侧已经建好，缺的是生产侧的一个纯函数**，外加一处符号修正。

三条主结论：

| # | 结论 | 性质 |
|---|---|---|
| **1** | `production_intensity` 的符号是反的 | ⚠️ **缺陷**，见 §2.2 |
| **2** | 四个信号只能由调用方声明，网关不提取 | 缺口，一个纯函数可补 |
| **3** | 治理骨架领先，决策依据落后 | 战略取舍，方向正确 |

---

## 一、先摆事实：uRouter 的实际形态

对标前先把主体量化清楚，避免拿印象比较。

### 1.1 代码构成（核对自 `crates/`）

| crate | 行数 | 职责 |
|---|---:|---|
| `urouter-gateway` | 21,041 | HTTP、Redis、上游调用 |
| **`urouter-contracts`** | **4,266** | **纯函数决策核** |
| `urouter-ai` | 2,205 | 目录与能力模型 |
| `urouter-core` | 2,150 | 路由配置与 `decide()` |
| `urouter-protocol` | 1,620 | 入站/出站协议翻译 |
| `urouter-transport` | 1,428 | 出站 provider 适配 |
| `urouter-artifact` | 1,188 | 整数推理工件 |
| `urouter-eval` | 1,096 | 反事实估计 |
| 其余 6 个 | 1,635 | embed / client / types / infer / py |
| **合计** | **36,514** | 457 个测试通过 |

**`urouter-gateway` 占 58%。** 这个比例本身是个信号：真正做决策的 `contracts` + `core` 合起来 6,416 行，不到网关的三分之一。

### 1.2 纯函数核的硬证据

`crates/urouter-contracts/Cargo.toml`：

```toml
[dependencies]
serde.workspace = true
serde_json.workspace = true
```

**4,266 行决策逻辑，两个依赖，都是序列化库。** 没有 HTTP、没有时钟、没有随机数、没有 Redis。这条由 `xtask check-pure-crates` 在 CI 里强制。

这是本次对标中 uRouter 唯一**没有任何对手**的地方——四个开源项目里没有一个把决策逻辑与 I/O 分离到可以整体重放的程度。

### 1.3 供给侧

| | 数量 |
|---|---:|
| providers（`catalog/catalog.json`） | 44 |
| **models** | **9** |
| instances（`catalog/providers/instances.json`） | 53 |

**44 个 provider 对 9 个模型。** 供给的**宽度**已经铺开，**深度**几乎是空的。这个数字对后面每一个"路由质量"的结论都是前置约束——见 §4.1。

### 1.4 档位决策的实际逻辑

`core:1290–1330`，`select_tier` 计算两个布尔量再交给级联：

```rust
let high_quality = hint.difficulty == "hard"
    || hint.workload == "plan"
    || preference_bias_millis(...) < -500
    || signal_quality                     // ← 四个轨迹维度的 max
    || long_context_quality;              // ← 长上下文

let low_cost = auxiliary
    || hint.value_class ∈ {"auxiliary","disposable"}
    || preference_bias_millis(...) > 500
    || signal_low_cost;
```

**注意这是一组 `||`。** 任何一条成立就翻转，没有权重、没有加总、没有置信度——与 Switchyard 的 `tanh` 加权打分是完全不同的结构。

---

## 二、逐维度对标

### 2.1 轨迹信号：消费侧已建，生产侧为零

**这是最重要的一节。**

| | Switchyard | uRouter |
|---|---|---|
| 维度定义 | `severity` / `spinning` / `exploring` / `production_intensity` | **同名四个** |
| **信号来源** | **从请求体 `messages` 解析** | **调用方在 `urouter.signals[]` 里声明** |
| 合成方式 | 加权和 → `tanh` → 有符号分数 | `max()` → 阈值 500 → 布尔 |
| 置信度 | `confidence = |score|`，不足则弃权 | 无 |
| 降档 | 硬规则（测试通过+有产出+无报错） | 无（`production_intensity` 反被当升档信号） |

**生产侧确实为零**，已核对：`urouter-gateway` 内 `signals` 的全部出现点都是 `ingest_piggyback`（`main.rs:3834`）——把调用方随请求捎带的反馈写进反馈库，**没有任何代码从 `messages` 里提取轨迹**。

这意味着今天的实际行为是：

```
智能体客户端不改代码 → signals 恒为空 → signal_quality 恒为 false
                     → 四个维度对路由没有任何影响
```

而 Switchyard 证明了这四个维度**本来就躺在请求体里**（工具调用、工具结果、轮次、压缩标记全在 `messages` 中），提取只需纯字符串扫描。

**差距的真实大小**：一个 `contracts/src/trajectory.rs`，输入 `&[Message]`，输出四个有界维度。零 I/O、零依赖、可进纯函数核。消费侧、配置侧、trace 侧全都不用动。

### 2.2 ⚠️ `production_intensity` 的符号是反的

`core:1303` 把四个维度一起丢进 `signal_score`，取 `max()`，超过阈值即 `signal_quality = true` → `high_quality = true` → **升到贵档**。

Switchyard 的定义（`opensource/Switchyard/crates/libsy/src/algorithms/util/stage.rs:363`）：

```rust
let raw = SIGNAL_UNIT
    * (d.severity / HARD_SEVERITY + d.spinning + d.exploring - d.production_intensity);
//                                                            ↑ 减号
```

`production_intensity = ratio(write + edit, all_ops)` —— **智能体正在稳定产出代码**。Switchyard 用它**降档**（活干得顺，用便宜的就够）。uRouter 用同一个名字**升档**。

我自己写的 [`Switchyard_深度技术解读报告.md`](Switchyard_深度技术解读报告.md) 第 434 行把这个减号记录得清清楚楚：

> `production_intensity` | → efficient | 近期窗口内落地的写入与编辑

**符号是在移植过程中丢的。** 而且 [`uRouter_设计文档.md`](uRouter_设计文档.md):1050 原本的设计是"四个维度的权重从 `RouterArtifact.calibration.signal_weights` 读取，默认值用 Switchyard 已标定的那组"——现在的 `max()` 实现是那个设计的简化版，简化时把符号一起丢了。

**影响**：今天为零（没人产生信号）。但一旦按 §2.1 补上提取，这条会立刻生效，且方向恰好与成本优化的第一性原理相反——**智能体干得越顺，越往贵的模型上送**。

**必须在补提取之前修掉。** 修法有两种：

| 方案 | 改动 | 代价 |
|---|---|---|
| A. 最小修正 | 把 `production_intensity` 从 `signal_quality` 移到 `signal_low_cost` | 一行，立即可做 |
| B. 结构修正 | 换成带符号的加权和 + 置信度阈值 | 与 `RouterArtifact.signal_weights` 的原设计对齐，但改了 `select_tier` 的形状 |

**建议先做 A**（一行、零风险、立刻消除反向激励），把 B 归入 Auto 方案 Layer 2 的正式改造。

### 2.3 内容信号：与 LiteLLM 同一水位

| | LiteLLM AutoRouter | uRouter `semantic_rules` |
|---|---|---|
| 输入 | `messages[-1]`（`auto_router.py:121`） | 最后一条用户消息（`core:168` 注释） |
| 方法 | 嵌入相似度 | 关键词匹配 |
| 热路径代价 | 1 次嵌入调用 | 0 |
| 输出 | 直接是模型名 | `SemanticTask`（4 值）+ 需求抽取 |
| 可重放 | ❌ 依赖外部嵌入服务 | ✅ |

**信号面完全相同**——都只看最后一条消息。uRouter 用关键词换掉了嵌入，赢了成本与可重放性，输了泛化能力。

一个结构性区别值得强调：LiteLLM 的路由名**就是**模型名，配置作者手写映射；uRouter 的 `SemanticTask` 只做需求抽取（能力要求），不直接决定模型。后者是更好的分层——这正是 [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) 里"意图是索引键不是难度判断"那条的架构基础。

### 2.4 部署选择：单目标 vs 多目标

`contracts/src/capacity.rs:115`：

```rust
pub enum DeploymentPicker { Weighted, LeastLoaded, LowestLatency, LowestQuotaUsage }
```

**四个都是单目标。** 对比 FreeLLMAPI（`scoring.ts:44`）的凸组合：

```ts
balanced: { reliability: 0.5,  speed: 0.25, intelligence: 0.25 }
fastest:  { reliability: 0.35, speed: 0.55, intelligence: 0.10 }
```

**注意 `fastest` 的 reliability 仍有 0.35。** 注释写明理由：不让"快但坏掉"的模型赢。

uRouter 的 `LowestLatency` 没有任何这类下限。此前修掉的"8ms 返回 401 的部署看起来最快"（延迟采样门控）是这个问题的一个**实例**；一般形式还在——单目标排序本身没有健康度地板。

不过 uRouter 有 FreeLLMAPI 没有的东西：熔断器、配额账本、`plan_capacity_lease` 的确定性快照决策。**把多目标加进来的正确方式是给 picker 加一个健康度门槛（准入），而不是加权求和（会破坏确定性排序的可解释性）。**

### 2.5 成本进入决策：都不及格，但败因不同

| | 做法 | 问题 |
|---|---|---|
| LiteLLM | `item_cost = input_price + output_price`（`lowest_cost.py:295`） | 标量相加，不看请求形状；未知模型按 $5/token 哨兵值参与算术（`:284`） |
| LLMRouter | 可配权重，**默认 `cost: 0`** | 默认根本不优化成本 |
| FreeLLMAPI | 无 | 全免费额度，成本恒为 0 |
| **uRouter** | **不进档位选择** | 档位序即价格序是**隐含约定**，没有任何代码验证 |

uRouter 的败因是最轻的（没做 ≠ 做错），但也最需要补：这正是 [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) 存在的理由。

补的时候**不要抄 LiteLLM 的标量模型**——正确形式是 `in_tokens × p_in + out_tokens × p_out`，依赖 [`uRouter_技术架构评审方案.md`](uRouter_技术架构评审方案.md) P0-1 的 token 估算统一先落地。

### 2.6 反馈闭环：唯一的代际差

| | 能回答"换个策略会怎样"？ |
|---|---|
| LiteLLM | ❌ 六种策略上线多年，无估计器 |
| Switchyard | ❌ 只有 Prometheus 计数 |
| FreeLLMAPI | ❌ |
| LLMRouter | ⚠️ 靠**全因子网格**规避——每条 query × 每个模型都实测一遍 |
| **uRouter** | ✅ IPS / SNIPS / DR + ESS 门禁 + `SupportDomain` |

LLMRouter 的做法值得单独说：它的训练数据是所有臂都被观测到的完整网格，所以路由退化成普通监督学习，**不需要反事实推断**。代价是离线要付 N 倍推理费，且模型池一变就得重跑。

uRouter 走的是相反路线——只有生产日志的赌博机反馈，靠估计器补上未观测的臂。**这个取舍是对的**（供给侧 44 provider 且在增长，全网格不可维护），但它把 ESS 门禁变成了不可放弃的东西：三个估计量分歧时的正确动作是继续采样，不是取平均。

### 2.7 治理与可运维性：无对手

| 能力 | LiteLLM | Switchyard | LLMRouter | FreeLLMAPI | uRouter |
|---|---|---|---|---|---|
| 决策逐字节可重放 | ❌ | ✅ | ✅ | ⚠️ | ✅ |
| 配置签名 + revision 绑定 | ❌ | ❌ | ❌ | ⚠️ Ed25519 feed | ✅ HMAC manifest |
| 一键回滚到 last-good | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| 决策证据链持久化 | ❌ | ❌ | ❌ | ❌ | ✅ DecisionRecord v2 |
| 启动前静态校验 | ❌ | ⚠️ | ❌ | ❌ | ✅ `--dry-run` 18 项 |
| 灰度 / 影子 / kill switch | ⚠️ | ⚠️ | ❌ | ❌ | ✅ |

FreeLLMAPI 的 Ed25519 签名目录 feed 是唯一在治理上有可比之处的设计（uRouter 的 `catalog_feed.rs` 正是借鉴自它）。

**这一格的领先不是"做得更多"，是"做了别人没做的那一类"。** 而且它是后补不上的：决策可重放要求纯函数核，纯函数核要求从第一天就把 I/O 挡在外面。

---

## 三、五种范式，uRouter 用了几种

用 [`开源LLM路由项目技术框架对比分析.md`](开源LLM路由项目技术框架对比分析.md) 的分类：

| 范式 | uRouter 现状 |
|---|---|
| **A 语义匹配** | ✅ 关键词版（弱化的 A） |
| **B 轨迹打分** | ⚠️ **消费侧已建、生产侧为零、符号有误** |
| **C 级联+自验证** | ❌ 无（也**不建议**做，见 §5） |
| **D 离线监督学习** | ✅ `urouter-artifact` 整数推理 + `urouter-lab train` |
| **E 在线赌博机** | ⚠️ 有确定性探索与 propensity，但无质量维度的臂 |

**五种里占了三种半，比任何单个开源项目都多。** 但每一种都在最浅的实现层次：A 是关键词、B 没接上、D 的工件缺训练数据（因为只有 9 个模型，见 §4.1）。

---

## 四、两个必须先承认的前置约束

对标很容易得出"补上 X 就领先了"的结论。有两条现实约束会让那种结论落空。

### 4.1 9 个模型撑不起分级路由

**44 provider / 9 model** 这个比例是所有路由质量结论的天花板：

- **档位分级需要每档至少 2–3 个可用模型**，否则一个模型下线整档就空了
- **L2 配对实验需要每档都能跑同一套用例**，9 个模型分摊到 3–4 档，每档 2–3 个，且要覆盖不同能力
- **`urouter-artifact` 的训练需要足够的臂**，9 个臂里若有几个是本地 fixture，有效臂更少
- **反事实估计的 ESS 随臂数下降**

**结论：在模型深度补上来之前，任何路由算法的改进都测不出统计显著的效果。** 这不是可以并行推进的事——它是前置依赖。

44 个 provider 已经注册但只有 9 个模型，说明 provider 条目大多没有对应的模型条目（缺 `max_output_tokens` / `cost` / `compat` 等无法从 freellmapi 导出中获得的字段）。**补模型深度 = 补这些字段，是数据工作不是代码工作。**

### 4.2 `turn_depth` 在多协议入口上不可靠

Switchyard 自己的注释（`tool_signals.rs:236`）：

> 消息数是轮次深度的代理。**依赖线格式**——Anthropic 把工具结果批量塞进更少的消息，OpenAI-chat 不会——所以以此为门槛的判据在跨请求来源时是近似的。

uRouter 同时暴露 `/v1/chat/completions`、`/v1/messages`、`/v1/responses`、Gemini、Ollama 五种入口。**同一条轨迹从不同入口进来，消息数不同，路由结果就会不同。**

补 §2.1 的提取函数时必须避开这个坑：**数工具结果的个数，不数消息数**。这一点 Switchyard 自己都没做到（它承认是近似），是 uRouter 可以做得更好的地方——因为 uRouter 有 `urouter-protocol` 的统一 IR，可以在**翻译后**的表示上提取信号，天然消除线格式差异。

---

## 五、差距清单（按"改动量 ÷ 收益"排序）

| # | 事项 | 改动量 | 收益 | 前置 |
|---|---|---|---|---|
| **G1** | 修 `production_intensity` 符号（方案 A） | **1 行** | 消除反向激励 | 无 |
| **G2** | `contracts/src/trajectory.rs`：从统一 IR 提取四维度 | 一个纯函数 + 模式表 | **激活已建成的消费侧** | G1 |
| **G3** | 补模型深度（9 → 每档 3+） | 数据工作 | **解锁所有评测** | 无 |
| **G4** | 成本进入档位比较（`in×p_in + out×p_out`） | 中 | 第一性原理的核心 | P0-1 token 估算 |
| **G5** | `DeploymentPicker` 加健康度准入门槛 | 小 | 修 §2.4 的一般形式 | 无 |
| **G6** | 带符号加权 + 置信度弃权（方案 B） | 中 | 与 artifact 可训练权重对齐 | G2 |
| **G7** | 基于结果证据的降档（测试通过+有产出+无错） | 小 | 见下 | G2 |
| **G8** | L2 报告增列"最高档是否达标" | 报告字段 | 识别 `HOPELESS` 类别 | G3 |

**G1 + G2 是一个 PR 的量，且能把一个已经写好但完全没通电的子系统接通。** 这是全表里性价比最高的一项。

**G3 是唯一的硬前置**：没有模型深度，G4–G8 的效果都测不出来。

### 关于 G7：需要修正我此前的一条禁令

[`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) 里写的是：

> **轨迹信号只抬高下限，从不降低。** 结构信号能证明"这里出问题了"，不能证明"这里很简单"。

Switchyard 的 `should_deescalate`（`stage.rs:386`）表明这条划得过宽：

```rust
signal.tests_passed
    && (signal.recent_write_count + signal.recent_edit_count) >= 1
    && signal.severity <= 0.0
```

这不是"看起来简单所以降档"，是**"这一轮已经收尾了所以降档"**——用的是**结果证据**（测试真的通过了、代码真的写出来了），不是难度猜测。

原禁令想拦的是"用文本特征猜简单"，那个仍然应该拦。但**结果证据降档**是另一回事，应当放行。建议把禁令改写为：

> 轨迹信号中的**预测性信号**只抬高下限；**结果性信号**（测试通过、任务完成标记）可以降低下限。

Switchyard 的规则顺序也要一并抄：**升档检查在降档之前**，这样"测试通过但同时出了 critical 错误"的一轮仍然升档。

---

## 六、明确不做

| 不做 | 理由 |
|---|---|
| **热路径嵌入调用**（LiteLLM AutoRouter） | 每请求一次外部 I/O，摧毁决策可重放性——那是 uRouter 唯一的护城河 |
| **AutoMix 式每请求自验证** | 延迟与调用数都翻倍。只取 `HOPELESS` 洞见（→ G8），不取机制 |
| **LLMRouter 式全网格训练数据** | N 倍离线推理成本，模型池一变就重跑。44 provider 且在增长，不可维护 |
| **Router-R1 式"模型即工具"** | 要求路由器参与推理过程，与网关架构不可调和 |
| **FreeLLMAPI 式加权求和 picker** | 会破坏 `plan_capacity_lease` 确定性排序的可解释性。改成健康度**准入门槛**（→ G5）而非加权 |
| **LiteLLM 式标量成本模型** | 忽略请求形状。长文抽取与短问长答的正确答案相反 |

---

## 七、一句话总结

> **uRouter 把别人没做的那一层（可审计、可重放、可回滚）做完了，把别人做了的那一层（决策依据）做浅了；而对标发现，最深的那个缺口——轨迹路由——消费侧其实已经建好，缺的是一个纯函数和一个符号。**

最先该做的两件事：

1. **G1**：`core:1303` 把 `production_intensity` 移出 `signal_quality`（一行）
2. **G3**：把模型深度从 9 补到每档 3+（数据工作，是其余一切的前置）

---

## 相关文档

- [`开源LLM路由项目技术框架对比分析.md`](开源LLM路由项目技术框架对比分析.md) — 以开源项目为主体的横向比较与五范式分类
- [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) — G4 / G7 的落点
- [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) — §2.3 内容信号的升级路径
- [`uRouter_智能路由评测方案.md`](uRouter_智能路由评测方案.md) — G8 的落点，L2 配对实验
- [`uRouter_技术架构评审方案.md`](uRouter_技术架构评审方案.md) — P0-1 token 估算，G4 的前置
- [`Switchyard_深度技术解读报告.md`](Switchyard_深度技术解读报告.md) — §2.1 / §2.2 / G7 的来源
- [`LiteLLM_Router_深度技术解读报告.md`](LiteLLM_Router_深度技术解读报告.md)
- [`LLMRouter_深度技术解读报告.md`](LLMRouter_深度技术解读报告.md)
- [`FreeLLMAPI_深度技术解读与uRouter架构对比报告.md`](FreeLLMAPI_深度技术解读与uRouter架构对比报告.md)
