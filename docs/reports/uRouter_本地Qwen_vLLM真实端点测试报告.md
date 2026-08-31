# uRouter 本地 Qwen vLLM 真实端点测试报告

> 测试日期：2026-08-26  
> 端点：`http://127.0.0.1:8087`  
> 访问方式：宿主机 loopback，无认证

## 结论

端点可以作为 OpenAI Chat Completions 兼容的文本与结构化输出 deployment 使用，
但当前不应进入要求工具调用或 developer role 的 Auto 候选集。推理过程被混入普通
`content`，对 UI 和跨模型 handoff 也需要额外处理。

最重要的部署差异：用户描述为 `qwen3.8-27B`，但 `/v1/models` 实际返回
`Qwen3.5-4B`，模型根目录为 `/home/dataset0/images/Qwen3.5-4B`。uRouter 目录按
服务自报事实登记为 `local-vllm/qwen3.5-4b`，未将其误标为 27B。

后续宿主机进程检查确认，真正的 `Qwen3.8-27B` 运行在 `8094`，详见独立的
`uRouter_本地Qwen3.8-27B_vLLM真实端点测试报告.md`。

## 环境识别

| 项目 | 实测结果 |
|---|---|
| `/health` | HTTP 200，空响应体 |
| `/version` | vLLM `0.19.0` |
| 模型 ID | `Qwen3.5-4B` |
| 最大模型长度 | 16384 |
| 认证 | 无认证可访问 |

## 协议测试

| 场景 | 结果 | 证据 |
|---|---|---|
| 非流式 Chat Completions | 通过 | HTTP 200，约 0.64 秒，usage 完整 |
| 严格 JSON Schema | 通过 | HTTP 200，返回 `status=ok, count=3`，约 1.16 秒 |
| 流式 SSE | 通过 | HTTP 200，首 chunk 约 21 ms，总耗时约 0.88 秒 |
| 流式 usage | 通过 | 最终 chunk 返回 17 prompt、96 completion tokens |
| finish reason | 通过 | 正常返回 `stop` 或 `length` |
| 强制工具调用 | 失败 | HTTP 400，服务未配置 `--tool-call-parser` |
| developer role | 失败 | HTTP 400，`Unexpected message role` |

## 行为风险

1. 简单的“只返回一个词”指令仍会输出显式 `Thinking Process`，并可能耗尽
   `max_tokens` 后以 `length` 结束。
2. `reasoning` 没有通过独立字段返回，而是混在 `content` 中。UI 若直接展示会泄露
   内部思考文本，并影响用户感知的首字延迟。
3. 模型理论能力不能代替 deployment 能力。工具调用是否可用取决于 vLLM 启动参数，
   因此当前 deployment 必须标记 `tool_calling=false`。

## 建议的 vLLM 调整

- 确认端口指向是否正确；若目标确实是 27B，应先修复实际加载模型与 served model name。
- 为目标 Qwen 模型配置匹配的 `--tool-call-parser`，再重新执行工具调用测试。
- 检查 chat template 或 reasoning parser 配置，将 reasoning 与最终回答分离；在修复前，
  不应让该 deployment 承担要求简洁输出或透明 handoff 的请求。
- 保留 `stream_options.include_usage=true`，该配置已能为 DecisionRecord 提供实际 usage。
