> Generated snapshot; source: contracts/README.md; source sha256: 9edcdf896b8cacf8aeea4edd3577100f11231dddaf26636ebff0b9a4afc15dc1; revision 1.0.5. Do not edit.

# 行为契约

适用：modorch 0.0.2 开发树；revision 1.1.0。公开签名由源码/Rustdoc 维护，本文目录只定义可观察语义。

| 范围 | 契约 | 验证入口 |
|---|---|---|
| 角色与启用 | [Component](component-contract.md) | Factory、默认空选择 |
| DAG | [Dependency](dependency-contract.md) | 缺失/禁用、环、角色方向、拓扑 |
| 启停/故障 | [Lifecycle](lifecycle-contract.md) | 补偿、超时、Force、Restart |
| 消息 | [Messaging](messaging-contract.md) | 入队/完成、路由、Lane、取消 |
| 后台任务 | [Scheduler](scheduler-contract.md) | Owner、Tick、真实退出 |
| 共享资源 | [Resource](resource-contract.md) | 访问、所有权、租约 |
| 版本与 Skill | [Compatibility](compatibility-contract.md) | 解析依赖与指纹 |
| 发布门禁 | [Validation](validation-contract.md) | 映射、日志、阻断 |

规则 ID 不得静默复用为相反语义。兼容性变化须更新 CHANGELOG、迁移说明、测试、Skill revision 和快照。历史发布工件保留原 Contract；新版本通过独立 Bundle / Git revision 追溯。
