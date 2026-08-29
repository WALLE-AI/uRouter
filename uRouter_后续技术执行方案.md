# uRouter 后续技术执行方案

> 版本：v1.1（架构复审修订）  
> 基线日期：2026-08-29  
> 工作目录：`D:\llm\uRouter`  
> 适用范围：从当前可运行的 OpenAI 兼容 Gateway 基线，继续完成原始设计中的核心架构、生产能力、评估学习和嵌入形态。

## 1. 执行结论

当前项目不是从零开始：`urouter-ai`、单实例 Gateway、Redis 多实例状态、基础容量/熔断、记录反馈、RBAC、SessionBinding、AionUI/WorkBuddy 适配器和 provider discovery 已经存在。后续工作不得重写这些能力，而应采用“保持行为、逐步抽核、按门禁交付”的方式推进。

推荐关键路径：

```text
P0 基线治理
  -> P1 核心契约、记录与纯决策内核
  -> P1.5 Provider/Deployment 基础契约
  -> P2 生产路由与可观测性
  -> P3 Catalog 生命周期与语义路由
  -> P4 数据集与离线评估
  -> P5 RouterArtifact、Shadow、Canary
  -> P6 协议扩展与嵌入形态
```

复审后的初步工作量按 1 名熟悉 Rust 的工程师估算为 20-28 个净工程周；2 名工程师约需 14-18 个日历周。核心契约、真实数据观察和 Artifact 发布存在强串行依赖，不能按人数线性压缩。真实流量观察、云资源审批、Redis HA 演练和灰度观察时间不计入净工程周。完成 P0 后必须用实际代码规模和依赖图重新估算，不能把本文日期当作发布承诺。

## 2. 执行原则

1. **一个完成口径**：以后只使用本文任务 ID 和退出门禁，不再用“优化版 M0”代表原始设计全部完成。
2. **兼容优先**：每次重构后，`/v1/chat/completions`、`/v1/models`、`/v1/explain` 和现有 Redis 状态语义必须保持兼容。
3. **禁止空壳 crate**：只有当代码、测试和调用方可在同一个变更中迁入时才创建新 crate。
4. **决策核零 I/O**：特征、质量决策、容量过滤和可靠性策略只接收显式快照，不直接访问网络、磁盘、Redis、环境变量或系统时钟。
5. **先可解释再智能化**：没有完整 Feature/Filter/Picker/Cascade trace，不上线语义或学习路由。
6. **先 Shadow 再 Canary**：学习策略不能直接接管生产请求。
7. **事实与策略分离**：Catalog 保存可审计事实；Route/Artifact 保存选择策略；运行时健康状态不写回 Catalog。
8. **密钥不落库**：密钥只从环境变量、secret manager 或受控凭据代理读取。曾在会话中暴露的商用 API key 应在继续生产验证前轮换。
9. **记录契约先于真实流量**：没有版本化 DecisionRecord、Catalog revision、特征版本和候选集合，不采集用于训练或策略评价的流量。
10. **网关不承担工具执行**：uRouter 判断是否需要工具并选择具备能力的模型；天气、搜索、数据库等工具由 Agent Host 编排执行。
11. **共享状态逐类定义一致性**：预算、配额、绑定、熔断和指标不能共用一个 Redis 故障策略。
12. **迁移先于 schema 修改**：任何 Redis key、记录或 API schema 变更必须先定义新旧版本共存与回滚方式。

## 3. 当前基线与保护范围

### 3.1 已有能力，后续必须做回归保护

| 能力 | 当前实现位置 | 后续动作 |
|---|---|---|
| Catalog、能力、兼容性、精确计价 | `crates/urouter-ai` | 保持公开 API，按 semver 演进 |
| OpenAI Chat Gateway、stream、explain | `crates/urouter-gateway` | 作为所有阶段的兼容门禁 |
| 内存/Redis SessionBinding | `binding.rs` 及 Gateway 状态层 | 抽象端口但不改变 key 和 CAS 语义 |
| 本地/共享 Circuit、Half-Open | `capacity.rs`、`circuit.rs` | 提取策略与状态访问边界 |
| DecisionRecord、feedback、TTL、删除 | Gateway 记录层 | 升级 schema 时保持旧记录可读 |
| 管理面 RBAC 与审计 | `management_auth.rs` | 扩展端点沿用同一授权矩阵 |
| Provider discovery | `tools/urouter-catalog/src/provider_sync.rs` | 继续补全 enrich/probe/publish，不另起工具 |
| smoke/soak 工具 | `tools/urouter-smoke`、`tools/urouter-soak` | 作为发布验证入口 |

