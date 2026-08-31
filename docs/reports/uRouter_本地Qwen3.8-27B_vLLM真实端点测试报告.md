# uRouter 本地 Qwen3.8-27B vLLM 真实端点测试报告

> 测试日期：2026-08-26  
> 端点：`http://127.0.0.1:8094`  
> uRouter ID：`local-vllm-qwen38/qwen3.8-27b`

## 结论

Qwen3.8-27B 实际运行在 `8094`，不是 `8087`。该 deployment 已具备智能体 Auto
选型所需的核心能力：文本、图像、工具调用、严格结构化输出、流式 usage 和可控
reasoning。Chat Completions 不接受 developer role，消息归一化时必须降级为 system role。

## 部署识别

| 项目 | 实测结果 |
|---|---|
| 进程 PID | `1284125` |
| 模型根目录 | `/home/dataset0/images/Qwen3.8-27B` |
| 服务端口 | `8094` |
| served model name | `Qwen3.8-27B` |
| vLLM 版本 | `0.19.0` |
| tensor parallel | 4 |
| 最大模型长度 | 262144 |
| 默认 thinking | 关闭 |
| 工具解析器 | `qwen3_coder` |
| reasoning parser | `qwen3` |

## 协议测试

| 场景 | 结果 | 证据 |
|---|---|---|
| 模型身份 | 通过 | `/v1/models` 返回 `Qwen3.8-27B` 和 262144 context |
| 非流式 Chat | 通过 | 精确返回 `UROUTER_QWEN38_OK`，约 2.33 秒 |
| 工具调用 | 通过 | 标准 `tool_calls`，`finish_reason=tool_calls`，约 1.95 秒 |
| JSON Schema | 通过 | 严格返回 `status=ok, count=27`，约 0.49 秒 |
| 流式 SSE | 通过 | 首 chunk 约 13 ms，总耗时约 0.57 秒 |
| 流式 usage | 通过 | 最终 chunk 返回 19 prompt、2 completion tokens |
| 图像输入 | 通过 | 接受 PNG data URL 并返回 `IMAGE_OK`，约 0.21 秒 |
| 显式 thinking | 通过 | reasoning 独立字段返回，正文为 `323`，约 1.01 秒 |
| Responses API | 通过 | `/v1/responses` 返回 completed response 和独立 reasoning item |
| developer role | 失败 | HTTP 400，`Unexpected message role` |

## Auto 路由建议

- 可进入需要视觉、工具调用、JSON Schema 或长上下文的候选集。
- 对 Chat Completions 请求，将 developer role 归一化为 system role。
- 普通低延迟请求保持 thinking 关闭；只有任务策略明确要求推理时才启用
  `chat_template_kwargs.enable_thinking=true`。
- 支持 Responses API 的调用方优先使用 Responses，以获得更清晰的 reasoning/output 分离。
- 当前本地成本登记为 0，仅代表无外部 API 账单，不代表 GPU 资源成本为 0；后续应通过
  Deployment cost override 加入 GPU 摊销成本。
