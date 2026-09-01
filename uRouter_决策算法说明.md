# uRouter 决策算法说明

> 更新日期：2026-09-01
> 依据：`crates/urouter-core`、`crates/urouter-contracts`、`crates/urouter-ai`、`crates/urouter-artifact` 的当前实现逐行核对。
> 引用位置只写文件与函数名，不写行号 —— 行号会漂移。

uRouter 的"决策"不是单一模型，而是**七层相互独立的决策器**。每层职责单一、可单独替换、可单独测试。本文按请求流经的顺序描述。

## 目录

1. [能力准入](#一能力准入硬过滤)
2. [语义分类](#二语义分类规则优先--显式弃权)
3. [Tier 级联](#三tier-级联)
4. [部署选择](#四部署选择)
5. [失败处理](#五失败处理三个独立决策器)
6. [learned policy](#六learned-policy可选默认关闭)
7. [探索](#七探索默认关闭)
8. [离线评估](#离线评估不在请求路径上)
9. [贯穿性设计](#贯穿性设计)

---

## 一、能力准入（硬过滤）

`urouter-ai::admission::eligible_models`

不是打分，是**布尔排除**。逐个遍历 Catalog 中的模型，任一条件不满足即排除并记录原因：

- 生命周期不是 `Active`
- 缺少请求所需能力：tools / 图片 / 结构化输出 / reasoning
- 上下文窗口不足
- 请求要求的 Host 工具未被声明

产出 `AdmissionResult { eligible, excluded }`，`excluded` 逐条带原因。这是 `/v1/explain` 里能看到"哪个模型为什么被排除"的来源。

**这一层在所有其他决策之前，且显式指定模型也必须通过。** 指定了 Catalog 里存在但不满足能力要求的模型，返回 `NoEligibleTier` 而不是放行。

## 二、语义分类（规则优先 + 显式弃权）

`urouter-core::semantic_requirement`

不是分类模型，是**窄模式关键词匹配 + 置信度**。只有三类高置信度模式会改变路由：

| 任务 | 触发条件 | 置信度 | 效果 |
|---|---|---:|---|
| `Greeting` | 精确问候语 | 980 | 保持 efficient |
| `RealtimeWeather` | `天气` / `气温` / `weather` / `forecast` | 950 | 强制要求 Host 声明 `get_weather` |
| `EquationSolving` | `二元一次方程` / `方程组` / `solve the equation` / `solve equation`，或同时出现 `=` `x` `y` | 930 | 要求 reasoning 能力 |

其余一律 `General`，置信度 0，**显式 abstain**，回退到既有策略。

### 规则外置（`route.json` 的 `semantic_rules`）

内置规则覆盖面窄且无法泛化，因此规则可以外置成 Route 的一部分发布：

```json
{
  "id": "urouter/auto",
  "tiers": [ ... ],
  "semantic_rules": {
    "schema_version": 1,
    "mode": "extend",
    "rules": [
      {"id": "sql_generation", "task": "equation_solving", "confidence_millis": 900,
       "contains_any": ["写一段 sql", "生成建表语句", "write some sql"],
       "requires_reasoning": true},
      {"id": "db_migration", "task": "equation_solving", "confidence_millis": 880,
       "contains_all": ["数据库", "迁移"], "requires_reasoning": true}
    ]
  }
}
```

放在 Route 文件里是刻意的：签名 control manifest 已经对它做哈希，规则因此**白拿** revision 绑定、热加载、`last_good` / `fail_closed` 和一键回滚，不需要第二条需要保持一致的分发通道。

| 字段 | 说明 |
|---|---|
| `mode` | `extend`（默认）配置规则先试、内置规则兜底；`replace` 需显式选择，会关掉全部内置规则 |
| `equals` | 归一化后精确匹配 |
| `contains_any` | 任一子串命中 |
| `contains_all` | 全部子串都出现 |
| `requires_reasoning` / `requires_tools` | 并入 `CapabilityRequirement`，直接影响能力准入 |
| `required_tool` | Host 未声明该工具则在调上游前拒绝 |
| `confidence_millis` | 1..=1000，0 是 abstain 专用，规则不能声明 |

**匹配语义**：配置规则按声明顺序求值，**首个命中者胜** —— 发布的规则集有可从文件读出的确定优先级。配置规则先于内置规则，因此可以覆盖内置分类。

**归一化共用**：配置规则和内置规则走同一个 `normalized_user_text`（小写 + 去首尾标点）。否则同一句话在配置里和代码里可能分类不同。

**`extend` 保住安全断言**：天气那条内置规则是"网关绝不伪造工具结果"的实现。默认 `extend` 意味着一份忘了写天气规则的配置，无法静默移除这个保证。选 `replace` 才会关掉它，是显式决定。

**校验在 `RouteConfig::validate` 内**，控制面在加载和每次热加载时都会调用。因此坏规则集会让候选 Route 失败，由既有的 `last_good` / `fail_closed` 策略处置 —— 没有第二条需要维护的拒绝路径。拒绝的情况：schema 版本不符、id 为空或重复、无任何匹配条件（发布了却永不命中，几乎总是编辑错误）、空 pattern（会捕获全部流量）、置信度越界、`required_tool` 为空。

**当前限制**：`task` 只能取内置的四个枚举值之一。新增业务意图（如上例的 SQL）目前借用一个既有 task 值，路由效果正确，但在指标和 artifact 支持域里会显示为借用的那个名字。规则的 `id` 会记在 `SemanticClassification.rule_id` 上用于归因。开放自定义 task 名需要改动一个已序列化的公共枚举，留作独立变更。

设计取舍是**宁可不判，也不猜错**。因此 `解方程 x^2-5x+6=0`（一元式，含 `=` 和 `x` 但无 `y`，也不含关键词）**不触发** `EquationSolving`，落回 `General`。这是刻意的：扩大匹配面会带来误判，而误判的代价高于漏判。

天气这一条尤其保守：网关**绝不伪造工具结果**。缺少 Host 声明的 `get_weather` 时，在调上游之前返回 400 `missing_required_tool`；后续带 tool-result 的消息继续走 capable 模型。

## 三、Tier 级联

`urouter-contracts::select_tier_with_cascade`（纯函数）+ `urouter-core::select_tier`（输入组装）

### 级联规则

三条规则**按序短路**，每条要么 `Selected` 要么 `Abstain`，全程写入 `cascade_trace`：

```
PinRule      → contract.preference.pin_tier 命中 → reason = preference_pin
QualityRule  → high_quality && !auxiliary        → 取最高 tier，reason = quality_guard
DefaultRule  → 兜底                              → 取最低 tier，reason = default_<tier>
```

`preference.floor_tier` 先截断候选集，但 **`PinRule` 不受 floor 约束**，它对全量候选求值 —— pin 是显式意图，不应被 floor 静默改写。

### 输入信号的计算

```
high_quality = difficulty == "hard"
             || workload == "plan"
             || preference_bias_millis < -500
             || signal_score(["severity","spinning","exploring","production_intensity"]) > 500
             || 长上下文超过 route.long_context_quality_threshold_tokens

low_cost     = call.role == auxiliary
             || value_class ∈ {auxiliary, disposable}
             || preference_bias_millis > 500
             || signal_score(["cost_sensitive","disposable"]) > 500
```

`signal_score` 取匹配类别中**强度最大**的一个（不是求和），经 `quantize_bias` 量化：`clamp(-1.0, 1.0) × 1000` 四舍五入为整数。偏好偏置同样量化，缺省用 `route.default_preference_bias_millis`。

量化的意义是把浮点偏好变成整数比较，使决策可重放。

### 优先级高于级联的两条短路

| 情况 | reason | 说明 |
|---|---|---|
| 显式指定模型 | `explicit_model` | 跳过分级，tier 记为 `pinned`，但仍过能力准入 |
| 会话/任务绑定命中 | `task_binding` | `bind_decision` 直接改写 tier 和 model |

### 一处 reason 改写

级联结束后，若候选集被能力过滤缩减过（`eligible.len() < tiers.len()`）**且**选中的正是首个候选，reason 改写为 `capability_required`，并追加一条 `capability_filter` 规则记录。这让"因为能力受限才选了它"和"因为默认策略选了它"在证据上可区分。

## 四、部署选择

`urouter-contracts::plan_capacity_lease_with_picker` + `select_weighted_deployment`（纯函数）

两段式：**先按 order 分层，再在同层内用 picker，最后加权票选**。

### 1. 排除不可用

- `retry_excluded`（本次重试已排除）
- 本地熔断状态为 `Open` 或 `HalfOpenProbeInFlight` → `local_circuit_unavailable`

全部不可用则返回 `Exhausted`，并带上每个候选的排除原因。

### 2. 取最小 order 层

`order` 是**硬优先级**，跨层不比较。只有最小 order 的候选参与后续选择，其余标记 `lower_priority_order`。

### 3. Picker 在同层内降权

| Picker | 指标 | 降权原因码 |
|---|---|---|
| `Weighted`（默认） | 无，纯权重 | — |
| `LeastLoaded` | `in_flight` | `higher_load` |
| `LowestLatency` | `latency_ewma_ms` | `higher_latency` |
| `LowestQuotaUsage` | `quota_usage_millis` | `higher_quota_usage` |

取同层最优指标值，把**严格劣于**它的候选标为不可用并记原因。指标缺失的候选不参与比较，不被降权。

### 4. 加权票选

```
position = ticket % total_weight
按 ID 排序后线性扫描，逐个扣减 weight，position 落入哪个区间就选哪个
```

**确定性**：同一 ticket 必得同一结果。这是决策可重放、`urouter-embed` 与网关能保持 parity 的前提。权重总和为 0 时返回 `ZeroWeight` 而不是随意选一个。

产出 `DeploymentEvaluation` 列表，每个候选标注 `Selected` / `RunnerUp` / `Excluded` 及原因，进入 DecisionRecord。

### 叠加：cache affinity

`urouter-gateway::filter_deployments` + `record_cache_affinity_success`

维护一个有界的 `prompt_profile_hash → 上次成功部署` 映射。命中时把该部署 `order` 置 0、其余 `+1`，从而让它进入最小 order 层。

三个约束：
- 只在**已通过准入**的候选内重排，不会因为亲和把不合格的部署拉回来
- 状态缺失时 **fail-open**，退化为普通选择
- 命中时计数 `urouter_cache_affinity_hit_total`，并在 trace 里标 `cache_affinity_hit`

## 五、失败处理（三个独立决策器）

### 5.1 重试 `RetryPolicy::directive`

三态输出：

```
不可重试 || 次数耗尽 || 无候选   → Stop
候选数 == 1                      → RetrySameDeployment { backoff_ms }
候选数 > 1                       → ReselectDeployment
```

退避 `backoff_ms(retries_used, retry_after_ms)`：

```
retry_after_ms 存在  → 直接采用（尊重上游的显式指示）
否则                 → min(base × 2^retries_used, max_backoff)
```

可重试的错误类型：`Transport`、`Timeout`、`RateLimited`、`ServerError`、`ProviderUnavailable`。
不可重试：`Unauthorized`、`NotFound`、`BadRequest` —— 确定性 4xx 重试只是浪费配额。

### 5.2 熔断 `cooldown_directive`

按**失败率**而非失败计数：

```
failures       = window.failures + 1
failure_millis = failures × 1000 / (successes + failures)

tier_size > 1 && (RateLimited || failure_millis >= failure_threshold_millis)
    → OpenCircuit
否则 → RecordFailure
```

`tier_size > 1` 这个前置条件是关键：**单部署永不冷却**。只有一个部署时把它熔断等于自杀 —— 没有任何地方可以转移流量。

`RateLimited` 单独短路：限流是明确的容量信号，不必等失败率累积。

三个作用域独立计数，互不干扰：`deployment` / `credential` / `provider`。Half-Open 阶段全局只允许一个探针（多实例下由 Redis 原子保证）。

### 5.3 Fallback 链 `plan_fallback_tiers`

DFS 展开 tier 图，三重保护：

- `emitted` 集合去重，同一 tier 不重复进入计划
- `active` 集合做**环检测**，`A → B → A` 直接报错而不是无限展开
- `max_depth` 截断深度

可按 `FallbackCause` 配置不同链路。`FallbackCause` 共 12 个变体，其中 8 个由 `UpstreamErrorKind` 直接映射：

| 来源 | 变体 |
|---|---|
| 由 `UpstreamErrorKind` 映射 | `Transport`、`Timeout`、`RateLimited`、`ServerError`、`ProviderUnavailable`、`Unauthorized`、`NotFound`、`BadRequest` |
| 仅由路由侧产生 | `ContextWindow`、`ContentPolicy`、`Quota`、`Capacity` |

即"超时降级到便宜 tier"和"限流转移到另一家 provider"可以是两条不同的链。

执行前会按**最坏链路成本**做预算预留 —— 不能出现"预算够走第一跳，但走完 fallback 就超支"。

## 六、learned policy（可选，默认关闭）

`urouter-artifact::RouterArtifact::infer` + `ArtifactController::decide`

**全整数运算，无浮点** —— 保证跨平台、跨版本可重放。这是它能被签名并作为发布物的前提。

### 三种模型

| 类型 | 算法 |
|---|---|
| `Mlp` | 单隐层 ReLU：`score = output_bias + Σ max(0, W·x + b) × w_out`，`score >= threshold` 则升级 |
| `Knn` | 平方距离排序取 k 近邻，**多数票**：`promoted × 2 >= k` 则升级 |
| `Contrastive` | 比较到两个质心的平方距离，`promoted <= baseline` 则升级 |

### 三道前置守卫

任一不过就回退规则决策，并在证据里记录 `fallback_reason`：

1. **kill switch** → `reason = kill_switch`。操作员的即时停止开关，无条件生效。
2. **操作预算**：`operation_limit < required_operations` → `OperationBudgetExceeded`

   required 由模型规模推导：MLP 是 `hidden × (dims + 2)`，KNN 是 `prototypes × (dims + 1)`，Contrastive 是 `dims × 2`；无 learned model 时为 6。**有界推理** —— 防止推理本身成为延迟来源。
3. **支持域**：语义任务不在 `support.semantic_tasks` 内、输入超 `maximum_input_text_bytes`、或有工具但 `tools_supported == false` → `OutOfSupportDomain`

另有一道运行时守卫在网关侧：artifact 绑定的 Catalog/Route revision 与当前控制面不一致时，直接回退规则决策，`fallback_reason = control_revision_changed`。

### Canary 分桶

```
bucket = SHA256(tenant_key + "\n" + task_key) 前 2 字节 % 10000
observations >= minimum_samples && bucket < canary_basis_points → 用 candidate
```

**稳定哈希**：同一任务永远落同一桶，不会在会话中途从 active 跳到 candidate。`minimum_samples` 门槛避免样本不足时就开始分流。

shadow 模式下 candidate 只推理不生效，结果记入 `shadow_tier` 供离线比对。

## 七、探索（默认关闭）

`urouter-gateway::apply_controlled_exploration`

ε-greedy，但**不使用随机数**：

```
digest = SHA256(tenant_key + "\0" + task_key + "\0exploration-v1")
draw   = digest[0..4] % 1_000_000
探索   = draw < epsilon_millionths
选中   = eligible[digest[4..8] % eligible.len()]
```

同一任务的探索决定是确定的、可重放的 —— 这对反事实评估至关重要：propensity 必须可复算。

需要**同时**满足五个条件才会启用：

1. 请求级 recording 已开启（`recording != none`）
2. 训练授权 `allow_training`
3. 显式探索授权
4. 请求声明的探索预算不超过服务端配置的上界
5. 至少 2 个可选 tier，且基线 tier 在其中

任何一条不满足，直接返回 `None`，走基线决策。

## 离线评估（不在请求路径上）

`urouter-eval::counterfactual_report`

反事实估计量：**IPS**、**SNIPS**、**Doubly-Robust**，各自带方差、95% 置信区间、**ESS**（有效样本量）。

用于评估策略变更是否值得发布，不参与任何在线决策。支持域门禁（support domain gate）会阻止在样本支持不足的区域做 canary。

---

## 贯穿性设计

### 纯函数 + 快照注入

级联、picker、retry、cooldown、fallback **全部是零 I/O 纯函数**，位于 `urouter-contracts` 和 `urouter-core`。运行时只负责两件事：注入 `CapacitySnapshot`、管理 lease。

三个直接后果：

- 网关（`urouter-gateway`）和进程内嵌入（`urouter-embed`）共用同一决策核，对相同输入产出相同决策 —— 这个 parity 有测试保证
- 决策可被穷举测试，不需要起网络或 Redis
- 决策可重放：给定相同的 Catalog / Route / artifact / 上下文 / ticket，结果确定

### 处处显式弃权 + 留痕

每层要么给出决策、要么显式 `Abstain`，没有隐式默认。`cascade_trace` 记录每条规则的 `outcome` 和 `reason`；部署选择记录每个候选的 `disposition` 和排除原因。

DecisionRecord v2 存的是**完整证据链**而不是结论。事后能回答"为什么走了这个模型"，而不只是"走了哪个模型"。

### 确定性优先于最优性

多处刻意选择确定性算法而非看起来更优的方案：加权票选用 `ticket % total_weight` 而非随机、canary 用稳定哈希分桶而非随机采样、探索用哈希而非 RNG、learned policy 用整数而非浮点。

代价是失去一些理论最优性，换来的是可重放、可归因、可离线评估 —— 对一个需要事后解释每次路由决策的系统，这个交换是值得的。

---

## 相关文档

- [`README.md`](README.md) — 接入方式与架构总览
- [`uRouter_技术架构流程图.md`](uRouter_技术架构流程图.md) — 完整请求流程图
- [`docs/gateway-m0.md`](docs/gateway-m0.md) — 网关契约与选择规则
- [`docs/requirements-traceability.md`](docs/requirements-traceability.md) — 权威执行状态
- [`docs/adr/`](docs/adr) — 架构决策记录，含事实/策略分离、定点计价等
