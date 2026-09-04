# 开源 LLM 路由项目技术框架对比分析

> 分析日期：2026-09-04
> 范围：`opensource/` 下六个项目的路由核心源码逐行核对
> 目的：搞清楚"智能路由"在工程上到底有几种做法、各自的前提与代价，以及 uRouter 站在哪里

---

## 零、先说结论

**"智能路由"不是一个问题，是三个被同一个词遮住的问题。** 六个项目里没有一个同时解决全部三个，多数只解决一个：

| 问题 | 定义 | 谁在解决 |
|---|---|---|
| **P1 负载均衡** | 同一个模型的多个部署，这次打哪个 | LiteLLM（6 种策略）、FreeLLMAPI |
| **P2 模型选择** | 这个请求该用哪个模型/档位 | Switchyard、LLMRouter、LiteLLM AutoRouter |
| **P3 任务编排** | 把一个任务拆开分给不同模型 | LLMRouter（多轮/Router-R1） |

LiteLLM 的六个 `RoutingStrategy` 全是 P1。它唯一的 P2 机制是 `AutoRouter`，一个**前置钩子**，与那六个策略正交。把 `lowest_cost` 当成"选便宜模型"是常见误读——它选的是同一个 model group 内最便宜的**部署**。

**六个项目定位：**

| 项目 | 语言 | 解决 | 决策依据 | 生产就绪 |
|---|---|---|---|---|
| **LiteLLM** | Python | P1 为主 + P2 语义钩子 | 运行时健康度 / 嵌入相似度 | ✅ 最成熟 |
| **Switchyard** | Rust | P2 | **对话轨迹信号** | ⚠️ 自称 experimental |
| **LLMRouter** | Python | P2 + P3 | 离线训练的学习模型 | ❌ 研究框架 |
| **FreeLLMAPI** | TS | P1 | 多臂赌博机 + 配额余量 | ✅ 但只面向免费额度 |
| **pi** | TS | 不解决 | 手动 `/model` | — 它是路由的**消费者** |
| **role_model** | Ruby | 不相关 | — | Rails 角色位掩码库，与 LLM 无关 |

`role_model` 与 LLM 路由毫无关系，下文不再提及。`pi` 作为客户端侧参照物在第五节出现一次。

---

## 一、五种"智能"的实现范式

这是全文的核心。六个项目一共用了五种范式，**它们的前提条件差异极大**，选错范式比选错参数严重得多。

### 范式 A：语义匹配（LiteLLM AutoRouter）

`litellm/router_strategy/auto_router/auto_router.py`

把最后一条用户消息做嵌入，与预定义路由的示例语句比相似度，命中哪条路由就用哪个模型：

```python
# auto_router.py:121
user_message: Dict[str, str] = messages[-1]
route_choice = self.routelayer(text=message_content)
model = route_choice.name or self.default_model
```

**注意 `route_choice.name` 直接就是模型名**——路由名与模型名是同一个字符串，映射关系由配置作者手写在 `utterances` 里。

| | |
|---|---|
| **前提** | 你已经知道"哪类问题该用哪个模型"，只是需要一个分类器 |
| **代价** | 每个请求一次嵌入调用（热路径网络 I/O） |
| **失效模式** | 只看 `messages[-1]`。智能体第 15 轮的 `"继续"` 与第 1 轮的 `"继续"` 嵌入完全相同，路由结果也相同——**上下文全部丢失** |
| **可重放性** | 差。依赖外部嵌入服务的输出 |

### 范式 B：轨迹信号打分（Switchyard stage_router）

`crates/libsy/src/algorithms/util/stage.rs` + `tool_signals.rs`

**这是六个项目里最值得细看的设计。** 它不看用户问了什么，只看**这次对话进行得顺不顺**。

信号从**请求体本身**解析出来——不需要客户端配合，不需要会话状态：

```rust
// tool_signals.rs:206
pub struct ToolSignals {
    pub severity: f32,              // 最近窗口内工具报错的最大严重度
    pub no_error_streak: u32,
    pub recent_write_count: u32,    // 窗口内的写入类工具调用
    pub recent_edit_count: u32,
    pub recent_read_count: u32,
    pub recent_todowrite_count: u32,
    pub tests_passed: bool,
    pub turn_depth: u32,
    pub compacted: bool,            // 上下文被压缩过
}
```

严重度靠**子串模式表**判定（`tool_signals.rs:32`）：

