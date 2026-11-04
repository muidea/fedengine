> Generated snapshot; source: docs/concepts/component-model.md; source sha256: 81821b126c2b27567c05cff87c33a777530b766f7a7782a90a969d7fa35619e0; revision 1.0.5. Do not edit.

# 两种组件角色

适用 0.0.2；角色语义见 [COMP-001 / COMP-002](contracts/component-contract.md)。

先判断是否需要向其他组件暴露受控资源实例：需要则选择 Initiator + Registry；主要提供消息/业务行为则选择 Module；不参与编排的帮助类型保持普通 Rust 类型。

| 场景 | 选择 | 原因 |
|---|---|---|
| 跨模块共享配置快照或连接池 | Initiator | 可受控借用、由 Owner 释放 |
| 私有订单缓存或存储 | Module | 状态通过消息封装 |
| 周期清理任务 | Module | 调度与生命周期管理 |
| 普通 DTO / Topic 常量 | 普通类型 / contract crate | 没有资源生命周期 |
| 同时监听消息的资源提供者 | Initiator + event_endpoint | 不需要第三种组件角色 |

Module 可以有 HashMap、Mutex、私有数据库；有状态不构成 Initiator 判据。Initiator 可以不安装消息端点。业务只能从 Facade 获取公共 API，不能把 Module 的完整实例发布到 Registry。

Factory 注册时不创建业务资源。Registered、Enabled、Instantiated、Running 是四个不同阶段；默认空选择。完整行为验证见 application::only_enabled_factories_create_and_dag_orders_lifecycle，私有 Handler 状态见 multi-module（源码定位：`../../examples/multi-module/src/main.rs`；从匹配框架源码查询）。