### 3.2 冻结的外部契约

- 默认公开模型名保持 `urouter/auto`。
- OpenAI SDK 的 `chat.completions.create()` 调用方式保持可用。
- 现有 `urouter` 请求扩展字段继续在转发上游前删除。
- Redis 已发布 key 必须版本化迁移，禁止直接更名导致历史会话失效。
- 新增响应信息优先通过可协商 Header、`/v1/explain` 或管理接口暴露；不得破坏标准 OpenAI 响应结构。
- 每个逻辑请求必须可关联 `request_id`、`decision_id`、`attempt_id` 和 provider request id；重试不得重复记账或重复接收反馈。
- 管理 API 和 Explain 输出必须携带 schema/contract version，并通过兼容性测试。

## 4. 统一完成定义

每个任务只有同时满足以下条件才能标记为完成：

- 代码、配置 schema、错误契约和文档在同一变更中更新。
- 单元测试覆盖成功、拒绝、超时和边界条件。
- 跨 crate 契约有集成测试；在线路径变化有 HTTP 端到端测试。
- 状态机和并发代码必须有属性测试、竞态测试或故障注入，不能只依赖固定样例。
- `cargo fmt --all -- --check` 通过。
- `cargo clippy --workspace --all-targets -- -D warnings` 通过。
- `cargo test --release --workspace` 通过。
- `cargo doc --workspace --no-deps` 通过。
- Catalog 校验和语义 diff 通过。
- 不包含真实密钥、用户正文、未脱敏工具输出。
- 对延迟、内存或成本有影响的变更附前后基准。
- 新功能有关闭开关和明确回滚路径。

通用门禁命令：

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --release --workspace
cargo doc --workspace --no-deps
cargo run -q -p urouter-catalog -- --json check catalog/catalog.json
cargo run -q -p urouter-catalog -- --json diff catalog/catalog.json catalog/catalog.json
cargo run --release -p urouter-catalog --example release_benchmark
```

## 5. 阶段 P0：基线治理与安全收口

**目标**：建立唯一任务账本，确保后续重构有可重复的基线。预计 7-10 个工作日。

| ID | 任务 | 交付物 | 验收标准 |
|---|---|---|---|
| P0-01 | 建立需求追踪矩阵 | `docs/requirements-traceability.md` | 原始设计 §5-§20 每项都有状态、代码、测试和后续任务 ID |
| P0-02 | 固化当前行为基线 | `tests/baseline/` 或 Gateway 集成测试 | 四个核心端点、stream、fallback、Redis unavailable 契约可重复执行 |
| P0-03 | 对齐文档口径 | 更新设计状态、架构流程图、README | 不再出现旧 vLLM 当前链路或互相矛盾的 M0 完成描述 |
| P0-04 | 密钥安全检查 | secret 扫描门禁、轮换记录模板 | 仓库和测试输出无真实 key；已暴露商用 key 完成轮换 |
| P0-05 | 补齐 CI 基础门禁 | `.github/workflows/ci.yml` | Ubuntu/Windows、release tests、文档、Catalog、secret scan、依赖审计均自动执行 |
| P0-06 | 记录架构决策 | `docs/adr/0004-*.md` 起 | crate 边界、配置格式、错误语义、唯一部署熔断均有决策记录 |
| P0-07 | Schema 与状态迁移框架 | migration ADR、版本登记表、contract tests | Redis key、记录、Catalog 和 API 均定义双读/双写、滚动升级和旧版本回滚行为 |
| P0-08 | 请求身份与幂等契约 | request/decision/attempt/provider ID 规范 | 一次逻辑请求的多次尝试、费用和反馈可以唯一归因且不重复 |
| P0-09 | 威胁模型与信任边界 | `docs/security/threat-model.md` | client、trusted proxy、Gateway、Redis、provider 和管理面的责任明确 |
| P0-10 | API 契约基线 | OpenAPI/JSON Schema、错误 envelope | 现有管理端点、Explain、分页、Header 有机器可读版本和兼容测试 |

退出门禁：在干净环境运行通用门禁全部通过；基线 HTTP 测试无需商用 API 也能用 mock upstream 运行；付费 smoke 独立且默认关闭。

## 6. 阶段 P1：核心契约、记录与纯决策内核

**目标**：在不改变在线行为的前提下，先稳定可训练记录契约，再把 Gateway 中的纯策略拆成可测试内核。预计 4-6 周。

### 6.1 建议依赖方向

```text
urouter-contracts
  <- urouter-ai
  <- urouter-feature
  <- urouter-policy
       |- quality
       |- capacity
       `- reliability

urouter-state
  |- memory adapter
  `- redis adapter

