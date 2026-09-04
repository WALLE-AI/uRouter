# uRouter 智能路由 / 智能体路由评测方案

> 制定日期：2026-09-04
> 目的：验证 [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) 是否达成第一性原理——**在保证质量的前提下用最优模型，而不是一直用最贵的**
> 依据：对现有 `eval/` 资产与 `urouter-lab` 能力逐条核对，方案接在其上而非另起

---

## 零、先明确路由评测与模型评测的根本差异

**不能用评测模型的方式评测路由器。**

模型评测问"这个回答好不好"，有参考答案；路由评测问"这个档位选对了吗"，而"对"的定义是一个**反事实**——换个档位会怎样，而这在同一次请求上观察不到。

由此产生四个必须正面处理的困难：

| 困难 | 后果 | 本方案的处理 |
|---|---|---|
| 真值是反事实 | 无法直接标注"应该用哪档" | L3 用 IPS/SNIPS/DR + ESS；L2 用配对实验强制观测两侧 |
| 质量随任务而变 | 没有单一质量指标 | 按类别分别定义可自动判定的成功判据 |
| 智能体轨迹路径依赖 | 第 3 轮选错会污染第 4–20 轮 | L4 以**任务**为评测单元，而非以轮次 |
| 系统自适应 | 升级、黏性下限使请求间不独立 | 全部指标按任务聚合；单轮指标仅作诊断 |

**核心原则：任何单一层都不足以判定成败，必须四层证据同向。**

---

## 一、现有资产（复用，不重建）

逐条核实过，以下已存在：

| 资产 | 位置 | 用途 |
|---|---|---|
| 套件/运行/汇总三段格式 | `eval/suites/*.json`、`eval/real/*-{cases,run,summary}.json` | 直接沿用 |
| 五类别真实运行基线 | `eval/real/siliconflow-five-category-*` | L2 的历史基线 |
| 长上下文套件 | `eval/real/siliconflow-long-context-*` | 缓存/长文场景 |
| 网关基准 | `urouter-lab gateway-benchmark --suite --max-cost-nano-usd` | L2 执行器，**自带成本上限** |
| 反事实估计 | `urouter-lab counterfactual` / `urouter-eval:568` | L3 |
| 参数扫描 | `urouter-lab sweep --candidates` | L3 阈值搜索 |
| 离线重放 | `urouter-lab replay --artifact --dataset --maximum-p99-micros` | L1 决策重放 + 延迟门禁 |
| 数据集构建与隔离 | `urouter-lab dataset` / `build_dataset` | 隐私/留存/删除治理 |
| 支持域门禁 | `urouter-eval::SupportDomain` | 防止在样本不足处下结论 |
| 单端点能力验证 | `urouter-smoke --model` | L0 前置 |

已有 summary 字段（`mean_quality_millionths`、`total_cost_nano_usd`、`p50/p95_latency_ms`、`category_summaries`）**恰好就是本方案需要的三元组**，沿用即可。

---

## 二、四层评测体系

```
L0  端点体检        每个档位真的活着、能力属实          smoke        分钟级
L1  决策评测        不调上游，只验路由选择是否符合预期   replay       秒级，CI 每次
L2  端到端配对      同一请求在多个档位各跑一次，比较     benchmark    小时级，每次发布
L3  反事实评估      在真实流量上估计"换策略会怎样"       counterfactual  离线，周级
L4  智能体轨迹      整任务成败与总成本                  新增          发布前
```

**L1 是主力**——它零成本、零延迟、可进 CI，能覆盖绝大多数路由回归。L2/L3/L4 分别补上 L1 看不到的东西。

---

### L0 端点体检（前置门禁）

评测路由之前必须确认档位本身可用，否则测的是上游可用性而不是路由。

```bash
for m in $(urouter-catalog list catalog/catalog.json); do
  urouter-smoke --model "$m" --json
done
```

**门禁**：参与本轮评测的每个档位至少一个部署通过；不通过的档位从套件中显式排除并在报告中列出——**不能静默跳过**，否则"没测到"会被读成"测过了"。

---

### L1 决策评测：不调上游，只验选择

`POST /v1/explain` 只做决策不调上游，因此可以用极低成本覆盖大量用例。

**套件格式**（扩展现有 `eval/suites/*.json`，向后兼容）：

