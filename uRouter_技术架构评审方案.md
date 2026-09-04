# uRouter 技术架构评审方案

> 评审日期：2026-09-04
> 依据：对当前代码逐行核对，所有结论均附可复现的验证命令
> 立场：这份文档只写我能证明的问题。其中 P0-2 是我在本轮改动中自己引入的债，一并列出。

---

## 零、总体判断

**架构骨架是好的，问题集中在两处：估算层不可信，以及运行时状态的一致性边界画得不齐。**

值得保留的部分（不要动）：

- **纯函数决策核 + 快照注入**。`urouter-contracts` 依赖只有 `serde`，由 `xtask check-pure-crates` 在 CI 强制。这带来 gateway↔embed parity、决策可穷举测试、决策可重放——这是 uRouter 相对同类项目唯一不可复制的部分。
- **事实与行为分离**。`catalog/` 是可签名、可版本、可回滚的数据；`crates/` 是行为。新增一个 OpenAI 兼容 provider 是一条目录条目而不是一次发版，这条在本轮导入 39 家 provider 时得到了验证。
- **确定性优先于最优性**。加权票选用 ticket 取模、canary 用稳定哈希、探索用哈希派生、learned policy 全整数——换来可重放与可离线评估。

下面是需要修的。按严重度排序，每条都给了验证方式和我建议的处置。

---

## 一、P0：估算层

### P0-1 两套 token 估算器对同一请求给出 3 倍分歧

**事实**

```
crates/urouter-core/src/lib.rs:1053   let prompt_tokens = characters.div_ceil(4);   // 准入用
crates/urouter-gateway/src/main.rs:4599  let input_tokens = serialized_bytes.div_ceil(4).max(1);  // 配额/成本用
```

同一段中文请求（460 字符 / 1380 字节）：

| 消费方 | 估算 | 用途 |
|---|---:|---|
| `urouter-core` | **115 tok** | `min_context_window`，决定哪些模型被准入 |
| `urouter-gateway` | **345 tok** | 配额预留、成本预留 |
| 真实 tokenizer | ~306–460 tok | — |

**为什么是 P0**

准入侧低估约 3–4 倍。后果不是"估得不准"，而是**准入会放行上下文窗口装不下这个请求的模型**，然后请求在上游以 `context_length_exceeded` 失败。这条链路本轮刚接通（错误体分类 → `FallbackCause::ContextWindow` → 落到长上下文 tier），所以现在不会硬失败，但每次都要浪费一次上游往返和一次 fallback 跳转。

英文场景 `chars/4` 大致正确，所以这个缺陷在中文流量上才暴露——而这个项目显然服务中文。

**复现**

```bash
python3 -c "
s='请帮我分析一下这段代码的性能瓶颈'*20
print('chars/4 =', len(s)//4, '| bytes/4 =', len(s.encode())//4)"
```

**建议处置**

1. 立刻：把两处统一到同一个纯函数（放 `urouter-contracts::tokens`），消除 3 倍分歧。这是低风险改动，且分歧本身无论如何都是 bug。
2. 短期：估算器按脚本分段——ASCII 段 `chars/4`，CJK 段 `chars/1.5`。仍是启发式，但把中文误差从 3 倍压到 1.5 倍以内，且不引入依赖。
3. 中期：`ModelSpec` 增 `tokenizer` 提示（`cl100k` / `o200k` / `sentencepiece` / `unknown`），按需接真 tokenizer。这是目录事实，不是代码分支。

**验收**：一条中文长 prompt 的准入估算与配额估算之差 < 20%；`/v1/explain` 上新增 `estimated_input_tokens` 便于核对。

---

### P0-2 两套并行的配额系统（本轮引入的债）

**事实**

```
crates/urouter-gateway/src/quota.rs         租户级：--tenant-* 标志，in-flight/RPM/TPM
crates/urouter-gateway/src/scoped_quota.rs  多作用域：route.quota_limits，四作用域×两窗口×三维度
```