```rust
("oom",               CRITICAL, &["out of memory", "memoryerror", ...]),
("connection_refused",CRITICAL, &["connection refused", ...]),
("traceback",         HARD,     &["traceback (most recent call last)"]),
("exit_nonzero",      SOFT,     &["exit code 1", "returned non-zero", ...]),
```

其中一条模式带着这样的注释：

> 锚定为 `"file does not exist"`（而不是裸的 `"does not exist"`，后者会在 `ls` 输出和散文里误触发）——**在 1006 条本地轨迹上挖掘，22 真阳 / 2 假阳**。

这是有人真的跑过生产才写得出的东西。

四个维度合成成一个有符号分数（`stage.rs:285`、`:358`）：

```rust
let spinning  = deep_enough && no_production && !investigating;  // 卡住了
let exploring = deep_enough && no_production && investigating;   // 在调研
let production_intensity = ratio(write + edit, all_ops);         // 在产出

let raw = SIGNAL_UNIT * (severity/HARD_SEVERITY + spinning + exploring - production_intensity);
let score = (SCORE_GAIN * raw).tanh();       // 挤进 (-1, +1)
// confidence = score.abs()
```

**`spinning` 与 `exploring` 互斥**（注释明说：避免在产出轴上重复计数）。正分→强档，负分→弱档，`|score|` 即置信度。

决策是四级瀑布（`stage.rs:408`）：

```
1. 硬升档  critical 错误 / 上下文被压缩         → 强档
2. 硬降档  测试通过 + 刚写过代码 + 无报错        → 弱档
3. 打分器  置信度 ≥ 阈值 → 按符号走
4. Fall open  置信度不足 → 交给 LLM 分类器，或用默认档
```

| | |
|---|---|
| **前提** | 请求体里有工具调用历史（即：这是智能体流量，不是单轮问答） |
| **代价** | 零。纯字符串扫描，无网络调用 |
| **失效模式** | 对单轮对话完全无信号（`no_signal` 分支单独处理）；模式表是语言/工具特定的，换一套工具链要重新挖掘 |
| **可重放性** | 好。纯函数，确定性 |

### 范式 C：级联 + 自验证（LLMRouter AutoMix）

`llmrouter/models/automix/methods.py`

先用小模型跑，让它**自己验证**答案，根据验证分决定是否升级到大模型。最简版是阈值，最完整版是 POMDP。

POMDP 的隐状态设计是这个范式最有价值的一点（`methods.py:249`）：

```python
categories = ["NEEDY", "GOOD", "HOPELESS"]
```

- `GOOD` — 小模型已经做对了，升级是浪费
- `NEEDY` — 小模型做错了但大模型能做对，**升级有价值**
- `HOPELESS` — **大模型也做不对，升级同样是浪费**

`HOPELESS` 这一状态是关键。它把"这题难"和"升档有用"**拆成了两件事**。绝大多数升档逻辑（包括 Switchyard 的、包括 uRouter 目前的设计）都隐含假设"难 ⇒ 升档有收益"，而 AutoMix 明确否认这个假设。

配置里的成本比也很直白（`configs/model_config_train/automix.yaml`）：

```yaml
small_model_cost: 1
large_model_cost: 50
verifier_cost: 1
```

**验证器与小模型同价**——所以级联的账是：省下 49 的概率，要能盖过多付 1 的确定成本。

| | |
|---|---|
| **前提** | 任务的答案可自验证（数学、选择题、有测试的代码） |
| **代价** | 每个请求至少 2 次调用（生成 + 验证），升档时 3 次 |
| **失效模式** | 自验证在开放式生成上不可靠；延迟至少翻倍 |
| **可重放性** | 中。依赖验证器输出 |

### 范式 D：离线监督学习（LLMRouter 16 个 router）

`llmrouter/models/` 下 16 个路由器：KNN、SVM、MLP、矩阵分解、Elo、图神经网络、BERT……全部继承 `MetaRouter`（`meta_router.py:11`）。

**最重要的不是模型结构，是训练数据的形状。** 一条训练记录：

```json
{"task_name":"agentverse-logicgrid", "query":"...", "model_name":"llama3-chatqa-1.5-8b",
 "performance":0.0, "input_tokens":449, "output_tokens":4, "response_time":1.786, ...}
```

**每条 query × 每个候选模型各一条记录。** 这是一个**全因子网格**——所有臂的结果都被观测到了。