```json
{
  "schema_version": 2,
  "cases": [
    {
      "id": "hard-hint-no-longer-jumps-to-top",
      "category": "cost",
      "request": {
        "model": "urouter/auto",
        "messages": [{"role": "user", "content": "写一个快排"}],
        "urouter": {"hint": {"difficulty": "hard"}}
      },
      "expect": {
        "tier_not": "frontier",
        "tier_at_least": "mid",
        "reason": "cheapest_above_floor",
        "quality_floor_source": "caller_hint"
      }
    }
  ]
}
```

`expect` 支持 `tier` / `tier_at_least` / `tier_not` / `reason` / `quality_floor` / `quality_floor_source` / `admission_excludes`。

**必须覆盖的用例族**（每族对应方案中的一条断言）：

| 族 | 断言 | 对应 |
|---|---|---|
| A 中间档位可选中 | 3+ tier 时能选到中间 | 诊断 1.1 |
| B `hard` 不再锁最贵 | 抬高 floor，其上仍选最便宜 | 1.2 / 2.5 |
| C 长上下文不抬档 | 20 万 token 抽取任务不跳最贵 | 1.5 / 2.1 |
| D 成本进入比较 | 同能力下选更便宜者 | 1.3 |
| E 缓存边际成本 | 缓存命中的贵档优于未缓存的便宜档 | 1.6 / 1.1 |
| F 轨迹信号抬档 | 连续工具报错/循环/用户纠正 → 抬高 | 2.2 |
| G 黏性下限 | 同任务后续轮不回落 | 2.3 |
| H 副调用降档 | `role=auxiliary` 落在更低档 | 1.7 / 3.4 |
| I 硬准入不可绕过 | 缺能力的模型永不被选 | 2.0 ① |
| J 迁移边界 | 工具循环中途拒绝换模型 | 3.3 |
| K token 估算一致 | 中英文两侧估算差 < 20% | 1.8 / L0 |

**执行与门禁**

```bash
urouter-lab benchmark --cases eval/suites/routing-decision-v2.json \
                      --output eval/real/routing-decision-run.json
```

- 全部用例通过；任一失败即阻断发布
- **进 CI**（`.github/workflows/ci.yml`），与现有 `catalog semantic diff` 同级
- 新增一条 PR 门禁：`/v1/explain` 的决策在同一 revision 下必须逐字节可重放（保护决策确定性）

---

### L2 端到端配对实验：强制观测两侧

L1 只能验"选了哪档"，验不了"选对了吗"。L2 用**配对**解决反事实观测问题：**同一请求在多个档位各跑一次**。

```bash
for tier in cheap mid frontier; do
  urouter-lab gateway-benchmark \
    --suite eval/suites/quality-cost-pairs.json \
    --base-url http://127.0.0.1:8787/v1 \
    --cases-output  eval/real/pairs-$tier-cases.json \
    --report-output eval/real/pairs-$tier-run.json \
    --max-cost-nano-usd 50000000
done
```

（各档通过 `urouter.preference.pin_tier` 强制固定，这正是 `pin_tier` 保留强制语义的用途。）

**质量判据必须可自动判定**——人工评分不可重复、不可进 CI。按类别定义：

| 类别 | 判据 | 类型 |
|---|---|---|
| 工具调用 | 是否发出预期工具、参数是否符合 schema | 确定性 |
| 结构化输出 | 是否满足 `response_format` schema | 确定性 |
| 数学/方程 | 最终数值是否正确 | 确定性 |
| 代码 | 能否通过随附单测 | 确定性 |
| 长上下文抽取 | 是否命中埋入的事实（needle） | 确定性 |
| 日常对话 | 非空、非拒答、语言正确 | 弱判据，仅作下限 |

**刻意不设开放式生成类别**——它无法自动判定，混进来只会让整套指标不可信。开放式质量交给 L3 的线上反馈信号。

**输出**：每个类别一张 `(档位 × 质量 × 成本 × 延迟)` 表。**这张表是所有 floor 配置的依据**——`quality_floors.by_task` 的每一条都应能在这张表里指出出处。

**门禁**：对每个类别，找出"质量与最高档差距 ≤ ε 的最便宜档位"，它就是该类别的推荐 floor。若推荐 floor 恒等于最高档，说明**该类别不存在优化空间**，应从优化范围中移除而不是硬压。

**L2 同时是任务意图模块的决策点。** 类别应按 [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) 的候选标签集划分（`conversation` / `code_write` / `code_explain` / `data_extract` / `translate` / `summarize` / `plan` / `math` / `tool_use`），因为：

