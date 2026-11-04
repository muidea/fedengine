> Generated snapshot; source: docs/concepts/lifecycle.md; source sha256: 2856adefc5bd9fb532bbb01cfd54bf41f00101816d19e0a85e711e739507c3a3; revision 1.0.5. Do not edit.

# 生命周期、补偿与资源固定点

适用 0.0.2；规范见 [Lifecycle](contracts/lifecycle-contract.md) 与 [Resource](contracts/resource-contract.md)。

build 校验而不创建实例；start 按拓扑 create/setup，Initiator 导出服务，安装组件 Handler，随后全体 start、全体 ready，开放准入并 activate_ingress。钩子必须及时返回，长任务交给 Context 的 Scheduler。完整时序与资源屏障见实现细节（源码定位：`../lifecycle.md`；从匹配框架源码查询）。

正常关闭先关闭根准入、停止生产者、执行 begin_shutdown/quiesce，再等待消息、任务、Timer 和服务租约计数到固定点，最后逆 DAG teardown 并终止执行器。应用必须在销毁宿主 Runtime 之前等待回执。

StopIncomplete 只表示当前等待或事务未完成；再次调用等待既有事务，已完成 checkpoint 不重做。Forced 不等于资源已正常回收。Setup 部分失败仍进入补偿，StartupError 主因与 cleanup 分开保留。

监督支持 FailFast、StopDependents、Restart。重建须先排空并拆卸旧依赖闭包，再推进代际；消息不会自动重放。重建预算在 Application 生命周期内累计，旧句柄不能访问新资源。验证见 supervision.rs（源码定位：`../../crates/modorch/tests/supervision.rs`；从匹配框架源码查询）。