这一点决定了一切。有了全网格，路由就退化成普通的监督学习（"给定 query，哪个 model 的 performance 最高且成本可接受"），不需要任何反事实推断。代价是：**离线要付 N 倍推理费用**去构造这个网格。

目标函数是显式加权的：

```yaml
metric:
  weights:
    performance: 1
    cost: 0
    llm_judge: 0
```

默认 `cost: 0`——**默认配置下它根本不优化成本**，纯追质量。要做成本感知路由必须自己调权重。

| | |
|---|---|
| **前提** | 能负担全网格离线评测；候选模型集合稳定 |
| **代价** | 训练数据构造成本 = 样本数 × 模型数 × 单次推理；模型池一变就要重跑 |
| **失效模式** | 新增一个模型即冷启动；线上分布漂移无法察觉 |
| **可重放性** | 好（模型固定后是确定的），但**不可解释** |

### 范式 E：在线赌博机（FreeLLMAPI）

`server/src/services/scoring.ts`

三个维度的凸组合，再乘两个护栏（`scoring.ts:8`）：

```
base      = w_rel·reliability + w_speed·speed + w_intel·intelligence
effective = base × headroomFactor × rateLimitFactor
```

四套预设权重（`scoring.ts:44`）：

```ts
balanced: { reliability: 0.5,  speed: 0.25, intelligence: 0.25 }
smartest: { reliability: 0.35, speed: 0.10, intelligence: 0.55 }
fastest:  { reliability: 0.35, speed: 0.55, intelligence: 0.10 }
reliable: { reliability: 0.70, speed: 0.15, intelligence: 0.15 }
```

注意**四套里 reliability 权重最低也有 0.35**——注释里写明了理由：不让一个"聪明但坏掉"或"快但坏掉"的模型赢。

`headroomFactor` 保护快用完免费额度的模型，`rateLimitFactor` 躲开正在限流的。还有一个高峰时段调整：把 60% 的 speed 权重挪到 reliability 上（`PEAK_SPEED_TO_RELIABILITY = 0.6`），且 `fastest` / `reliable` 两个极端预设豁免——因为调整会让它们变成别的预设。

**这里的 `intelligence` 是人工填的静态排名，不是学出来的。** 所以严格说这是"运行时健康度感知的加权选择"，赌博机成分只在探索项上。

| | |
|---|---|
| **前提** | 候选模型大致同质（都能干这活），差别只在质量/速度/可用性 |
| **代价** | 零额外调用 |
| **失效模式** | 不看请求内容。同一个权重配置对"你好"和"证明黎曼猜想"给出同样的排序 |
| **可重放性** | 取决于探索项实现 |

---

## 二、关键差异：决策信号从哪来

把五种范式按"信号来源"重排，会看到一条更清晰的分界线：

| 信号来源 | 项目 | 能回答的问题 | 不能回答的问题 |
|---|---|---|---|
| **请求内容**（最后一条消息） | LiteLLM AutoRouter、uRouter `semantic_rules` | 这是什么类型的问题 | 这次对话进行得怎么样 |
| **完整轨迹**（解析请求体） | **Switchyard** | 这次对话进行得怎么样 | 这个问题本身难不难 |
| **模型输出 + 自验证** | AutoMix | 小模型这次做对了吗 | （需要额外调用） |
| **离线全网格** | LLMRouter | 这类 query 哪个模型最好 | 线上此刻哪个部署活着 |
| **运行时健康度** | FreeLLMAPI、LiteLLM LB | 哪个部署此刻最健康 | 这个请求需要多强的模型 |

**这五行两两之间几乎不重叠。** 一个完整的路由器需要多行同时成立，而没有任何一个开源项目做到了三行以上。

其中最被低估的是第二行。Switchyard 证明了一件事：

> **OpenAI chat completions 请求体里已经带着整条轨迹**——工具调用、工具结果、轮次深度、压缩标记全都在 `messages` 里。路由器不需要客户端配合，不需要会话状态，不需要额外存储，就能拿到轨迹信号。

这条对任何想做智能体路由的网关都成立，而且实现成本极低（纯字符串扫描）。

---

## 三、几个值得单独指出的工程细节

### 3.1 LiteLLM `lowest_cost` 的成本模型是错的

`litellm/router_strategy/lowest_cost.py:295`

```python
item_cost = item_input_cost + item_output_cost
```