- 这张表就是 `by_task` 的数据来源，类别口径必须与标签口径一致
- **若各标签的推荐 floor 全都相同，说明区分任务类型对路由没有信息量**——此时应停止建设意图分类器（该模块的 I0 决策点），`by_task` 留空，floor 由其余三个来源决定

换言之：**先用 L2 测出各类任务的最优档位确实不同，再投入建分类器**。反过来做（先建分类器再找用途）是常见的浪费。

---

### L3 线上反事实评估：在真实流量上回答"换策略会怎样"

L1/L2 用的是构造用例，覆盖不了真实流量分布。L3 直接在生产流量上估计。

**前提（缺一不可）**

1. 探索开启且 propensity 可复算——现有实现是哈希派生的确定性探索，满足
2. `recording != none` 且 `allow_training`
3. 通过 `SupportDomain` 门禁

```bash
urouter-lab dataset        --output eval/real/traffic-dataset.json     # 隐私/留存/删除治理
urouter-lab counterfactual --samples eval/real/traffic-dataset.json \
                           --output eval/real/counterfactual-report.json
```

**要回答的问题**

- 把 `math` 的 floor 从 `capable` 降到 `mid`，质量降多少、成本降多少？
- 关掉"长上下文抬档"，质量指标是否变化？（若无变化，则证实诊断 1.5）
- 副调用全部降到最低档，主线质量是否受影响？（应无影响——副调用不进历史）

**门禁**：任何 floor 下调必须满足 `ESS ≥ 阈值` 且 IPS/SNIPS/DR **三个估计量同向**。三者分歧说明权重方差过大，结论不可用——**此时的正确动作是继续采样，不是取平均**。

---

### L4 智能体轨迹评测（本方案最独特的一层）

前三层都是**逐请求**的。智能体的成败是**逐任务**的：第 3 轮一个错的工具调用，会让 4–20 轮全部作废。逐请求指标完全看不到这个。

**评测单元 = 一个完整任务，而不是一次请求。**

**任务套件**（新增 `eval/suites/agent-trajectory.json`）：

```json
{
  "schema_version": 1,
  "tasks": [
    {
      "id": "multi-step-file-edit",
      "harness": "aionui",
      "tools": ["read_file", "write_file", "list_dir"],
      "goal": "在 src/lib.rs 中把函数 foo 重命名为 bar 并更新所有调用点",
      "fixture": "eval/fixtures/rename-repo/",
      "success": {
        "kind": "deterministic",
        "check": "grep -c 'fn bar' src/lib.rs == 1 && grep -c 'foo' src/ == 0"
      },
      "max_turns": 20
    }
  ]
}
```

**四个必测指标**（前两个是主指标）

| 指标 | 说明 | 为什么 |
|---|---|---|
| **任务成功率** | 确定性判据 | 唯一真正重要的质量指标 |
| **每任务总成本** | 全轮次成本之和 | 单轮便宜但多花 10 轮，是净亏 |
| 平均轮次数 | 完成任务用了几轮 | 选错档位的放大效应在这里显形 |
| 主线切换次数 | 任务内主线模型变更次数 | 应接近 0；抖动比一直用贵的更糟 |

**必须做的三组对照**

```
基线 A：全程最贵档            → 质量上界与成本上界
基线 B：全程最便宜档          → 成本下界，暴露"便宜的代价"
实验 C：本方案（floor+升级+副调用降档）
```

**判定标准**：

```
C 的任务成功率 ≥ A 的成功率 − ε   且   C 的每任务成本 < A 的成本
```

若 C 成本低于 A 但成功率也显著低于 A，**方案未达成第一性原理**——那是在牺牲质量换成本，不是优化。

**必须包含的轨迹形态**（对应 Layer 2.2 的每条判据）：

| 形态 | 验证 |
|---|---|
| 全程顺利，无工具错误 | 应全程停在低档（省钱路径） |
| 中途连续工具报错 | 应抬档并最终完成 |
| 工具调用循环 | 应被判据捕获并抬档 |
| 用户中途纠正 | 应抬档 |
| 上下文触发压缩 | 应抬档 |
| 大量副调用 | 副调用应显著低于主线档位 |

**同时必须验证不该发生的**：顺利轨迹**不应**发生任何升级——若发生，说明验证器或轨迹判据过敏，会系统性推高成本。

---

## 三、指标定义（统一口径）

所有层共用同一组定义，避免各层结论无法拼接：