两套各有：独立的 Repository trait、独立的 Redis key 空间（`:quota:` vs `:quota:v2:`）、独立的 Lease 与 `Drop`、独立的错误码路径。一个租户级 RPM 现在可以在两个地方配置，且两者都会生效。

**为什么是我的债**

我选择新增而非替换，理由是旧路径的语义与三个错误码被既有测试钉死，替换风险高。这个判断在当时是对的，但它留下的是重复而不是分层——文档里写的"两层，都必须通过"是对现状的合理化，不是设计意图。

**建议处置**

把租户级并入多作用域账本：`--tenant-max-in-flight` 等标志在启动时翻译成三条 `QuotaLimit`（Tenant/InFlight/Concurrency、Tenant/Minute/Requests、Tenant/Minute/Tokens），旧的 `quota.rs` 整个删除。`QuotaBreach::error_code()` 已经保留了三个历史租户错误码，所以对客户端零影响。

**验收**：`quota.rs` 删除；`tests.rs:3685` 的错误码断言不变通过；Redis 只剩 `:quota:v2:` 一个 key 空间。

---

## 二、P1：一致性边界

### P1-1 两个 picker 的信号是进程本地的，熔断却是共享的

**事实**

```
crates/urouter-gateway/src/capacity.rs:88   health: Mutex<BTreeMap<String, DeploymentHealth>>
    DeploymentHealth { latency_ewma_ms, observed_quota_usage_millis, in_flight, events }
crates/urouter-gateway/src/circuit.rs       RedisCircuitRepository（7 处）
```

熔断状态走 Redis 是因为"每实例各自判断"被认定为错误；但 `latency_ewma_ms` 与 `observed_quota_usage_millis` 仍是进程本地。多实例部署下：

- `DeploymentPicker::LowestLatency` 按本实例观察到的延迟排序
- `DeploymentPicker::LowestQuotaUsage` 按本实例观察到的配额消耗排序——**而配额本身是跨实例共享的**

第二条尤其别扭：账本在 Redis 里知道全局用了多少，但回写给 picker 的是本实例那份。刚扩容的新实例会认为所有部署配额都是 0，从而集中打向已经接近耗尽的那个。

**建议处置**

`quota_usage_millis` 已经由 Redis 账本算出全局值——把回写改成用 `reserve` 返回的 observation（它已经是全局的），而不是本实例累计。这一条几乎零成本，因为数据已经在手上。

`latency_ewma_ms` 保持进程本地是可以接受的（`docs/redis-consistency-matrix.md` 已声明它是"进程本地建议值，无正确性决策依赖"），但需要在 `LowestLatency` 的文档里写明这一点，否则运维会误以为它是全局最优。

**验收**：`docs/redis-consistency-matrix.md` 为这两个信号各增一行，明确标注作用域；`quota_usage_millis` 的回写来源改为 observation 并加一条多实例测试。

---

### P1-2 `Compat` 七个字段中四个仍未被消费

**事实**（逐字段 grep 消费点）

| 字段 | 消费点 | 状态 |
|---|---:|---|
| `max_tokens_field` | 1 | 已接 |
| `supports_usage_in_streaming` | 1 | 本轮接通 |
| `supports_developer_role` | 1 | 已接 |
| `supports_finish_reason` | **0** | 死 |
| `tool_call_format` | **0** | 死 |
| `structured_output_format` | **0** | 死 |
| `thinking_format` | **0** | 死 |

这四个字段被 schema 校验、被写进 `catalog/manifest.json` 的 `compat` 哈希、被要求非 built_in provider 必填——**但从不被读取**。

**为什么要处理**

一个被校验、被哈希、被强制填写的字段，读起来像是一个保证。运维填了 `tool_call_format: anthropic`，会合理地认为工具调用会按 Anthropic 格式序列化——实际不会。这比字段不存在更危险。

**建议处置**

二选一，不要拖：