urouter-runtime
  `- transport/execution adapters

urouter-gateway -> contracts + ai + feature + policy + state + runtime
```

早期将 quality/capacity/reliability 保持为 `urouter-policy` 内部模块，只有出现独立发布或依赖隔离需求时才继续拆 crate。新 crate 不得反向依赖 `urouter-gateway`；contracts/feature/policy 不得依赖 `reqwest`、`redis`、`axum`、`tokio::fs`。

| ID | 任务 | 主要内容 | 验收标准 |
|---|---|---|---|
| P1-01 | 定义请求与决策契约 | `FeatureFrame`、`RoutingRequirement`、`DecisionContext`、版本号 | 同一输入快照序列化后决策完全确定 |
| P1-02 | 完整追踪模型 | `CascadeTrace`、`FilterTrace`、`PickerTrace`、abstain reason | `/v1/explain` 能展示候选从进入到淘汰的全过程 |
| P1-03 | DecisionRecord v2 | Feature/Decision/Execution/Outcome、Catalog/Feature/Policy revision | P2 开始前新流量已按 v2 记录，旧记录迁移或明确隔离 |
| P1-04 | 记录隐私与探索字段 | recording mode、redaction、eligible set、propensity | 探索默认关闭，但字段在采集真实流量前已经稳定 |
| P1-05 | 提取 Capacity 策略 | capability/circuit/quota/budget filter traits 和 picker trait | golden parity、属性测试和过滤不变量全部通过 |
| P1-06 | 提取 Reliability 策略 | typed error、retry、cooldown、fallback plan | 策略只生成 `RoutingPlan`，不执行网络 I/O |
| P1-07 | 提取 DecisionCascade | Rule/Signal 接口，预留 Model/Judge/Escalation | 规则与信号决策可独立单测并支持 abstain |
| P1-08 | 建立 State ports | clock、binding、circuit、quota、record 接口 | 内存和 Redis adapter 通过同一套 contract tests |
| P1-09 | 强制依赖边界 | `cargo-deny`/依赖检查脚本 | CI 阻止纯决策 crate 引入 I/O 依赖 |
| P1-10 | 统一配置与 dry-run | 版本化配置 schema、16 项启动检查 | `urouter-gateway --config ... --dry-run` 返回结构化结果且不监听端口 |
| P1-11 | 扩展正确性测试 | property/fuzz/state-machine/concurrency tests | fallback 无环、过滤单调、JSON 不 panic、CAS/配额竞态可重复验证 |

迁移顺序必须是：先复制出纯函数并做 parity test，再切换 Gateway 调用点，最后删除旧实现。禁止一次性重写 `main.rs`。

退出门禁：现有 Gateway 全部测试通过；100 条以上 parity corpus 只是回归下限，同时通过属性、模糊、状态机和并发测试；DecisionRecord v2 已在非探索模式生产；`/v1/explain` 同一输入输出稳定；纯 crate 依赖门禁通过。

## 7. 阶段 P1.5：Provider/Deployment 基础契约

**目标**：在生产过滤链之前稳定其依赖的 provider、区域、凭据和生命周期元数据。预计 1-2 周。

