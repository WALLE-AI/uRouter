# uRouter 技术架构流程图

uRouter 是一个使用 Rust 实现的、面向 Agent 的多模型智能路由网关。核心目标是根据请求能力、任务上下文、成本与质量偏好选择模型，并提供会话绑定、故障转移、治理审计和可观测性。

## 技术架构流程图

```mermaid
flowchart TD
    Client["Agent / OpenAI SDK / 普通客户端"]
    Adapter["AionUI / WorkBuddy 适配器<br/>Header 转 uRouter v2 合约"]
    API["Axum HTTP Gateway<br/>Chat / Explain / Models / Management"]

    Client --> API
    Client --> Adapter --> API

    subgraph Request["请求治理与路由决策"]
        Tenant["租户解析<br/>x-urouter-tenant-id"]
        Contract["解析 uRouter 合约<br/>任务、会话、调用角色、数据策略"]
        Requirement["请求能力分析<br/>文本 / 图片 / 音频 / Tools<br/>结构化输出 / Reasoning / Context"]
        Admission["能力准入<br/>过滤不支持请求的模型"]
        Tier["确定性分层选择<br/>显式模型 / Tier Pin / Quality Guard<br/>Cost Preference / Capability Required"]
        Binding["任务或会话绑定<br/>固定 Model + Provider + API<br/>Prompt / Toolset Identity"]

        Tenant --> Contract --> Requirement --> Admission --> Tier --> Binding
    end

    API --> Tenant

    Catalog["catalog/catalog.json<br/>Provider、Model、Capability<br/>Pricing、Compat、Endpoint、Auth"]
    Route["gateway/route.json<br/>efficient / capable<br/>Deployment / Fallback"]
    Catalog --> Admission
    Catalog --> Tier
    Route --> Tier

    subgraph Execution["上游执行与容错"]
        Fallback["构造 Tier Fallback 链"]
        Capacity["进程内容量管理<br/>Order + Weight + In-flight"]
        Circuit["熔断与 Half-Open 探测<br/>Deployment / Credential / Provider"]
        Endpoint["Endpoint 与认证规划"]
        Rewrite["请求兼容性改写<br/>移除 urouter<br/>替换 upstream model ID<br/>Role / max_tokens 归一化"]
        Upstream["OpenAI-compatible Chat Endpoint<br/>本地 vLLM 或 Provider"]
        Retry{"执行成功？"}
        Reselect["错误分类、退避<br/>重新选择 Deployment"]
        NextTier["进入 Fallback Tier"]

        Binding --> Fallback --> Capacity --> Circuit --> Endpoint --> Rewrite --> Upstream
        Upstream --> Retry
        Retry -- "可重试错误" --> Reselect --> Capacity
        Retry -- "Tier 耗尽" --> NextTier --> Capacity
    end

    subgraph Result["响应、计费与可观测性"]
        Usage["解析 Usage"]
        Pricing["精确成本计算<br/>Nano-USD 定点计价"]
        Commit["成功后提交任务/会话绑定<br/>First-success-wins"]
        Record["DecisionRecord<br/>模型、原因、尝试、延迟、成本、Evidence"]
        Disclosure["路由结果披露<br/>响应 Header / JSON / SSE Event"]
        Metrics["Prometheus Metrics"]
        Response["客户端响应"]

        Retry -- "成功" --> Usage --> Pricing --> Commit --> Record --> Disclosure --> Response
        Retry -- "最终失败" --> Record
        Record --> Metrics
    end

    subgraph State["状态与治理后端"]
        Memory["单实例模式<br/>内存 Binding / Circuit / Record / Feedback<br/>可选异步 JSONL"]
        Redis["多实例模式<br/>Redis 权威状态<br/>Binding / Circuit / Record / Feedback<br/>TTL + CAS + 原子删除"]
        RBAC["管理面 RBAC<br/>Bearer Keyring 热加载<br/>同步审计 JSONL"]
    end

    Binding <--> Memory
    Binding <--> Redis
    Circuit <--> Memory
    Circuit <--> Redis
    Record --> Memory
    Record --> Redis
    RBAC --> API
```

## 核心模块关系

```mermaid
flowchart LR
    Types["urouter-types<br/>ID、WireApi、Usage、Money"]
    AI["urouter-ai<br/>Catalog、Capability、Admission<br/>Pricing、Endpoint、Auth、Evidence"]
    Gateway["urouter-gateway<br/>HTTP、Routing、Binding、Circuit<br/>Retry、Fallback、Governance"]
    CatalogCLI["urouter-catalog<br/>目录检查、查询、成本、Diff"]
    Smoke["urouter-smoke<br/>真实端点能力验证"]
    Soak["urouter-soak<br/>并发与稳定性压测"]

    Types --> AI --> Gateway
    AI --> CatalogCLI
    AI --> Smoke
    Gateway --> Soak
```

## 当前实际路由

| Tier | 模型 | 主要能力 | 故障转移 |
|---|---|---|---|
| `efficient` | `local-vllm/qwen3.5-4b` | 文本、结构化输出 | `capable` |
| `capable` | `local-vllm-qwen38/qwen3.8-27b` | 文本、图片、Tools、Reasoning、大上下文 | 无 |

## 关键架构行为

- `urouter/auto` 先执行能力过滤，再根据难度、调用角色和偏好选择 Tier。
- 显式模型请求绕过 Auto 分层，但仍执行能力准入。
- Primary 调用可以绑定到精确模型；Auxiliary 调用绕过主任务绑定并偏向低成本 Tier。
- v2 合约使用 `conversation + branch` 维持会话连续性，并校验 Prompt、Toolset 身份。
- 429、5xx、超时和传输错误可以重试；确定性 4xx 不重试。
- 非流式响应在 JSON 中加入路由披露；流式响应在 `[DONE]` 前追加 `urouter.decision` SSE 事件。
- 启用 Redis 后，Redis 是多实例 Binding、Circuit、DecisionRecord 和 Feedback 的唯一权威状态；Redis 不可用时路由状态操作返回稳定的 503。
- 当前网关执行层仅支持 `open_ai_chat`。目录虽然可以描述 Anthropic Messages 和 OpenAI Responses，但尚未实现跨协议转换。

## 主要实现位置

- 网关入口与请求执行：`crates/urouter-gateway/src/main.rs`
- 路由配置与决策算法：`crates/urouter-gateway/src/lib.rs`
- 模型事实与目录快照：`crates/urouter-ai/src/catalog.rs`
- 公共类型与计价类型：`crates/urouter-types/src/`
- 当前 Auto 路由配置：`gateway/route.json`
- 模型与 Provider 目录：`catalog/catalog.json`

## 验证说明

本架构图依据当前代码、配置和已有测试定义进行静态分析。尝试运行 workspace 测试时，本机未缓存 `axum`，且环境无法连接 crates.io，因此未完成编译验证。
