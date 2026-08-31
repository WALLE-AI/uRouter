# uRouter SiliconFlow 真实模型路由验证报告

> 验证日期：2026-08-30  
> Provider：SiliconFlow  
> SDK：OpenAI Node.js SDK（本机未安装 Python）  
> Gateway：`http://127.0.0.1:8791/v1`，测试结束后关闭  
> 凭据：仅通过进程环境注入，未写入仓库、日志或报告

## 结论

真实模型路由、OpenAI SDK 兼容、流式响应、推理升级、工具调用和工具结果
continuation 均通过。简单问候实际使用免费 Qwen 7B；方程和实时天气工具链
实际使用 Pro DeepSeek-R1。天气请求未声明 Host 工具时，在调用上游前稳定拒绝。

## 测试结果

| 场景 | 实际模型 | 路由 | Usage | 上游时延 | Catalog 成本 |
|---|---|---|---:|---:|---:|
| SiliconFlow SDK 直连市场问题，stream | `Pro/deepseek-ai/DeepSeek-R1` | 基线 | 未请求 stream usage | - | Provider 账单 |
| `hello`，非流式 | `Qwen/Qwen2.5-7B-Instruct` | efficient / `default_efficient` | 30 in / 14 out | 1761 ms | $0 |
| 二元方程 | `Pro/deepseek-ai/DeepSeek-R1` | capable / `capability_required` | 21 in / 518 out | 21275 ms | $0.001234608 |
| 武汉天气，生成 tool call | `Pro/deepseek-ai/DeepSeek-R1` | capable / `capability_required` | 102 in / 61 out | 3227 ms | $0.000204097 |
| 注入真实天气结果并 continuation | `Pro/deepseek-ai/DeepSeek-R1` | capable / `capability_required` | 173 in / 133 out | 6361 ms | $0.000415862 |
| `hello`，stream | `Qwen/Qwen2.5-7B-Instruct` | efficient / `default_efficient` | 30 in / 14 out | 483 ms | $0 |

三次网关 DeepSeek 调用披露成本合计 `$0.001854567`，按 Catalog 中的
汇率约为人民币 `¥0.0126`。SDK 直连基线未请求流式 Usage，因此不纳入该合计。

## 机器可读评测基线

上述 5 次真实网关模型调用已转换为 `urouter-lab benchmark` 输入：

- 输入：`eval/real/siliconflow-2026-08-30-cases.json`
- 汇总：`eval/real/siliconflow-2026-08-30-summary.json`
- 任务成功：5/5
- 平均人工复核质量：1,000,000 millionths（本批全部满足预期）
- 平均上游延迟：6,621.4 ms
- 上游延迟 P50/P95：3,227 ms / 21,275 ms
- 总成本：1,854,567 nano-USD（`$0.001854567`）
- 类别覆盖：daily 2、tool 2、math 1

分类延迟为：daily 平均 1,122 ms（P95 1,761 ms），tool 平均 4,794 ms
（P95 6,361 ms），math 当前唯一实例为 21,275 ms。数学请求构成当前最明显的
延迟长尾，需要在补充重复样本后评估 balanced 模型或更细粒度升级策略。

质量值是人工复核的二值任务成功分，不是 Judge 模型评分。当前没有 code、
long-context、重复运行、随机探索或基线策略对照，因此该结果只作为首个真实
E2E 基线，不能用于 learned artifact 发布或宣称统计显著性。

## 五类真实评测与策略修复

2026-08-30 使用 `urouter-lab gateway-benchmark` 对 daily、tool、math、code、
long-context 各执行 1 个真实 Gateway 请求。原始策略结果为 4/5：

- greeting：Qwen 7B / efficient，通过，1,812 ms，成本 0。
- weather tool：DeepSeek-R1 / capable，通过，5,697 ms，`$0.000301427`。
- equation：DeepSeek-R1 / capable，通过，21,821 ms，`$0.001180340`。
- Rust code：Qwen 7B / efficient，通过，1,250 ms，成本 0。
- long-context：Qwen 7B / efficient，HTTP 200 但仅输出 1 token，质量失败。