| ID | 任务 | 主要内容 | 验收标准 |
|---|---|---|---|
| P15-01 | Provider identity | provider instance、service、region、API family | 三个 provider 的部署标识不依赖字符串约定 |
| P15-02 | Credential metadata | credential label、auth kind、availability，不保存 secret | auth filter 可工作，日志和 trace 不出现凭据 |
| P15-03 | Deployment lifecycle | discovered/reviewed/active/stale/retired | 非 active 部署不能进入在线候选集合 |
| P15-04 | Policy metadata | residency、tenant allowlist、RPM/TPM 来源 | P2 Filter 使用结构化字段而不是硬编码 provider |
| P15-05 | Revision binding | Catalog/Route/Deployment revision | 单次请求及其 DecisionRecord 固定同一组 revision |

退出门禁：P2 所有过滤器依赖的字段已进入 schema 并有来源/缺失语义；SiliconFlow、百炼和火山 discovery 均能投影为同一基础契约，但尚不要求自动发布。

## 8. 阶段 P2：生产路由、可靠性与可观测性

**目标**：让 Filter -> Pick -> Execute 在生产约束下闭环，并具备可运营证据。预计 3-4 周。

| ID | 任务 | 主要内容 | 验收标准 |
|---|---|---|---|
| P2-01 | 配额与并发 | RPM、TPM、max in-flight、原子预留/归还 | 双实例总量不超配；取消和失败不泄漏额度 |
| P2-02 | 预算预授权与结算 | estimate -> reserve -> execute -> settle -> release | 流式、取消、usage 缺失、重试、超预留量都有确定账务语义 |
| P2-03 | 完整过滤链 | auth、retired、region、residency、tenant policy | 每次剔除都有机器可读原因和指标 |
| P2-04 | Picker 扩展 | weighted、least-loaded、latency、quota-usage | 选择器可配置；固定 seed 的测试结果可重复 |
| P2-05 | 类型化回退 | context、policy、quota、timeout 等不同 fallback 链 | Bad Request 不误重试；每类错误走规定链路 |
| P2-06 | 流式部分失败 | client cancel、first-token 后失败、usage unavailable | 断连及时取消上游；部分失败不会静默重复扣费 |
| P2-07 | Readiness 与 drain | `/health/live`、`/health/ready`、优雅停机 | Redis/配置不可用时 readiness 准确，存量 stream 有界退出 |
| P2-08 | 指标体系 v2 | histogram、低基数标签、trace exemplar | tenant/task/request 不进入 metrics label；覆盖 TTFT、成本、配额和 fallback |
| P2-09 | SLO 与混沌测试 | 双实例、Redis 重启、上游超时、断流 | 生成自动化报告；阈值失败时 CI/发布门禁失败 |
| P2-10 | Redis 一致性矩阵 | budget/quota/binding/circuit/metrics 分别声明一致性与故障策略 | 硬预算、绑定 fail-closed；指标/延迟信号可 fail-open；故障测试符合矩阵 |
| P2-11 | 控制面 revision 协调 | 配置权威、签名、分发、readiness、回滚 | 多 Gateway 不混用 revision；失联时按 last-good 或 fail-closed 契约运行 |

### 8.1 预算账务生命周期

```text
request_id
  -> estimate(max_tokens, model price, retry ceiling)
  -> reserve(tenant budget, quota window)
  -> execute(attempt_id...)
  -> settle(actual usage or conservative estimate)
  -> release(unused reservation)
```

每个 attempt 单独记账，逻辑请求聚合展示。客户端取消、上游超时和 usage 缺失不能直接按零成本结算；应使用 provider 可验证 usage，或采用配置化保守估值并标记 `estimated=true`。重复提交相同 idempotency key 不得产生第二次预留。

### 8.2 Redis 一致性与故障语义

| 状态 | 一致性要求 | 默认故障语义 |
|---|---|---|
| 硬预算 | 强一致预留/结算 | fail-closed |
| RPM/TPM | 强一致，或显式配置有界超发 | fail-closed |
| SessionBinding | CAS 强一致 | fail-closed |
| 全局熔断/Half-Open | 原子状态转换 | 共享状态不可用时 fail-closed |
| 延迟/负载 EWMA | 最终一致 | fail-open，退化为 weighted |
| Metrics/trace | 最终一致 | fail-open，不阻塞请求 |

关键 SLO 初始门槛：

