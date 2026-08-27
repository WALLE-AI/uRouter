# uRouter Redis 多实例状态迭代验证报告

> 验证日期：2026-08-26  
> Redis：`127.0.0.1:16379`（临时、无持久化测试实例）  
> Gateway A：`127.0.0.1:8788`  
> Gateway B：`127.0.0.1:8789`

## 实现结论

TaskBinding 已从网关内同步内存表拆为异步状态端口。默认后端仍是有界内存表；配置
`--redis-url` 后使用 Redis Hash、tenant 索引和不依赖可选 Lua 模块的原子 CAS 脚本。

CAS 在一个原子操作中处理四种结果：首次创建、同模型刷新、授权迁移和冲突。并发首次调用
保持 first-success-wins；只有授权迁移增加 generation。绑定和 tenant 索引均有 TTL。

## 自动化验证

真实 Redis 集成测试使用两个独立连接并发写同一 task：

- 一个写入返回 `Created`，另一个返回 `Conflict`。
- winner generation 为 1。
- 授权模型迁移返回 `Migrated`，generation 变为 2。
- 删除 tenant A 后 tenant B 的绑定仍存在。

复现命令：

```bash
UROUTER_TEST_REDIS_URL=redis://127.0.0.1:16379/ \
  cargo test -p urouter-gateway --bin urouter-gateway \
  redis_cas_is_shared_atomic_and_tenant_scoped -- --ignored
```

## 双实例真实模型验证

1. A、B 使用相同 Redis URL 和 prefix，均要求可信 tenant header。
2. 向 A 发送 WorkBuddy primary + hard/plan 请求。
3. A 路由到 8094 `Qwen3.8-27B`，模型返回 `REDIS_A_OK`，绑定写入 Redis。
4. 向 B 对同 tenant/task 发送无 hard hint 的 `/v1/explain`。
5. B 返回 `reason=task_binding`、`tier=capable`、Qwen3.8-27B。
6. 向 B 用另一 tenant 和相同 task ID 请求，返回 `reason=default_efficient`、Qwen3.5-4B。

这证明 task continuity 可以跨网关实例共享，同时 tenant scope 没有被 Redis 全局状态破坏。

## 当前边界

- Redis 当前只承载 TaskBinding；capacity/circuit、feedback 和 DecisionRecord 仍为实例内状态。
- Redis URL 由部署系统保护，不应通过不可信配置传入。
- 生产 Redis 需要 TLS/ACL、持久化、主从或集群策略；临时验证实例不代表生产拓扑。
- 下一任务是 GW-520：共享 cooldown/circuit，并区分 credential/provider/deployment 故障作用域。