**把输入单价和输出单价直接相加当作标量排序键**，完全不看这次请求的输入/输出 token 比例。后果是：一个"输入便宜、输出贵"的模型和一个反过来的模型排名相同——而对 20 万 token 的长文抽取任务（输入极重、输出极轻）与对短问长答任务，正确答案是相反的。

同一函数里还有个 sentinel（`:284`）：

```python
item_input_cost = item_litellm_model_cost_map.get("input_cost_per_token", 5.0)
```

未知模型按 **$5 / token** 计价——不是真实成本，是一个"排到最后"的哨兵值。可它随后就参与了 `item_cost` 的算术。

另外整个 `{model_group}_map` 是读-改-写一个缓存键，并发下会丢更新。

### 3.2 Switchyard 允许降档，而且是硬规则

`stage.rs:386`

```rust
fn should_deescalate(signal: &ToolSignals) -> bool {
    signal.tests_passed
        && (signal.recent_write_count + signal.recent_edit_count) >= 1
        && signal.severity <= 0.0
}
```

三个条件同时成立才降：**测试通过了 + 刚写过代码 + 窗口内无报错**。这不是"看起来简单所以降档"，是"这一轮已经收尾了所以降档"——用的是**结果证据**，不是难度猜测。

规则顺序也是刻意的（`stage.rs:408` 的注释明说）：升档检查在降档之前，所以"测试通过但同时出了 critical 错误"的一轮仍然升档。

### 3.3 上下文压缩会抹掉轨迹信号

`tool_signals.rs:240` 的注释指出了一个非常隐蔽的 bug：

> 压缩会重置路由器累积的信号，所以一个已经升档的任务会**掉回弱档**。

他们的处理是把 `compacted` 做成**自锁存**的——压缩摘要会留在后续每一轮的上下文前缀里，所以一旦压缩过，`should_escalate` 就永久返回 true。

任何"从请求体推断轨迹"的设计都会撞上这个问题，因为压缩的定义就是丢历史。

### 3.4 `turn_depth` 是线格式相关的

同一处注释：

> Anthropic 把工具结果打包进更少的消息，OpenAI-chat 不会——所以以此为门槛的判据在跨请求来源时是近似的。

一个多协议网关（uRouter 正是）如果用消息数当轮次深度，在 `/v1/messages` 和 `/v1/chat/completions` 两条入口上会得到不同的路由结果。**这是协议翻译层泄漏进路由决策的一个真实例子。**

### 3.5 LLMRouter 的 "多轮" 不是多轮对话

`knnmultiroundrouter` 名字里的 multi-round 指的是**把一个 query 拆成子查询，各自路由，再聚合**（`_decompose_query` / `_execute_sub_query` / `_aggregate_responses`）——是 P3 编排，不是对话多轮。

这个模式对应的是"子任务用便宜模型、聚合用强模型"，与 uRouter 的 `CallRole::Auxiliary` 思路同源。

### 3.6 Router-R1：把模型当工具

`llmrouter/models/router_r1/` 是另一个极端——路由器本身是一个 RL 训练过的 LLM，在推理过程中决定调用哪个别的 LLM：

```
<think>为什么需要外部信息、哪个模型合适</think>
<search> LLM-Name:Your-Query </search>
<information>...</information>
```

**路由不再是网关的功能，而是生成循环的一部分。** 这条路线与"网关做路由"在架构上不可调和：它要求路由器能看到并参与推理过程。代价是路由器自己就是个 7B 模型在做多轮生成。

---

## 四、横向对比矩阵

| 维度 | LiteLLM | Switchyard | LLMRouter | FreeLLMAPI | uRouter |
|---|---|---|---|---|---|
| **主要解决** | P1 负载均衡 | P2 模型选择 | P2+P3 | P1 | P1+P2 |
| **内容信号** | 嵌入（可选） | 无 | 学习模型 | 无 | 关键词规则 |
| **轨迹信号** | 无 | ✅ **9 维** | 无 | 无 | ⚠️ **消费侧已建、生产侧为零**（`core:1303`）|
| **健康度信号** | ✅ 5 种策略 | 无 | 无 | ✅ | ✅ |
| **成本进入决策** | ⚠️ 标量近似 | 无 | 可配（默认关） | 无（全免费） | 设计中 |
| **热路径额外调用** | 1 次嵌入 | 0（或 1 次分类器） | 0 | 0 | 0 |
| **决策可重放** | ❌ | ✅ | ✅ | ⚠️ | ✅ **签名+revision** |
| **反事实评估** | ❌ | ❌ | 靠全网格规避 | ❌ | ✅ IPS/SNIPS/DR |
| **供给侧规模** | ✅ 极大 | 配置驱动 | 研究用小集合 | 34 家免费 | 44 provider |
| **降档能力** | — | ✅ 硬规则 | ✅ | — | 设计上禁止（应修正，见对标分析 G7）|