- **接上**：`tool_call_format` 与 `structured_output_format` 在 transport 的 `build_request` 里生效，`thinking_format` 决定推理参数形状。这是 transport 层已经具备的插槽。
- **或删掉**：从 `Compat` 移除，`missing_required_fields` 同步收缩。目录哈希会变，走一次正常发布。

我倾向前者——这四个字段描述的差异是真实存在的（Anthropic 的 `tool_use` 块 vs OpenAI 的 `tool_calls`），迟早要接。

**验收**：`grep -c` 每个字段消费点 > 0，或字段已从 `Compat` 移除。

---

### P1-3 出站适配层按 wire 组织，缺 provider 维度

**事实**

`TransportRegistry` 按 `WireApi` 注册 4 个 transport。但有三类差异不属于任何一种 wire：

| Provider | 差异 | 现状 |
|---|---|---|
| Cloudflare | URL 内嵌 per-install account id | **未导入**（41 家里唯一缺的） |
| Sail | 提交 job → 轮询 → 终态转回 Chat | 目录里有，实际调用会失败 |
| AI Horde | 队列语义、`max_tokens` 下限 16、usage 只有 kudos | 手工加了目录条目，行为差异未表达 |

**建议处置**

`TransportRegistry` 增一层 provider 覆盖：先按 `provider.id` 查特化，未命中再按 `WireApi` 查默认。不需要新 crate，`ProviderTransport` trait 不变。

**验收**：Cloudflare 能作为一条带 `{ACCOUNT_ID}` 模板的目录条目导入；`registry.resolve_for(provider, api)` 有测试覆盖特化优先。

---

## 三、P2：结构与规模

### P2-1 `main.rs` 6700 行

**事实**：本轮 clippy 的 `too_many_lines` 触发 5 次，每次我都靠抽helper 压回 100 行以下。这是症状不是原因——`main.rs` 同时承载 HTTP handler、tier 执行循环、重试、协议 handler、成本估算、配额估算、错误映射、流式处理。

**建议处置**：按已有的模块边界继续切分，优先度：

```
execution.rs   execute_routed_upstream / execute_tier / 重试循环   （最大、最常改）
handlers.rs    chat_completions / responses / anthropic / gemini / ollama
estimate.rs    token 与成本估算（正好是 P0-1 要统一的地方）
error.rs       GatewayError 及其 From/IntoResponse
```

不急，但每次改动都在加重。切分是纯机械操作，`pub(crate)` 即可，无行为变更。

---

### P2-2 决策核的复杂度远超它当前的配置

**事实**：七层决策器 + learned policy + 反事实评估，运行在 **2 个 tier / 9 个模型**上。44 个 provider 中 36 个无模型。

这不是设计缺陷，是投入错配：继续打磨决策算法的边际收益，远低于把模型池填起来。分级路由在 2 个候选之间做选择时，`LowestLatency` / `LowestQuotaUsage` / 加权票选这些机制基本没有发挥空间。

**建议处置**：把下一阶段的重心从 `crates/` 移到 `catalog/`。每接入一家 provider 需要的是凭据 + 一次评审，不是代码。

---

### P2-3 目录没有 per-install 分层

**事实**：`catalog/catalog.json` 是单文件。运维新增一个自有 provider 必须编辑随仓库发布的那份，于是每次升级都冲突。

**建议处置**：增加 overlay：`--catalog-overlay catalog.local.json`，加载时合并（本地覆盖同 id 条目），overlay 参与 revision 计算但不进随仓库发布的 manifest。

---

## 四、P3：功能缺口（已知，非缺陷）

| 项 | 现状 | 影响 |
|---|---|---|
| 上游流式仅 `OpenAiChat` | Responses/Anthropic/Gemini 出站不支持流 | agent 客户端默认流式，会落到非流 tier |
| Gemini/Ollama 入站不支持流 | 建流前显式拒绝 | 行为正确，但功能缺失 |
| `/v1/embeddings` | 未实现 | 目录里无 embedding 模型，缺能力位与路由路径 |
| 音视频模态 | `Modality::{Audio,Video}` 存在但无代码路径 | `ContentPart` 只有 Text/ImageUrl/Json |