| 指标 | 初始门槛 |
|---|---:|
| 纯路由 P95 | <= 10 ms |
| 纯路由 P99 | <= 25 ms |
| Explain 5000 次错误率 | <= 0.1% |
| 非流式路由新增 RSS | <= 10 MiB/5000 请求 |
| 客户端取消至上游取消 P95 | <= 250 ms |
| 配额超发 | 0 |

退出门禁：本地 mock、双实例 Redis 和受控真实 provider 三层测试均通过；指标可以回答“为什么选它、为什么排除其他模型、花费多少、是否发生降级”。

## 9. 阶段 P3：Catalog 全生命周期与语义路由

**目标**：把现有 discovery 扩展成 provider 无关、可审计、可回滚的 Catalog 发布链，并在稳定记录契约上启用语义需求识别。预计 3-4 周。

```text
discover -> normalize -> enrich -> probe -> candidate -> review -> publish
                                                       -> reject/quarantine
```

| ID | 任务 | 主要内容 | 验收标准 |
|---|---|---|---|
| P3-01 | Provider plugin contract | pagination、auth plan、rate limit、raw evidence | SiliconFlow、百炼、火山使用同一 trait 和状态格式 |
| P3-02 | 事实补全 | reviewed overrides、字段级来源、汇率证据 | 未知价格/能力不会被猜测或进入 active route |
| P3-03 | 有预算的能力探测 | text/tools/json/reasoning probe | 探测有并发/费用上限，结果保留证据哈希和时间 |
| P3-04 | Candidate 生成与验证 | schema、冲突、退役、alias 检查 | candidate 不直接修改 active Catalog |
| P3-05 | 原子发布和回滚 | Catalog+manifest 一致切换、last-good | 进程崩溃不会留下半发布状态；可一键回滚 revision |
| P3-06 | Gateway 热加载 | ETag/revision、Arc snapshot swap | 单请求只看到一个 revision；失败保留 last-good |
| P3-07 | Catalog 管理面 | `/v1/catalog`、refresh/status 接口 | 沿用 RBAC/audit；refresh 不接受客户端提供密钥 |
| P3-08 | 凭据扩展 | OAuth、ambient credentials、轮换锁 | 不同 provider 可插拔；日志只显示 credential label |
| P3-09 | 模型退役流程 | stale/retired/grace period | 新请求停止选择，已有 session 按迁移策略处理 |
| P3-10 | 语义任务识别 v1 | 规则优先、轻量分类器可选、显式置信度 | “你好”默认 efficient；方程按推理需求升级；低置信度 abstain |
| P3-11 | 工具需求契约 | `requires_tools`、required capabilities、host disclosure | 天气请求只选择工具兼容模型；Gateway 不伪装成天气工具执行器 |
| P3-12 | Agent Host 联调 | tool availability 输入、tool-call continuation | 没有可用工具时明确拒绝/提示；有工具时由 Host 执行并继续会话 |

工具职责固定为：uRouter 只输出 `requires_tools` 和能力要求，Agent Host 提供并执行具体工具，模型生成 tool call。选择更大模型不能替代天气、搜索等实时工具。

退出门禁：三个 provider discovery contract tests 通过；至少一个非 SiliconFlow provider 完成受控真实发现；Catalog 发布/回滚/热加载故障注入通过；“你好、武汉天气、二元一次方程、无工具天气请求”四类路由和 Host 联调测试通过。

## 10. 阶段 P4：数据集、基准与离线评估

**目标**：先证明数据可信，再训练路由器。预计 3-4 周，另需至少一周真实流量观察。

| ID | 任务 | 主要内容 | 验收标准 |
|---|---|---|---|
| P4-01 | 记录质量门禁 | 校验 v2 完整度、revision、候选和 outcome | 不完整或跨 revision 的记录进入隔离区而非训练集 |
| P4-02 | 隐私与脱敏验证 | recording modes、redaction profile、consent | 默认不保存正文；导出前二次脱敏并有抽样审计 |
| P4-03 | 受控探索启用 | epsilon、propensity、eligible set | 每个探索样本可用于 IPS/SNIPS；tenant 显式授权且有费用上限 |
| P4-04 | 数据集导出 | tenant/task/time filter、manifest、hash | 删除代际生效；重复导出可复现 |
| P4-05 | 基准套件 | 日常问答、工具、数学、代码、长上下文 | 每类有质量、成本、延迟基线和支持域 |
| P4-06 | 反事实评估 | IPS、SNIPS、DR、置信区间 | 报告同时展示估计值、方差和有效样本量 |
| P4-07 | 支持域门禁 | OOD/低支持检测 | 不支持任务不得进入 learned policy canary |
| P4-08 | 反馈防投毒 | 限流、来源权重、异常检测、隔离 | 可疑反馈不进入默认训练集，审计可追踪 |