---

## 五、uRouter 站在哪里

**uRouter 与这五个项目不在同一个坐标系上。** 它们优化的是"选得准"，uRouter 的骨架优化的是"选得可审计、可重放、可回滚"——签名 manifest、revision 绑定、纯函数决策核、DecisionRecord 证据链、IPS/SNIPS/DR 反事实评估、`--dry-run` 18 项检查。**这套东西在其余五个项目里合起来也找不到一份。**

代价是对称的：**uRouter 的"智能"目前是五种范式里最弱的一种**——关键词规则匹配最后一条用户消息（`urouter-core/src/lib.rs:168` 的注释明确写着"Matching is over the latest user message"），与 LiteLLM AutoRouter 的信号面完全相同，只是把嵌入换成了关键词。

换句话说：

```
其余项目：路由决策做得好，但决策不可审计、不可重放、不可回滚
uRouter：治理骨架完整，但决策依据薄
```

这不是坏事——**治理骨架是后补不上的，决策依据是可以往上加的**。反过来做（先做准，再补治理）在 LiteLLM 上已经能看到结果：六种路由策略上线多年，至今没有一条能回答"如果换成另一个策略，成本会降多少"。

### 反例：pi

`pi` 是个编码智能体，有 20+ provider 的模型目录（`packages/ai/src/models.generated.ts`，脚本生成），但**没有任何自动模型选择**——只有手动 `/model` 选择器和 OpenRouter 的 `allow_fallbacks` 透传标志。

它的意义是提醒：**路由器的客户是这类智能体**。pi 用户手动选模型这件事本身，就是 uRouter 要消灭的场景。任何智能体路由设计都应该以"pi 这样的客户端零改动接入"为约束——这也正是 uRouter 保持 OpenAI 兼容入口的理由。

---

## 六、可以借鉴的（按价值排序）

### ★★★ 1. 从请求体解析轨迹信号（Switchyard）

**最高价值，且与 uRouter 现有架构零冲突。**

- 信号提取是纯字符串扫描 → 天然属于 `urouter-contracts` 纯函数核，零 I/O
- 不需要会话状态、不需要客户端配合 → 不破坏无状态网关
- 输出是有界整数/浮点维度 → 可以直接进 `FeatureFrame`，被 `urouter-artifact` 的整数推理消费
- 与 [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) Layer 2 ② 的"轨迹信号"是同一件事，Switchyard 给出了可直接参考的实现

**具体可搬的四样**：错误严重度模式表（含分级）、`spinning`/`exploring` 互斥划分、窗口化最大严重度（让错误在恢复轮里持续存在，而不是下一轮清零）、`compacted` 自锁存。

> **补充核对（写完本节后发现）**：`crates/urouter-core/src/lib.rs:1303` 已经在消费这**四个同名维度**，只是它们必须由调用方在 `urouter.signals[]` 里声明——网关不做提取，所以今天恒为空。而且 `production_intensity` 被并入了升档条件，符号与 Switchyard 相反。详见 [`uRouter_对标开源路由器技术差距分析.md`](uRouter_对标开源路由器技术差距分析.md) §2.1–§2.2。

⚠️ **必须处理 3.4 的坑**：uRouter 是多协议入口，`turn_depth` 不能用消息数。应当数**工具结果的个数**而非消息数。

### ★★★ 2. AutoMix 的 `HOPELESS` 状态

**这条纠正的是一个我在 Auto 方案里没有写对的假设。**

现有设计是"轨迹出问题 → 抬高下限"，隐含假设升档有收益。AutoMix 明确区分：

```
NEEDY    小模型错、大模型对   → 升档有价值
HOPELESS 大模型也错          → 升档纯浪费
```

如果 L2 配对实验显示某类任务在所有档位上成功率都很低，那类任务的正确动作是**降档**（反正都做不成，别花贵的钱），而不是升档。这应当写进 `quality_floors` 的配置语义里。