```
质量（逐任务）  = 确定性判据通过率
质量（逐请求）  = 验证器 Sufficient 率（弱代理，仅诊断）
成本            = Σ 实际 usage × 目录费率（nano-USD，非估算值）
延迟            = 端到端 P50 / P90 / P99
升级率          = 发生升级的请求 / 总请求，**按 agent 与单轮分别统计**
主线切换率      = 任务内主线模型变更次数
副调用比与档位差 = auxiliary 占比、其平均档位与主线的差
```

**成本一律用实际 usage 结算值，不用估算值。** 估算值是被评测对象（P0-1），拿它做评测口径等于自证。

---

## 四、验收标准

### 必须达成（不达成即方案未成立）

| # | 标准 | 层 |
|---|---|---|
| 1 | L1 全部用例通过，且进 CI | L1 |
| 2 | 智能体任务成功率相对"全程最贵档"下降 ≤ ε（建议 ε = 2%） | L4 |
| 3 | 每任务总成本显著低于"全程最贵档" | L4 |
| 4 | 主线切换次数中位数 = 0 | L4 |
| 5 | 顺利轨迹的升级率 ≈ 0 | L4 |
| 6 | 任何 floor 下调都有 ESS 达标 + 三估计量同向的支撑 | L3 |
| 7 | P90 延迟劣化 ≤ 可接受阈值 | L2/L4 |
| 8 | 预算超支次数 = 0 | 全部 |

### 诊断（不达标不阻断，但需解释）

- 升级率落在 2%–20%
- 副调用平均档位显著低于主线
- 每条轨迹判据都有非零命中（长期零命中的判据应删除而非放宽）
- 切换到更便宜档后成本确实下降（否则是缓存边际成本算错）

---

## 五、执行节奏

| 频率 | 内容 | 阻断 |
|---|---|---|
| 每次 PR | L1 决策评测 + 决策可重放 | 是 |
| 每次发布 | L0 体检 → L2 配对 → L4 轨迹（缩减集） | 是 |
| 每周 | L3 反事实 + floor 调优建议 | 否 |
| floor 变更时 | L3 支持域门禁 + L4 全量 | 是 |

**canary 与既有机制一致**：shadow → 1% → 5% → 10%，护栏指标越界即 `last-good` 回滚。

---

## 六、评测本身的陷阱

写下来是因为这些都很容易犯：

| 陷阱 | 后果 | 规避 |
|---|---|---|
| 用估算成本做评测口径 | 自证，掩盖 P0-1 | 一律用实际 usage 结算 |
| 只看逐请求指标 | 看不到轨迹污染，会得出"便宜档很好"的错误结论 | L4 以任务为单元 |
| 开放式生成进自动评测 | 判据不可靠，污染全部结论 | 只用确定性判据 |
| 套件里没有"顺利轨迹" | 只测异常路径，测不出过敏升级 | 必含顺利形态 |
| 缺 L0 体检 | 把上游不可用读成路由错误 | L0 为前置门禁 |
| ESS 不足就下结论 | 反事实估计在稀疏区域方差极大 | 支持域门禁强制 |
| 每档只跑一次 | 采样噪声被当成档位差异 | `repeats` ≥ 3，报告方差 |
| 只与"最便宜"比 | 会得出"当然更贵更好"的空结论 | 必须同时与"全程最贵"比 |

---

## 七、先做哪一步

**L1 + L4 的顺利轨迹用例，优先于其余一切。**

- L1 零成本、可进 CI，能挡住绝大多数路由回归
- L4 的顺利轨迹用例验证的是"不该升级时不升级"——这是**方案自伤风险最高的地方**（过敏升级会同时推高成本与延迟，且不会被任何质量指标发现）

L2/L3 需要真实凭据与流量，可在池子扩充后启动。

---

## 相关文档

- [`uRouter_Auto模式成本最优化技术方案.md`](uRouter_Auto模式成本最优化技术方案.md) — 被评测的方案
- [`uRouter_任务意图识别模块设计.md`](uRouter_任务意图识别模块设计.md) — L2 类别口径与 I0 决策点
- [`uRouter_技术架构评审方案.md`](uRouter_技术架构评审方案.md) — P0-1 成本估算前置
- `eval/suites/`、`eval/real/` — 现有套件与运行记录
- `crates/urouter-eval/src/lib.rs` — 反事实估计量与支持域
- `tools/urouter-lab/` — replay / counterfactual / sweep / benchmark