退出门禁：能够从一周真实流量生成版本化数据集；基准报告可以分开回答质量变化、降级节省、缓存节省和置信区间；不满足支持域的任务明确回退规则策略。

## 11. 阶段 P5：RouterArtifact、Shadow 与 Canary

**目标**：实现可验证、可回滚的学习路由，而不是直接替换规则路由。预计 3-4 周。

| ID | 任务 | 主要内容 | 验收标准 |
|---|---|---|---|
| P5-01 | Artifact schema | manifest、feature/catalog revision、hash、签名 | 不兼容 artifact 在加载前失败 |
| P5-02 | 训练与导出 | baseline、MLP/其他候选、ONNX | 同数据同 seed 可复现；导出满足七项门禁 |
| P5-03 | 在线 infer | bounded runtime、超时、fallback | 推理失败自动回到规则路由且有指标 |
| P5-04 | Active/Candidate 双槽 | 原子加载、last-good、kill switch | 坏 artifact 不影响 active；可立即禁用 |
| P5-05 | Shadow | 同请求双决策、只执行 active | 不增加上游模型调用；记录差异和估计成本 |
| P5-06 | Task Canary | tenant/task hash 稳定分桶 | 同任务不跨组；比例可调；有最小样本门槛 |
| P5-07 | 自动回滚 | 质量、错误、成本、延迟阈值 | 任一硬阈值触发回滚并留下审计记录 |
| P5-08 | 发布演练 | shadow -> 1% -> 5% -> 10% | 每阶段完成观察窗口和人工批准 |
| P5-09 | 有界 Step/Driver | Judge/Escalation 调用深度、递归保护、调用预算、取消 | 默认深度不超过 1；Judge 不会再次触发 Judge；失败回到规则策略 |

建议初始发布门禁：质量置信区间下界不低于规则基线容忍值；总成本改善达到预设目标；错误率和 P95 延迟无显著恶化；unsupported/OOD 流量不进入 canary。

退出门禁：完成一次坏 artifact 加载、一轮异常 canary 和一次人工 kill switch 的回滚演练；所有演练均不影响规则基线可用性。

## 12. 阶段 P6：协议扩展与嵌入形态

**目标**：在核心契约稳定后扩展协议生态，并交付同核嵌入模式。预计 2-3 周，后续持续迭代。

| ID | 任务 | 主要内容 | 验收标准 |
|---|---|---|---|
| P6-01 | 规范化消息 IR | tool、reasoning、usage、finish reason | OpenAI round-trip 测试无语义损失 |
| P6-02 | Provider transport | OpenAI-compatible、百炼/方舟差异 adapter | compat 不支持时准入拒绝，不在运行中猜测 |
| P6-03 | 跨 provider handoff | 消息降级规则与 loss report | 每次丢失/降级均可观测并受策略控制 |
| P6-04 | 新协议入口 | Responses/Anthropic 按需求选择 | 与 Chat 共用规范化 IR 和决策核 |
| P6-05 | `urouter-client` | 重试、地址列表、Gateway failover | 不重试不可安全重放的流式部分失败 |
| P6-06 | `urouter-embed` | 纯决策 facade、构造器和示例 | 同输入、同 artifact 与 Gateway tier 决策逐条一致 |
| P6-07 | 发布与 semver | API review、迁移指南、示例 | crate 可独立发布，公共类型有兼容策略 |

退出门禁：Gateway 与 Embed 使用同一份 parity corpus 和 artifact，tier 决策 100% 一致；协议降级有自动化契约测试。

## 13. 横切安全与运营任务

以下任务不等待单独里程碑，应在相关功能进入主分支时同步完成：