同一 2,551-token long-context 输入以 hard hint 进入 capable 后正确回答，耗时
15,448 ms、成本 `$0.002178408`。据此为 Route 增加可配置
`long_context_quality_threshold_tokens`，SiliconFlow Route 设置为 2,048。
原请求在不带 hard hint 的情况下复测：

- 自动选择 DeepSeek-R1 / capable；
- trace 记录 `structural_decider / long_context_quality`；
- 正确回答，14,318 ms，成本 `$0.002242115`。

修复后五类合成基线为 5/5，平均 8,979.6 ms、P50 5,697 ms、P95 21,821 ms，
总成本 `$0.003723882`。输入和汇总分别保存在
`eval/real/siliconflow-five-category-policy-cases.json` 与
`eval/real/siliconflow-five-category-policy-summary.json`。本轮新增真实付费调用
合计 `$0.005902290`，未超过授权预算；失败的 transport/免费模型调用成本为 0。

每类仍只有一个样本，该结果证明功能和策略修复，不构成统计显著的生产验收。

## 关键证据

### SDK 直连

- 请求模型：`Pro/deepseek-ai/DeepSeek-R1`
- 提示：`What New Opportunities Will Inference Models Bring to the Market?`
- 收到 reasoning 2411 字符、最终内容 2607 字符。
- 流式迭代正常结束，证明密钥、模型名和 SiliconFlow OpenAI-compatible API 有效。

### 简单问候

- 返回模型：`Qwen/Qwen2.5-7B-Instruct`
- 返回内容：`Hello! Nice to meet you. How can I assist you today?`
- uRouter 披露：`siliconflow/qwen2.5-7b-instruct`、`efficient`、
  `default_efficient`。
- 流式复测收到 18 个 chunk，终端 Usage 完整。

该非 Pro 模型在 SiliconFlow 当前规则中属于免费模型，因此成本 0 是有效事实，
不是计价缺失。官方说明免费模型使用原模型名，收费版本使用 `Pro/` 前缀。

### 二元方程

- 输入：`Solve x+y=10 and x-y=2. Answer concisely.`
- 返回模型：`Pro/deepseek-ai/DeepSeek-R1`
- 结果：`x=6, y=4`
- Provider 报告 366 reasoning tokens；Gateway 将总输出 Usage 纳入计价。

### 天气工具闭环

第一轮模型实际返回：

```json
{
  "name": "get_weather",
  "arguments": {"city": "Wuhan"}
}
```

Host 使用中国天气网 2026-08-30 11:30 发布的数据，将武汉 `32℃/24℃`
作为 `role=tool` 结果回传。第二轮 DeepSeek-R1 正常生成最终天气回答。

不提供 `get_weather` 声明时，Gateway 返回：

```json
{
  "error": {
    "code": "missing_required_tool",
    "type": "urouter_error"
  }
}
```

HTTP 状态为 400，且没有产生 DecisionRecord 或上游模型费用。

## 价格与来源

- SiliconFlow 价格页：<https://www2.siliconflow.cn/pricing>
- SiliconFlow Rate Limits 与免费/Pro 模型规则：
  <https://docs.siliconflow.cn/cn/userguide/rate-limits/rate-limit-and-upgradation>
- 天气工具数据：中国天气网湖北页面，2026-08-30 11:30 发布：
  <https://www.weather.com.cn/hubei/index.shtml>

## 安全说明

测试代码、Catalog、Route、DecisionRecord 和本报告均不包含 API key。商用
Gateway 在验证结束后关闭，以清除子进程环境中的凭据。由于该 key 曾以明文
发送到对话中，仍建议在 SiliconFlow 控制台轮换。
