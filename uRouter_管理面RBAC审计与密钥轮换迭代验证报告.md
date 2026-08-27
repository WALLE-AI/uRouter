# uRouter 管理面 RBAC、审计与密钥轮换迭代验证报告

> 日期：2026-08-26  
> 任务：GW-530

## 结论

GW-530 已实现。启用管理密钥环后，管理端点不再仅依赖可伪造的角色或租户声明，
而是校验 Bearer token 的 SHA-256 摘要、角色、tenant allowlist 和生效时间窗。
数据面 chat、feedback 写入、model discovery、health 与 explain 保持原协议。

## 已实现边界

| 能力 | 实现 |
|---|---|
| 认证 | Bearer token；配置只保存 SHA-256，不保存明文 token |
| 授权 | `reader < operator < admin`；逐管理端点声明最低角色 |
| Tenant | 密钥按 tenant hash allowlist 授权，支持 `*`；启用后强制显式 tenant header |
| 审计 | 允许和拒绝均写 JSONL；只有 `sync_data` 成功后才执行管理操作 |
| 脱敏 | tenant 和资源目标只记录作用域 hash，不记录原始 ID 或 token |
| 轮换 | 多个新旧密钥可重叠；支持 not-before/expiry；默认 5 秒热加载 |
| 故障 | 无效新文件保留上一份有效 keyring，并记录 reload 拒绝事件 |
| 兼容 | 未配置 keyring 时保持原 loopback 行为 |

## 权限矩阵

| 操作 | 最低角色 |
|---|---|
| decision/feedback/task-binding 查询，tier 健康查询 | Reader |
| decision/task records/task-binding 删除 | Operator |
| tenant 全量删除，Prometheus metrics | Admin |

## 测试覆盖

自动测试覆盖角色不足、跨 tenant、错密钥、过期密钥、双密钥重叠、同步审计、
token 脱敏、无效轮换回退和有效热加载。端点矩阵测试同时验证 HTTP 语义：
认证失败为 401，角色或 tenant 越权为 403，审计不可用为 503。

自动化门禁通过 72 个 Rust 单元/集成测试和 1 个 rustdoc 测试；3 个需要显式
Redis 环境变量的测试按设计忽略，已由前一轮真实 Redis 报告覆盖。

## 真实端点验证

当前 Qwen3.8-27B 地址为 `http://127.0.0.1:19121/starvlm`。本轮观测结果：

| 场景 | 结果 |
|---|---|
| `/starvlm/v1/models` | HTTP 200，模型 ID `Qwen3.8-27B` |
| 27B direct chat | HTTP 200 |
| 8790 Auto hard/primary | HTTP 200，`capable`，`quality_guard`，输出 `GW530_OK` |
| 8790 Auto trivial/background | HTTP 200，8087 Qwen3.5-4B，`efficient` |
| 无管理凭据查询 decision | HTTP 401 |
| Reader 查询 decision/tier | HTTP 200 |
| Reader 删除 decision | HTTP 403 |
| 跨 tenant Reader | HTTP 403 |
| Operator 查询 Admin metrics | HTTP 403 |
| Admin 查询 metrics | HTTP 200 |
| 拒绝审计身份 | 已记录实际 key ID/role，资源 ID 仅保留 hash |

旧 8094 服务在对照请求期间退出；catalog 已按用户提供的新根路径更新为
`19121/starvlm/v1`，随后 direct 与 Auto 两条链路均通过。

## 运维方案

1. 由密钥管理系统生成高熵 token，仅把 SHA-256 摘要写入 keyring。
2. 先加入新 key，保留旧 key，并设置清晰的生效和过期窗口。
3. 等所有客户端切换后删除旧 key；观察 `management_keyring.reload` 审计事件。
4. 将 audit JSONL 放在独立持久卷并接入日志采集；权限至少限制为网关进程可追加。
5. 生产入口继续由认证代理剥离并注入 `x-urouter-tenant-id`，不得信任终端用户直传。

GW-530 不包含 OAuth/OIDC、集中式 KMS 拉取或跨实例 decision/feedback 存储；这些属于后续任务。