这些都在文档里如实标注了，不构成"设计问题"，但应进排期。

---

## 五、评审执行方案

### 阶段划分

| 阶段 | 内容 | 前置 | 验收 |
|---|---|---|---|
| **R1** | P0-1 统一 token 估算 | 无 | 中英文估算分歧 < 20%，新增对比测试 |
| **R2** | P1-1 配额观测改用全局值 | 无 | 多实例测试：新实例看到已消耗的配额 |
| **R3** | P0-2 合并两套配额 | R2 | `quota.rs` 删除，错误码断言原样通过 |
| **R4** | P1-2 接通或删除四个死字段 | 无 | 每字段消费点 > 0 或已移除 |
| **R5** | P1-3 transport provider 特化 | R4 | Cloudflare 可导入 |
| **R6** | P2-1 `main.rs` 切分 | R3 | 单文件 < 2500 行，行为零变更 |
| **R7** | P2-3 目录 overlay | 无 | 升级不产生目录冲突 |

R1、R2、R4、R7 相互独立，可并行。

### 每阶段的固定门禁

```bash
cargo test --workspace                                  # 当前 457 passed / 0 failed
cargo clippy --workspace --all-targets                  # 当前 0 告警
cargo deny check                                        # advisories/bans/licenses/sources
cargo run -p urouter-xtask -- check-pure-crates          # contracts 依赖边界
cargo run -p urouter-xtask -- check-deploy
cargo run -p urouter-gateway -- --dry-run                # 当前 18 项
```

改 `RouteConfig` 时额外注意：新字段必须带 `skip_serializing_if`，`the_shipped_route_revision_is_pinned` 会拦住违规。

改 `catalog/` 时三件一起重生成：`catalog check` → `manifest` → `sync control-manifest`。

### 不该在本轮评审中做的

- **不要引入"利用率软降权护栏"**（在 80% 利用率线性降权）。它会成为与 `DeploymentPicker` 打架的第二套未声明选择策略，并让 `plan_capacity_lease_with_picker` 对固定快照变得不确定——直接摧毁纯函数核存在的理由。`LowestQuotaUsage` 就是那条 ramp，且它是操作员选择的、确定性的、已被 trace 的。
- **不要把排除原因从 `Vec<String>` 迁成枚举**。它们被序列化进 `DecisionRecord` schema v2 并持久化，且 `missing_modality:{}` 这类是参数化构造的。`&str` 分类器已提供枚举的全部好处而零 wire 代价。
- **不要给 `urouter-contracts` 加依赖**来解决 P0-1。CJK 分段是纯字符判断，不需要 tokenizer 库；真要接 tokenizer，接在 gateway 侧并把结果作为快照注入。

---

## 六、结论

架构没有需要推翻的地方。**最值得修的是 P0-1**——两个子系统对同一个请求的大小给出 3 倍不同的答案，这是正确性问题而不是精度问题，并且只在中文流量上暴露。**最该承认的是 P0-2**——我为了规避风险选择了新增而非替换，留下的是重复。

其余各条是演进债务，不阻塞使用。

真正决定这个项目下一步价值的，不在 `crates/` 里：44 个 provider 中 36 个没有模型。决策核已经比它要决策的对象复杂得多。

---

## 相关文档

- [`uRouter_技术架构流程图.md`](uRouter_技术架构流程图.md) — 总体架构图与请求流程
- [`uRouter_决策算法说明.md`](uRouter_决策算法说明.md) — 七层决策器逐层说明
- [`FreeLLMAPI_深度技术解读与uRouter架构对比报告.md`](FreeLLMAPI_深度技术解读与uRouter架构对比报告.md) — 与参考实现的对比
- `docs/redis-consistency-matrix.md` — 共享状态一致性矩阵（P1-1 需更新）
- `docs/public-api-and-semver.md` — 公共 API 变更约束