| ID | 任务 | 最晚完成阶段 |
|---|---|---|
| X-01 | Gateway TLS 或由入口代理强制 TLS，并记录部署契约 | P2 |
| X-02 | Redis ACL/TLS/AOF/HA 故障演练 | P2 |
| X-03 | 管理面与数据面独立监听/网络策略 | P2 |
| X-04 | tenant 级 rate limit、quota、budget | P2 |
| X-05 | 数据导出、删除和审计闭环 | P4 |
| X-06 | provider key/OAuth 轮换演练 | P3 |
| X-07 | 指标标签基数预算与敏感信息审查 | 每阶段 |
| X-08 | SBOM、依赖漏洞、license 和 secret 扫描 | P0 起持续 |
| X-09 | 指标标签基数和 trace/log 保留策略 | P2 |
| X-10 | 控制面配置签名、revision 权威和分发可用性 | P2 |
| X-11 | Linux/Windows 双平台 CI 与文件轮转/停机测试 | P0 起持续 |

## 14. 首个两周可直接执行的任务包

这是下一轮实施应直接领取的顺序，不需要等待 P0 全部文档写完才编码。

### 第 1 周：建立可信基线

| 顺序 | 任务 | 预计 | 产出 |
|---:|---|---:|---|
| 1 | P0-04 密钥轮换与 secret scan | 0.5 天 | 不含真实密钥的验证环境 |
| 2 | P0-01 需求追踪矩阵骨架 | 1 天 | 所有设计条目映射到任务 ID |
| 3 | P0-02 HTTP 行为基线 | 1.5 天 | mock upstream 集成测试 |
| 4 | P0-05 CI 门禁补齐 | 1 天 | Linux/Windows release test、audit、secret scan |
| 5 | P0-07/P0-08 迁移与幂等 ADR | 1 天 | schema 共存、request/attempt identity 契约 |

### 第 2 周：完成 P0 门禁并冻结契约输入

| 顺序 | 任务 | 预计 | 产出 |
|---:|---|---:|---|
| 1 | P0-09 威胁模型与信任边界 | 1 天 | client/proxy/Gateway/Redis/provider 边界 |
| 2 | P0-10 API 契约基线 | 1.5 天 | OpenAPI/JSON Schema、错误 envelope |
| 3 | P0-05 Linux/Windows CI 门禁 | 1 天 | 双平台、audit、secret scan |
| 4 | P0-03 文档和状态口径更新 | 0.5 天 | 架构图、README、状态矩阵一致 |
| 5 | P0 退出评审及 P1 重估 | 1 天 | 门禁证据、风险清单、P1 任务拆分 |

P0 通过后再开始 P1。第一个 P1 合并请求只包含 FeatureFrame、Trace 和 DecisionRecord v2 契约；第二个建立 parity/property 测试；第三个才迁移第一个过滤器。这样任一变更都可以独立回滚。

## 15. 发布流水线

```text
提交
  -> fmt/clippy/unit/doc
  -> schema/catalog/secret/dependency gates
  -> API compatibility + migration tests
  -> mock HTTP integration
  -> Redis contract + two-instance tests
  -> release benchmark + soak
  -> controlled paid-provider smoke
  -> shadow
  -> canary
  -> production
```

环境分级：

| 环境 | 允许数据 | 允许调用 | 用途 |
|---|---|---|---|
| CI | 合成数据 | mock upstream | 确定性正确性 |
| Integration | 脱敏合成数据 | 本地模型、Redis | 多实例和故障测试 |
| Staging | 授权脱敏流量 | 商用 provider 小预算 | 协议、计费、限额验证 |
| Production | 按 tenant policy | approved deployments | shadow/canary/active |

## 16. 风险与止损条件