**这一条不需要实现 AutoMix，只需要在 L2 报告里多看一列**：不是只看"最便宜的达标档位"，还要看"最高档是否达标"。最高档都不达标的类别应当单独处理。

### ★★ 3. 置信度 + fall-open 的两级结构（Switchyard）

打分器给出 `score` 和 `confidence = |score|`；置信度不足时不硬猜，而是**交给下一级**（LLM 分类器）或落到默认档。

对 uRouter 的意义：[`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) 里的 `label_confidence_millis` 应当有一个明确的**弃权路径**——低置信度时不是选一个次优标签，而是回落到 `unknown`，让 `by_task` 不参与 `max()`。这与"标签体系必须有 `unknown` 且是默认值"的硬约束是同一件事的两面。

### ★★ 4. FreeLLMAPI 的 reliability 权重下限

四套预设里 reliability 最低 0.35，理由是不让"聪明但坏掉"的模型赢。

uRouter 的对应物是 `DeploymentPicker`，目前 `LowestLatency` / `LowestQuotaUsage` 是**单目标**的——没有任何机制阻止一个健康度很差但延迟低的部署赢。（这正是我此前修掉的"8ms 返回 401 的部署看起来最快"那个缺陷的一般形式：修了延迟采样门控，但单目标排序本身还在。）

### ★ 5. 挖掘真实轨迹来标定模式（Switchyard 的 1006 条轨迹 / 22 TP / 2 FP）

方法论层面的借鉴：任何基于文本模式的判据都应当在真实轨迹上量化真假阳性，并把数字写进注释。uRouter 的 `semantic_rules` 目前没有任何这类标定。

---

## 七、明确不该借鉴的

| 不做 | 理由 |
|---|---|
| **LiteLLM 的标量成本模型**（输入价+输出价） | 忽略请求形状。uRouter 已有 token 估算，应当算 `in×p_in + out×p_out`；`uRouter_技术架构评审方案.md` P0-1 正在处理估算一致性，成本模型应当在那之后接上 |
| **热路径嵌入调用**（AutoRouter） | 每请求一次网络 I/O，且引入外部服务依赖，破坏决策可重放性。uRouter 的整数推理 artifact 是更好的载体 |
| **LLMRouter 的全网格训练数据** | N 倍离线推理成本，模型池一变就重跑。uRouter 的 IPS/SNIPS/DR 路线是为"只能观测到被选臂"的生产日志设计的，这是正确的取舍——但要守住 ESS 门禁 |
| **Router-R1 的模型即工具** | 要求路由器参与推理过程，与网关架构不可调和 |
| **AutoMix 的每请求自验证** | 延迟至少翻倍、调用数至少翻倍。只取它的 `HOPELESS` 洞见，不取它的机制 |
| **LiteLLM 的读-改-写缓存键** | 并发丢更新。uRouter 已经用 Lua 脚本做原子扣费，别退回去 |

---

## 八、一句话总结

> **五个项目各自解决了"智能路由"的一个切面，没有一个解决了两个以上；uRouter 解决的是第六个切面——让路由决策可审计、可重放、可回滚——而这个切面其余五个都没碰。**

最该补的一课来自 Switchyard：**轨迹信号就在请求体里，白拿的，零成本，而且是唯一能回答"这次对话进行得怎么样"的信号源**。最该记住的一条来自 AutoMix：**难 ≠ 升档有用**。

---

## 相关文档

- [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) — Layer 2 轨迹信号的落点
- [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) — 内容信号的载体与置信度弃权
- [`uRouter_智能路由评测方案.md`](uRouter_智能路由评测方案.md) — L2 配对实验，`HOPELESS` 类别的识别处
- [`uRouter_技术架构评审方案.md`](uRouter_技术架构评审方案.md) — P0-1 token 估算，成本模型的前置
- [`FreeLLMAPI_深度技术解读与uRouter架构对比报告.md`](FreeLLMAPI_深度技术解读与uRouter架构对比报告.md) — FreeLLMAPI 单项目详解
- [`LiteLLM_Router_深度技术解读报告.md`](LiteLLM_Router_深度技术解读报告.md) — LiteLLM 单项目详解
- [`LLMRouter_深度技术解读报告.md`](LLMRouter_深度技术解读报告.md) — LLMRouter 单项目详解
- [`Switchyard_深度技术解读报告.md`](Switchyard_深度技术解读报告.md) — Switchyard 单项目详解
