# uRouter Redis 共享熔断迭代验证报告

> 验证日期：2026-08-26  
> Redis：`127.0.0.1:16379` 临时无持久化实例  
> 真实模型：Qwen3.5-4B / `127.0.0.1:8087`

## 结论

GW-520/521 已完成。网关保留本地 in-flight、加权选择和快速 circuit，在外层增加 Redis
共享 gate。该结构避免把网络 I/O 塞入纯 CapacityManager，同时让多个网关共享摘除状态。

故障作用域如下：

| 错误 | 共享作用域 |
|---|---|
| transport、timeout、普通 500、404 | deployment |
| 401/403、429 | credential |
| 502/503/504 | provider |

每个候选必须同时通过 deployment、credential 和 provider 三个 scope。Redis Lua 脚本原子检查
Open 状态，并在 cooldown 到期后只授予一个全局 Half-Open probe token。成功、失败、取消分别
关闭、重开或释放 probe；状态键按 window/cooldown 自动过期。

## Redis 集成测试

三个显式 Redis 测试全部通过：

1. TaskBinding CAS：并发 Created/Conflict、授权 migration、tenant 删除。
2. Provider 失败：同 provider 备部署被摘除，其他 provider 可用；到期仅一个 probe。
3. Credential/deployment：同 credential 被摘除但同 provider 的其他 credential 可用；单 deployment
   失败不会错误摘除兄弟 deployment。

常规测试 67 个通过，Redis 显式集成测试 3 个通过，rustdoc 测试 1 个通过。

## 完整网关故障注入

测试路由见 `gateway/route.redis-circuit-test.json`：首部署指向未监听的 18087，备部署指向真实
8087。两个网关使用相同 Redis prefix 和 30 秒 cooldown。

| 实例 | 结果 |
|---|---|
| A / 8790 | 首次 transport 失败，随后 8087 成功；DecisionRecord 有 2 个 attempts |
| B / 8791 | 从 Redis 看到 deployment Open，直接调用 8087；DecisionRecord 只有 1 个 attempt |

`GET /v1/tiers` 同时显示：

- `efficient-unavailable` 本地状态 Open。
- 共享 deployment scope Open，并返回剩余 cooldown。
- credential 和 provider scope Closed。
- `efficient-live` 三个共享 scope 均 Closed。

## 当前边界

- Redis 共享的是 TaskBinding 和 circuit；feedback、DecisionRecord 仍按实例存储。
- 401/429 默认按 credential 处理；不同配额池必须配置不同 `credential_scope`。
- 生产 Redis 仍需 TLS、ACL、持久化、高可用和容量规划。
- GW-530..610 均已完成；下一条 M0 关键路径为 AionCore 动态 header 接线与 soak/SLO。
