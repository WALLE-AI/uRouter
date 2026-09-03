# FreeLLMAPI 深度技术解读与 uRouter 架构对比报告

> 分析日期：2026-09-03
> 分析对象：
> - `opensource/freellmapi`（v0.9.4，680 commits，最新提交 2026-09-02）
> - 本仓库 `uRouter`（M0–M4 code_complete）
> 依据：逐文件核对源码，不依赖 README 宣称。引用只写文件与函数名，不写行号。

---

## 目录

- [零、结论先行](#零结论先行)
- [一、freellmapi 深度解析](#一freellmapi-深度解析)
- [二、uRouter 深度解析](#二urouter-深度解析)
- [三、逐维度对比](#三逐维度对比)
- [四、三个更深的架构分歧](#四三个更深的架构分歧)
- [五、可互相借鉴的点](#五可互相借鉴的点)
- [六、总结与建议](#六总结与建议)

---

## 零、结论先行

**这两个项目名字都叫 "router"，但解决的根本不是同一个问题。**

| | freellmapi | uRouter |
|---|---|---|
| 定位 | **供给侧聚合器** | **决策侧路由器** |
| 核心矛盾 | 配额稀缺 | 决策归因 |
| 路由目标 | 找到一个**还能用**的模型 | 找到**该用**的模型并解释为什么 |

二者的架构分歧几乎全部可以从这一句推导出来。

---

## 一、freellmapi 深度解析

### 1.1 工程规模与技术栈

| 维度 | 数据 |
|---|---|
| 代码量 | TS/TSX 约 **129.6k 行**（server 50.7k） |
| 测试 | **275 个 test 文件**（server 242），vitest |
| 提交数 | 680 commits，最新 2026-09-02（desktop v0.9.4） |
| 形态 | npm workspaces monorepo：`server` / `client` / `cli` / `shared` / `desktop` |
| 运行时 | Node ≥20，Express 5，better-sqlite3，undici，zod，sharp |
| 交付面 | Docker、Electron 桌面端（Mac/Win/Linux）、Google Play、npx CLI、MCP server |
| 许可 | MIT |

值得注意的是**依赖极其克制**：没有 LangChain、没有 ORM、没有 Redis、没有队列。SQLite + 进程内内存计数器 + `better-sqlite3` 同步 API 撑起全部状态。这是"单用户本地部署"定位下的正确选择 —— `docs/architecture.md` 里明说：

> No multi-tenant auth. Run this for yourself; don't expose it to the internet.

### 1.2 分层架构

```
OpenAI SDK / Claude Code / Codex / 15+ agent
        │  Bearer freellmapi-…
        ▼
Express app.ts  ── CSP/helmet/CORS/rate-limit/requireAuth
        │  多协议入口：/v1/chat/completions · /v1/responses
        │            /v1/messages(Anthropic) · /v1beta(Gemini) · /api/ollama
        ▼
compression pipeline（可选，8 个引擎）
        ▼
router.ts (2194行)  ←→ scoring.ts (476行, bandit)
        │            ←→ ratelimit.ts (1435行, 配额账本)
        ▼
fallback-loop.ts (1249行)  ── 最多 20 次尝试 / 45s 墙钟预算
        ▼
providers/*.ts  ── 11 个适配器类，覆盖 34 个平台
        ▼
SQLite（AES-256-GCM 信封加密存 key）+ catalog-sync（Ed25519 签名目录）
```

### 1.3 核心算法：Thompson Sampling Bandit

`server/src/services/scoring.ts` 是整个项目最有价值的部分，设计相当干净：

```
base      = w_rel·reliability + w_speed·speed + w_intel·intelligence   （凸组合，∈[0,1]）
effective = base × headroomFactor × rateLimitFactor                     （两个护栏乘子）
```

预设权重向量：

| 策略 | reliability | speed | intelligence |
|---|---:|---:|---:|
| `balanced`（默认） | 0.50 | 0.25 | 0.25 |
| `smartest` | 0.35 | 0.10 | 0.55 |
| `fastest` | 0.35 | 0.55 | 0.10 |
| `reliable` | 0.70 | 0.15 | 0.15 |
| `custom` | 用户持久化的权重向量 | | |
| `priority` | 关闭 bandit，走手工链 | | |

四点做得好：

1. **量纲统一**。注释里明确说这是对老版本"一堆量纲不兼容的加分项手工封顶"的重写。三个轴全部归一化到 [0,1]，权重和为 1，所以 base 永远不会跑飞，任何一项都不需要人工 cap。

2. **可靠性用 Beta 后验 + Thompson 采样**（`sampleBeta`，Marsaglia & Tsang 两次 Gamma 抽样实现）。Beta(1,1) 均匀先验意味着"没见过的模型是真不确定，不是假定好或坏"，探索强度自动正比于不确定性 —— 一个模型不会因为连挂两次就被永久冻结。社区先验（community prior）折进 Beta 起始余额，本地样本自动稀释它。

3. **护栏是乘子而不是加项**。`headroomFactor` 在配额剩余 20% 时开始线性衰减到 0.1 地板；`rateLimitFactor` 把 429 惩罚映射成最低 0.4 的乘子（`RATE_LIMIT_MAX_DAMP = 0.6`）。乘子的语义是"不重排好模型之间的顺序，只在危险时把一个模型拉下来"，这个区分很精准。`rateWindowHeadroomFactor`（#899）把同一条 ramp 复用到 rpm/rpd/tpm/tpd 的实时利用率上，解决了窗口限额原本是**二值**的问题 —— 老行为是一直把模型钉在 #1 直到耗尽那一刻吃 429。

4. **速度轴的细节**：
   - 吞吐用饱和曲线 `1 - exp(-tok_s / 60)`，避免一个极快小模型把正常模型压成"慢"（注释直指 fork 里的 global-max-normalization bug）
   - TTFB 用 300ms → 5000ms 线性斜坡
   - 6:4 混合，无样本时返回乐观先验 0.6 保证探索
   - **#619 的 fix 尤其对**：超时不再只扣可靠性，而是以「零输出 token + 该延迟作为 TTFB」记入速度窗口，并 cap 在 `TIMEOUT_LATENCY_CAP_MS = 120s`，防止一次挂死支配 7 天窗口

其他值得记录的取舍：

- `intelligenceComposite = tierValue×1000 - sqrt(rank)×31`。sqrt 压缩让 rank 编辑可见（#673：线性 rank 被 tier×1000 淹没，用户改了等于没改），同时保证 tier 严格支配（最差 rank 的 `sqrt(1000)×31 ≈ 980 < 1000`）。
- 未识别的 size_label 是这条轴的**地板**而非排除项 —— 因为 intelligence 只是凸组合里的一项，可靠性和速度强的无 tier 模型仍能赢。自定义模型 seed 在目录中位 tier，理由是"unknown 是没有意见，不是最差"。
- 峰值时段动态权重（#760）默认关闭，且 `fastest`/`reliable` 两个极端预设**豁免**。理由写得很细：移 60% 速度权重会让 `fastest` 变成 reliability 0.68 / speed 0.22，即悄悄变成 `reliable` 的噪声副本 —— 宁可豁免也不 clamp，保住每个预设的身份。
- 峰值小时用 `Intl.DateTimeFormat` 在显式 IANA 时区里解读，不用 `Date#getHours()` —— 容器/VPS 镜像通常是 UTC，而流量不是。
- `startHour === endHour` 是**空窗口**而非 24 小时窗口：拖到同一个值的操作员意思是"什么都不做"，反过来会变成一个看起来是 no-op 的配置导致永久静默重加权。

### 1.4 配额账本与冷却阶梯

`server/src/services/ratelimit.ts`（1435 行）是第二核心。免费额度聚合的真正难点在这里：

- **四维窗口**：RPM / RPD / TPM / TPD，模型级 + provider 级双层（`canUseProvider` / `canUseProviderMinute` / `canUseProviderTokens`）
- **并发 lease**：`acquireLease` / `releaseLease`，`LEASE_MAX_AGE_MS = 2min` 自动老化。`RouteResult.release?.()` 放在 `finally` 里，且**故意声明为 optional** —— 注释解释：`finally` 里抛 TypeError 会**替换掉**正在传播的上游异常，把可诊断的 429 变成神秘 500。这种级别的错误路径洁癖在开源项目里少见。
- **冷却阶梯** `COOLDOWN_DURATIONS`，按 `(platform, model, key)` 三元组递增；区分 `TRANSIENT_COOLDOWN_MS = 90s` 与 `UNKNOWN_LIMIT_MAX_COOLDOWN_MS = 10min`；`NULL_LIMIT_HIT_THRESHOLD = 2` / `NULL_LIMIT_HIT_WINDOW_MS = 1h` 用于推断未声明的限额。
- **上游 back-off 提取**（`providers/base.ts`）：
  - `parseRetryAfterMs`：`Retry-After` 头，支持 delta-seconds 与 HTTP-date
  - `parseStatedRetryMs`：DFS（深度限 6）遍历错误体找 `retryDelay` / `retry_after` / `retryAfterSeconds`，支持 `google.rpc.RetryInfo` 的 protobuf duration `"17s"`
  - 散文兜底正则：`/(?:try again|retry)\s+(?:in|after)\s+(\d+...)\s*(ms|s|m|h...)/i`，锚定在显式 retry 短语上，防止无关数字被误当退避
  - 全部 clamp 到 `MAX_RETRY_AFTER_MS = 24h`，防恶意/畸形 Retry-After 把 key 永久 bench
  - 注释里的原则："a stated field is a promise, a sentence is an observation" —— 结构化字段优先于散文

- **`OUTPUT_RESERVE_CAP = 2000`（#470）**：客户端传 `max_tokens: 32000` 但根本用不完，按全量预留会把 TPM 6k–30k 的整个免费池排除掉，返回**零上游调用**的假 429。所以路由期只预留最多 2000 输出 token，输入照实算。理由：provider 计的是实际 token，欠预留只会撞上游 429/413（重试循环已处理），过预留则直接饿死路由。

  **这是典型的"只有真正跑过生产才会发现的 bug"。**

### 1.5 失败处理

`server/src/lib/fallback-loop.ts`（1249 行）：

| 参数 | 值 |
|---|---|
| `FALLBACK_MAX_RETRIES` | 20 |
| `DEFAULT_FALLBACK_TIME_BUDGET_MS` | 45s（可配置） |
| `MODEL_FAILURE_WINDOW_MS` / `THRESHOLD` / `COOLDOWN_MS` | 15min 内 3 次 → bench 10min |
| `AUTH_FAILURE_COOLDOWN_MS` | 5min，带 30s 去重的 key 重验触发 |
| `EMPTY_COMPLETION_STREAK_LIMIT` | 3 |

`msUntilNextUtcMidnight` 用于日限额到期估算；响应头暴露 attempt trail（`TRAIL_MAX_SHOWN = 10`，头长度上限 1024/2048）。

`router.ts` 的 `summarizeExhaustion` 值得单独提：把每个候选被排除的原因归到 9 个桶里，按可操作性排序返回：

```
All models exhausted: 47 routes checked
  (31 rate-limited or on cooldown, 12 no usable key configured,
   4 prompt too large for the model).
  Add more API keys or wait for rate limits to reset. Soonest reset ~23m.
```

解决的是 #423 —— "不透明的 routing_error 429"。

### 1.6 其他有分量的子系统

**压缩管线**（`services/compression/`，8 个引擎 + fidelity gate）

| 模式 | 启用引擎 |
|---|---|
| `off` | — |
| `lossless` | dedup, lite, jsoncompact |
| `standard` | + read-lifecycle, toolfilter |
| `aggressive` | + relevance, aging, hard-budget |

system prompt 按 SHA-256 做稳定前缀哈希并冻结不压。这是免费额度下的刚需 —— TPM 是硬约束。

**fusion**（`services/fusion.ts`，791 行）：虚拟模型 id `fusion`，默认 `DEFAULT_PANEL_K = 4` 路 panel + judge 合成。注释直言默认 K 高于 OpenRouter 的 3 —— "跑在免费额度上就是能负担更宽更多样的 panel"。

**catalog-sync**（`services/catalog-sync.ts`）：
- Ed25519 **签名**的模型目录，公钥硬编码在代码里，验签失败即丢弃 —— 被攻破的 CDN 或 MITM 无法注入模型/quirks
- 免费装月度快照，Premium 装实时（$19/yr）
- `MIN_CATALOG_VERSION` 保证下发目录只能比内置迁移新，防回滚
- `hasProvider` gate：目录里的平台没有注册适配器就不落库

**tool-call rescue**（`lib/tool-call-rescue.ts`）：把把 tool call 当纯文本吐出来的模型救回结构化 `tool_calls`；带 tools 的请求只路由到真支持的模型。

**数据库**：37 个迁移文件，带 `up/down/fresh/status/create` CLI 和 roundtrip 测试。key 用 AES-256-GCM 信封加密（`encrypted_key` / `iv` / `auth_tag` 三列），per-key proxy 覆盖同样加密（#590）。

**协议面**：OpenAI Chat Completions / Responses / Anthropic Messages / Gemini `/v1beta` / Ollama 模拟 / MCP server / embeddings / media（图像·音频·视频）。

### 1.7 弱点

1. **适配器广度是"注册"而非"适配"**。11 个 provider 类支撑 34 个平台，绝大多数是 `new OpenAICompatProvider({platform, baseUrl})` 一行注册。这其实是好设计，但意味着"34 家"的差异化处理很薄 —— 真正的 quirks 靠 `services/quirks.ts` + 目录下发补。

2. **单进程单用户是硬天花板**。速率账本是进程内 Map + SQLite，横向扩展需要重写；文档自己承认没有多租户。

3. **决策不可重放**。`sampleBeta` 用 `Math.random()`，`roundRobinIndex` 是可变全局 Map，`rateLimitPenalties` 是进程内衰减状态。同样的请求两次可能走不同模型，事后无法复现"为什么"。对聚合免费额度这个场景无所谓，对需要审计的场景是致命的。

4. **无会话连续性**。bandit 每次请求独立采样，第 5 轮完全可能从 Gemini 跳到 Groq 上的另一个模型 —— system prompt 遵从度、tool call 格式、思维链风格全变。

5. **合规是灰区**。README 自带 ToS 审查表：Cohere ❌ Avoid，Google / NVIDIA / GitHub Models / Z.ai / Cloudflare ⚠️ Caution。项目性质决定了它只能是"个人自托管"。

---

## 二、uRouter 深度解析

### 2.1 工程规模与技术栈

| 维度 | 数据 |
|---|---|
| 代码量 | Rust **35.7k 行**（gateway 18.4k，其中 tests.rs 4.5k） |
| 测试 | 319 通过 / 0 失败，覆盖率 77.9% |
| 形态 | Cargo workspace，**12 个 crate + 5 个工具** |
| 运行时 | Rust 2024 edition，axum 0.8，tokio，redis 0.32，rustls |
| 硬约束 | `unsafe_code = "forbid"`，clippy `pedantic` 全开 |
| 许可 | Apache-2.0 |

crate 切分是这个项目最"架构"的部分：

| crate | 行数 | 职责 |
|---|---:|---|
| `urouter-types` | 383 | 基础类型 |
| `urouter-contracts` | 1573 | **纯函数决策核**：tier 级联 / picker / retry / cooldown / fallback |
| `urouter-core` | 2016 | Route 配置 + 语义分类 + `decide` / `bind_decision` |
| `urouter-ai` | 2101 | Catalog + 能力准入 admission |
| `urouter-artifact` | 1188 | learned policy 推理，**全整数** |
| `urouter-eval` | 1096 | IPS / SNIPS / DR 反事实评估 |
| `urouter-protocol` | 1427 | 协议翻译 |
| `urouter-gateway` | 18407 | HTTP / Redis / 熔断 / 配额 / 绑定 |
| `urouter-embed` | 451 | 进程内嵌入（与网关共享决策核，parity 有测试保证） |
| `urouter-py` | 111 | Python 绑定 |

### 2.2 七层决策器

| 层 | 位置 | 性质 |
|---|---|---|
| 1 能力准入 | `urouter-ai::admission::eligible_models` | 布尔硬过滤，**显式指定模型也必须过** |
| 2 语义分类 | `urouter-core::classify_semantic_task` | 窄关键词 + 置信度，**显式 abstain** |
| 3 Tier 级联 | `select_tier_with_cascade` | Pin → QualityGuard → Default，短路 + trace |
| 4 部署选择 | `plan_capacity_lease_with_picker` | order 分层 → picker 降权 → 加权票选 |
| 5 失败处理 | retry / cooldown / fallback 三个独立决策器 | |
| 6 learned policy | `urouter-artifact::infer` | 默认关闭，三道守卫 |
| 7 探索 | `apply_controlled_exploration` | ε-greedy，**哈希非随机** |

几个判断力很好的地方：

- **"宁可不判，也不猜错"**。`解方程 x²-5x+6=0` 故意不触发 `EquationSolving`（含 `=` 和 `x` 但无 `y`，无关键词），落回 `General`。理由：误判代价 > 漏判代价。

- **天气规则是安全断言，不是分类**。缺 Host 声明的 `get_weather` 时在调上游**之前**返回 400 `missing_required_tool` —— 网关绝不伪造工具结果。而外置 `semantic_rules` 的默认 `mode: extend` 保证一份忘了写天气规则的配置**无法静默移除**这个保证；要关掉必须显式选 `replace`。**这是把安全属性编码进配置语义的做法。**

- **规则外置复用签名通道**。`semantic_rules` 放在 `route.json` 里是刻意的：签名 control manifest 已经对它哈希，规则因此白拿 revision 绑定、热加载、`last_good` / `fail_closed` 和一键回滚，不需要第二条要保持一致的分发通道。校验在 `RouteConfig::validate` 内，坏规则集走既有的失败路径 —— 没有第二条要维护的拒绝逻辑。

- **`PinRule` 不受 `floor_tier` 约束**。pin 是显式意图，不应被 floor 静默改写。

- **一处 reason 改写**：级联结束后，若候选集被能力过滤缩减过且选中的正是首个候选，reason 改写为 `capability_required` 并追加 `capability_filter` 规则记录 —— 让"因为能力受限才选了它"和"因为默认策略选了它"在证据上可区分。

- **单部署永不熔断**。`cooldown_directive` 的前置条件 `tier_size > 1` —— 只有一个部署时熔断它等于自杀，没有任何地方可以转移流量。这条在很多熔断实现里是缺的。熔断按**失败率**（`failures × 1000 / (successes + failures)`）而非失败计数，`RateLimited` 单独短路（限流是明确的容量信号，不必等失败率累积）。

- **fallback 链的三重保护**（`plan_fallback_tiers`）：`emitted` 去重、`active` 环检测（`A → B → A` 直接报错而非无限展开）、`max_depth` 截断。且**按最坏链路成本预留预算** —— 不会出现"够走第一跳但走完 fallback 超支"。`FallbackCause` 12 个变体，8 个由 `UpstreamErrorKind` 映射，4 个仅由路由侧产生（`ContextWindow` / `ContentPolicy` / `Quota` / `Capacity`），因此"超时降级到便宜 tier"和"限流转移到另一家 provider"可以是两条不同的链。

- **部署选择的四种 picker**：`Weighted`（默认）/ `LeastLoaded`（in_flight）/ `LowestLatency`（latency_ewma_ms）/ `LowestQuotaUsage`（quota_usage_millis）。同层内取最优指标值，把**严格劣于**它的候选标为不可用并记原因码；指标缺失的候选不参与比较、不被降权。

- **cache affinity**：`prompt_profile_hash → 上次成功部署` 的有界映射，命中时把该部署 `order` 置 0、其余 +1。三个约束：只在已通过准入的候选内重排、状态缺失时 **fail-open**、命中时计数并在 trace 里标 `cache_affinity_hit`。

### 2.3 决定性架构：确定性优先于最优性

这是 uRouter 与 freellmapi 最本质的分歧点。uRouter 在**四个**地方主动放弃了看起来更优的方案：

| 场景 | 常规做法 | uRouter |
|---|---|---|
| 加权选择 | 随机数 | `ticket % total_weight` 后按 ID 排序线性扫描 |
| Canary 分桶 | 随机采样 | `SHA256(tenant_key + "\n" + task_key)[0..2] % 10000` |
| 探索 | RNG | `SHA256(tenant \0 task \0 "exploration-v1")` 派生 draw |
| learned policy 推理 | 浮点 MLP | **全整数运算**（MLP / KNN / Contrastive 三种） |

换来三件事：

1. **决策可重放** —— 同 Catalog / Route / artifact / 上下文 / ticket → 同结果
2. **归因可信** —— DecisionRecord v2 存完整证据链而非结论
3. **可离线反事实评估** —— propensity 必须可复算，否则 IPS / SNIPS / DR 全是假的

全整数推理还有一个连带效果：artifact **可签名**并作为发布物分发 —— 浮点跨平台差异会让签名失效。

learned policy 的三道守卫也设计得克制：kill switch（无条件）、**操作预算**（`operation_limit < required_operations` 即回退，MLP 需 `hidden × (dims + 2)`，KNN 需 `prototypes × (dims + 1)` —— **有界推理，防止推理本身成为延迟来源**）、支持域（语义任务不在 `support.semantic_tasks` 内 / 输入超字节上限 / 有工具但 `tools_supported == false`）。另有一道运行时守卫：artifact 绑定的 revision 与当前控制面不一致时直接回退规则决策，`fallback_reason = control_revision_changed`。

探索需要**同时**满足五个条件才启用（recording 已开 + `allow_training` + 显式探索授权 + 请求探索预算不超服务端上界 + 至少 2 个可选 tier 且基线在其中），任一不满足直接返回 `None`。

### 2.4 会话绑定（freellmapi 完全没有的维度）

`gateway/src/binding.rs` 的 `TaskBinding` 把 `(tenant, conversation, branch)` 固定到**精确的 model + provider + api**：

```rust
pub(crate) struct TaskBinding {
    tenant_key, binding_key, task_key,
    conversation_key, branch_key,
    tier, model, provider, api,
    agent_harness, prompt_profile_hash, toolset_hash,
    bound_at_turn, last_seen_turn,
    generation, tenant_generation, task_generation,
}
```

写入返回四态 `Created / Unchanged / Migrated / Conflict`，三级 generation 用于失效，`StaleScope` 错误专门捕获"写入跨越了删除边界"。

这解决的是 freellmapi 结构上无法解决的问题：**多轮对话中途换模型导致行为跳变**。对聊天玩具无所谓，对 Agent 产品是灾难。

### 2.5 运行时基础设施

- **Redis 原生**：熔断状态、绑定状态、Half-Open 探针（全局只允许一个，Redis 原子保证）、配额 lease 全部走 Lua `Script`。为多实例设计。
- **三作用域独立熔断**：`deployment` / `credential` / `provider`，互不干扰。
- **控制面 revision 绑定**：签名 control manifest 对 catalog + route + semantic_rules 一起哈希，热加载带 `last_good` / `fail_closed` / 一键回滚。
- **`--dry-run`**：16 项纯配置校验，不碰网络 / Redis / 端口，失败退出码 2。
- **`/v1/explain`**：只做决策不调上游，返回 `feature_frame`（结构化特征）/ `admission`（谁被排除及原因）/ `routing_trace`（级联决策事件）。
- **响应头披露**：`x-urouter-request-id` / `decision-id` / `tier` / `model` / `reason` / `source` / `degraded` / `compatibility-mode` / `alternatives`。

`x-urouter-compatibility-mode: true` 这个设计很精妙 —— 用裸 OpenAI SDK 接入能用，但会明确告诉你"你拿不到会话连续性"。**降级是可见的，不是静默的。**

管理面端点：`/v1/artifacts` 的 promote / rollback / kill / rollout / observe，`/v1/catalog` 的 refresh / rollback，`/v1/decisions`、`/v1/stats`、`/v1/feedback`、`/metrics`、`/v1/tiers`。

### 2.6 弱点

诚实说，短板同样明显：

1. **Catalog 只有 7 个模型、6 个 provider**（`anthropic` / `local-fixture` / `local-vllm` / `local-vllm-qwen38` / `openai` / `siliconflow`），其中 3 个是本地 vLLM / fixture。默认 `route.json` 只有 `efficient` / `capable` 两个 tier，都指向本地 vLLM 端点。相比 freellmapi 的 474 模型族 / 635 端点，**供给侧几乎是空的**。

2. **没有 provider 适配器的广度**，也没有 key 池、key 加密、key 轮换、多 key 配额账本这一整套。freellmapi 的 `ratelimit.ts` 1435 行在 uRouter 里没有对应物（`quota.rs` 752 行 + `capacity.rs` 549 行是不同的抽象层：租约与容量，不是免费额度账本）。

3. **没有 UI**。freellmapi 有完整 React 仪表盘 + Electron 桌面端 + 15 个 agent 的一键配置生成器。uRouter 只有 HTTP API 和 JSON 配置文件。

4. **语义分类只有 3 条内置高置信规则**（Greeting 980 / RealtimeWeather 950 / EquationSolving 930）。虽然可外置到 `route.json`，但 `task` 只能取 4 个内置枚举值 —— 新增业务意图（如 SQL 生成）目前得借用一个既有 task 值，路由效果正确但在指标和 artifact 支持域里会显示成借用的名字。

5. **成熟度**。README 自己写 `code_complete ≠ 生产 accepted`：Redis HA / ACL / TLS、真实数据集、shadow / canary 观察窗口仍需外部验收。freellmapi 已经 v0.9.4、680 commits、上了 Google Play。

6. **决策骨架复杂度远超当前决策对象数量**。七层决策器现在只在 2 个 tier 之间做选择。

---

## 三、逐维度对比

| 维度 | freellmapi | uRouter |
|---|---|---|
| **核心矛盾** | 配额稀缺 | 决策归因 |
| **路由目标** | 找到一个**还能用**的模型 | 找到**该用**的模型并解释为什么 |
| **决策模型** | Thompson bandit 单一打分（3 轴凸组合 × 2 护栏） | 7 层独立决策器，每层显式 abstain |
| **确定性** | ❌ `Math.random()` 采样 + 进程内可变状态 | ✅ 哈希 / 整数 / ticket，全链路可重放 |
| **会话连续性** | ❌ 无 | ✅ `conversation+branch` 绑定到精确 model+provider+api |
| **状态后端** | SQLite + 进程内 Map | Redis（Lua 原子）+ 内存快照 |
| **横向扩展** | ❌ 单进程单用户（文档明说） | ✅ 多实例设计（Half-Open 全局单探针） |
| **多租户** | ❌ 明确不做 | ✅ tenant 头 + tenant_allowlist + residency |
| **供给侧广度** | ✅ 34 平台 / 635 端点 / 11 适配器 | ❌ 6 provider / 7 模型 |
| **配额管理** | ✅ RPM/RPD/TPM/TPD 双层 + 冷却阶梯 + lease | 部分（quota.rs / capacity.rs，无免费额度账本） |
| **失败处理** | 20 次尝试 / 45s 预算 / 滑动窗口 bench | 类型化 retry + 三作用域熔断 + DFS fallback 图（环检测） |
| **可解释性** | 排除原因归 9 桶 + attempt trail 头 | DecisionRecord v2 完整证据链 + `/v1/explain` + 8 个响应头 |
| **离线评估** | ❌ 无 | ✅ IPS / SNIPS / DR + 方差 + 95% CI + ESS |
| **learned policy** | ❌ 无（bandit 是在线的） | ✅ 可选、整数、可签名、三道守卫 + canary/shadow |
| **上下文压缩** | ✅ 8 引擎管线 + fidelity gate | ❌ 无 |
| **多模型合成** | ✅ fusion（4 路 panel + judge） | ❌ 无 |
| **协议面** | OpenAI / Anthropic / Gemini / Ollama / Responses / MCP | OpenAI / Anthropic / Responses |
| **配置分发** | ✅ Ed25519 签名目录，自动下发 | ✅ 签名 control manifest + revision 绑定 + 回滚 |
| **UI / 生态** | ✅ React 仪表盘 + Electron + CLI + 15 agent 生成器 | ❌ 仅 API |
| **测试** | 275 test 文件 | 319 tests，77.9% 覆盖 |
| **代码量** | 129.6k 行 TS | 35.7k 行 Rust |
| **许可** | MIT | Apache-2.0 |

---

## 四、三个更深的架构分歧

### 4.1 "打分" vs "过滤 + 级联"

freellmapi 把所有信号压进**一个标量**，靠权重预设调整偏好。优点是新信号加一个轴就行，缺点是**任何两个模型的顺序都可能因为任一信号微动而翻转**，且事后只能说"它分高"。

uRouter 是**先硬过滤（能力准入，布尔排除 + 逐条原因），再按规则短路级联（Pin → Quality → Default），最后在同 order 层内确定性票选**。优点是每一步的"为什么"都是可陈述的命题；缺点是规则集需要人维护，泛化能力靠 learned policy 补，而后者默认关闭。

**这不是谁对谁错，是"优化目标"vs"合约执行"的区别。**

- 免费额度池里没有合约，只有可用性 —— 打分是对的
- Agent 产品里有合约（这个 conversation 必须稳定、这个租户不能出境、这次调用必须能解释）—— 过滤 + 级联是对的

### 4.2 状态放哪里

freellmapi 的路由状态（penalty Map、roundRobin index、stats cache、in-flight leases）**全在进程内存**，SQLite 只做持久化兜底。这让它快、简单、零外部依赖，代价是不能扩、不能重放。

uRouter 把所有可变状态推到 **Redis + Lua 原子脚本**，决策核本身是**零 I/O 纯函数**（`urouter-contracts` / `urouter-core`），运行时只负责注入 `CapacitySnapshot` 和管理 lease。三个直接后果：

1. 网关与 `urouter-embed` 共用同一决策核，对相同输入产出相同决策，且这个 parity 有测试保证
2. 决策可被穷举测试，不需要起网络或 Redis
3. 决策可重放

**"纯函数核 + 快照注入"是 uRouter 最值得保留的架构资产。** 它让 embed 模式（进程内嵌入，无 HTTP 跳）和网关模式共享同一份语义 —— freellmapi 结构上做不到，它的 `router.ts` 直接 `getDb()`、直接读进程内 Map。

### 4.3 商业模式如何塑造架构

freellmapi 的 Premium 是"**目录的时效性**"（免费装 30 天延迟快照，付费装实时）。这直接决定了 `catalog-sync` 必须做 Ed25519 签名、必须有 `MIN_CATALOG_VERSION` 防回滚、必须 `hasProvider` gate。**收费点长在数据分发通道上，所以那个通道被做得最硬。**

uRouter 没有对应的商业化叙事，但它的 control manifest + revision 绑定 + artifact 签名 / promote / rollback / kill 那一套，本质是同一类基础设施（签名配置的安全分发），只是服务于**审计与回滚**而非**订阅门禁**。

---

## 五、可互相借鉴的点

### 5.1 uRouter 可以从 freellmapi 借的（按价值排序）

1. **配额账本（最高价值）**
   `ratelimit.ts` 的四维滑动窗口 + provider 级双层 cap + 递增冷却阶梯 + 并发 lease，是 uRouter 接真实商业 provider（不只是本地 vLLM）时必然要写的东西。
   **`OUTPUT_RESERVE_CAP` 那个坑建议直接抄结论**：按 `max_tokens` 全量预留会假性排空整个池子，返回零上游调用的假 429。

2. **`Retry-After` / 上游 back-off 提取**
   `providers/base.ts` 里 DFS 找 `retryDelay`（含 protobuf duration `"17s"`）+ 散文正则 + 24h clamp 的做法，比 uRouter 当前"有 `retry_after_ms` 就用，否则指数退避"要完整 —— 问题在于**谁来产出那个 `retry_after_ms`**，freellmapi 给了答案。

3. **失败原因的桶化摘要**
   uRouter 有 DecisionRecord 全证据链，但客户端拿到的错误未必好读。`summarizeExhaustion` 那种"N routes checked (3 rate-limited, 2 no key…), soonest reset ~23m"值得作为 `NoEligibleTier` 的响应体。

4. **上下文压缩管线**
   TPM 是任何多模型网关的硬约束。8 引擎 + fidelity gate + 稳定前缀冻结的分层设计可以整体移植，而且天然适合做成 uRouter 的一个纯函数 crate（无 I/O，可穷举测试，符合现有架构）。

5. **速度轴把超时计入**（#619）
   uRouter 的 `LowestLatency` picker 依赖 `latency_ewma_ms`，目前大概率只统计成功请求 —— 同样的 bug 会复现：一个一半请求挂死的部署会保持漂亮的延迟数字。

6. **协议面广度**
   Gemini `/v1beta` 原生 wire、Ollama 模拟（让 Zed / JetBrains 能接）、MCP server。uRouter 的 `protocol_translation.rs` 只有 398 行，扩展空间大。

### 5.2 freellmapi 能从 uRouter 借的

其实只有一个但很关键：**会话绑定**。

它现在被 15 个 coding agent 使用，而 coding agent 恰恰是最受不了中途换模型的场景。以它的架构（进程内 Map + SQLite）实现一个 `(conversation, branch) → model` 的粘性表并不难，难的是承认 **bandit 不该在会话中途重新采样**。

次要的：`/v1/explain` 式的"只决策不调用"端点，对调试比 attempt trail 头更有用。

---

## 六、总结与建议

### 一句话总结

**freellmapi 是把"不可靠的免费供给"工程化成"可用服务"的教科书** —— 它的价值密度在配额账本、退避解析、错误分桶这些"只有跑过生产才写得出"的细节里，而不在算法。

**uRouter 是把"路由决策"工程化成"可审计资产"的教科书** —— 纯函数核 + 确定性算法 + 完整证据链 + 反事实评估这一套，freellmapi 结构上永远做不到。

### 对 uRouter 的建议

uRouter 当前最大的风险不是架构，是**供给侧太空**：7 个模型、默认指向本地 vLLM 的配置，决定了那套精密的七层决策器现在只在 2 个 tier 之间做选择。**决策骨架的复杂度已经远超它当前要决策的对象数量。**

下一步边际收益最高的不是继续做决策算法，而是：

1. **把 catalog 和 provider 适配层做厚** —— 这恰好是 freellmapi 已经验证过的那部分，且不冲突（uRouter 的 `catalog/providers/` 目录已经有 `volcengine-ark` 的雏形）
2. **补配额账本** —— 接商业 provider 的前置条件
3. **保住纯函数核不被污染** —— 上面两项都应该以"快照注入"的形式进入决策核，而不是让 `urouter-core` 直接读状态

保持决策核的零 I/O 纯函数性质，是 uRouter 相对所有同类项目（LiteLLM / OpenRouter / Portkey / freellmapi）唯一不可复制的护城河。

---

## 相关文档

- [`README.md`](README.md) —— uRouter 接入方式与架构总览
- [`uRouter_决策算法说明.md`](uRouter_决策算法说明.md) —— 七层决策器逐层说明
- [`uRouter_技术架构流程图.md`](uRouter_技术架构流程图.md) —— 完整请求流程图
- [`LiteLLM_Router_深度技术解读报告.md`](LiteLLM_Router_深度技术解读报告.md)
- [`LLMRouter_深度技术解读报告.md`](LLMRouter_深度技术解读报告.md)
- [`Switchyard_深度技术解读报告.md`](Switchyard_深度技术解读报告.md)
- `opensource/freellmapi/docs/architecture.md` —— freellmapi 官方架构文档