| 风险 | 预防 | 立即停止条件 |
|---|---|---|
| 大规模重构破坏 Gateway | parity corpus、逐函数迁移 | 核心端点出现未解释的响应变化 |
| Redis 配额竞态 | Lua/事务、故障注入 | 任何租户出现配额超发 |
| 预算账务不平 | 预授权/结算/释放、幂等 attempt ledger | 重试、取消或 usage 缺失导致重复扣费/未释放 |
| 语义路由误升/误降 | abstain、规则优先、shadow | 关键任务质量显著低于基线 |
| Catalog 错误污染在线流量 | candidate/review/atomic publish | 价格或能力证据缺失仍进入 active |
| 学习数据有偏 | propensity、DR、支持域 | 无有效样本量仍宣称质量提升 |
| 指标泄漏 tenant/正文 | 标签白名单、审查 | 指标或日志出现敏感内容 |
| Provider 费用失控 | probe budget、hard cap | 单日探测达到预算上限 |
| 协议自动降级丢语义 | loss report、准入拒绝 | tool/schema 被静默丢弃 |
| Schema 滚动升级失败 | 双读/双写、版本门禁、回滚演练 | 新旧 Gateway 无法同时读取共享状态 |
| 工具职责越界 | requires_tools 契约、Host 联调 | Gateway 在没有真实工具结果时声称完成实时查询 |

## 17. 状态更新规则

每完成一个任务，需求追踪矩阵必须记录：

```text
task_id
status: planned | in_progress | blocked | done
owner
design_reference
code_reference
test_reference
evidence
remaining_risk
completed_at
```

里程碑只能使用以下状态：

- `planned`：尚未开始。
- `in_progress`：已有负责人和进行中的变更。
- `blocked`：存在明确外部依赖，已记录解除条件。
- `code_complete`：代码门禁完成，但真实流量或生产演练未完成。
- `accepted`：所有退出门禁和外部验证完成。

禁止仅根据提交数量或代码存在就把里程碑标记为 100%。

## 18. 总体验收清单

- [ ] 原始设计要求均进入需求追踪矩阵，没有“文档存在但无人执行”的条目。
- [ ] 纯决策内核零 I/O，并由 CI 强制依赖边界。
- [ ] Feature、Cascade、Filter、Picker、Execution trace 可完整解释一次路由。
- [ ] DecisionRecord v2 在采集训练流量前上线，包含候选、propensity 和全部 revision。
- [ ] Redis/API/记录 schema 支持新旧版本滚动升级和可验证回滚。
- [ ] request/decision/attempt/provider ID 可唯一关联重试、费用和反馈。
- [ ] RPM/TPM、并发、预算、租户策略在多实例下正确执行。
- [ ] 预算使用 estimate/reserve/settle/release，取消、重试和 usage 缺失账务可对平。
- [ ] Catalog 支持三个 provider 的发现、补全、探测、审核、发布、回滚和热加载。
- [ ] Gateway 具备 liveness、readiness、drain、SLO 指标和故障演练证据。
- [ ] Prometheus 标签为受控低基数，tenant/task/request 只进入受保护 trace/log。
- [ ] 数据集导出、删除、脱敏、propensity、离线评估和支持域门禁闭环。
- [ ] RouterArtifact 可复现、可验证、可 shadow、可 canary、可自动和人工回滚。
- [ ] 跨 provider/协议转换不会静默丢失工具或结构化输出语义。
- [ ] uRouter 只声明工具需求，天气等外部工具由 Agent Host 执行并有端到端契约测试。
- [ ] Gateway 和 Embed 使用同一决策核并通过逐条一致性测试。
- [ ] 所有真实密钥均由受控凭据来源注入，仓库和日志中无明文。

## 19. 与现有文档的关系

本文不替代详细设计，而是作为执行入口：

- `uRouter_设计文档.md`：目标架构和完整需求来源。
- `uRouter_技术架构深度评审与优化方案.md`：风险、优先级和收敛建议来源。
- `uRouter_技术架构流程图.md`：运行流程图，需在 P0-03 更新为当前状态。
- `uRouter_设计文档执行状态与完成预测.md`：旧状态快照，后续由需求追踪矩阵取代。
- `uRouter_多模型提供商扩展方案.md`：P3 的 provider 插件和发布设计。
- `uRouter_SiliconFlow模型同步方案.md`：P3 的首个 provider 生命周期样板。
- `docs/gateway-m0.md`：当前 Gateway 外部契约与回归基线。

下一次实施应从 **P0-04、P0-01、P0-02、P0-07、P0-08** 开始；在 P1 的 DecisionRecord v2 和 revision 契约上线前，不把新流量声明为可训练数据；在 P0 退出评审完成前，不启动 RouterArtifact 或 learned routing。
