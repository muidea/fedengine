> Generated snapshot; source: contracts/lifecycle-contract.md; source sha256: 8619b215eefafcc19cc4a037cac5a6d4fbb22d7853075516abb7c7b7fc09798d; revision 1.0.3. Do not edit.

# Lifecycle Contract

适用于 modorch 0.0.1 当前开发树，Contract revision 1.0.0。维护责任：Framework Maintainer + QA。发布前必须以验证报告确认；规则文本和源码冲突时阻断发布。

## LIFE-001：宿主所有权与应用隔离

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 同一 Tokio 宿主创建多个 Application。
- Trigger / Guarantee / Boundaries: 宿主持有 Runtime、信号、入口与 Subscriber；应用之间独立注册、消息与健康状态，框架不调用 process::exit。
- Test ID: CT-LIFE-001
- Test: applications_are_isolated_in_one_runtime（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-009
- Responsibility: Framework Maintainer / QA

## LIFE-002：启动失败补偿

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: create 已返回对象，setup 中途失败。
- Trigger / Guarantee / Boundaries: 先登记对象再 setup，失败进入部分初始化清理。StartupError.source 保留主因，cleanup 和 handle 可查询及重试；取消调用者等待也不能遗留已创建资源。
- Test ID: CT-LIFE-002
- Test: startup_error_is_bounded_and_preserves_failed_cleanup_for_retry（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
- Additional Test ID: CT-LIFE-002-CANCEL；cancellation_of_startup_does_not_abandon_created_resources（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）


## LIFE-003：关闭等待超时

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 关闭时仍有在途操作或钩子未完成。
- Trigger / Guarantee / Boundaries: shutdown_checked 超时返回 StopIncomplete；后台事务继续，资源保留，后续等待或重试只调用未完成 checkpoint。force_shutdown 返回 Forced，不能作为正常释放的证据。
- Test ID: CT-LIFE-003
- Test: shutdown_timeout_preserves_resources_and_accepts_only_causal_children（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-010
- Responsibility: Framework Maintainer / QA

## LIFE-004：强制关闭回执

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 在途工作尚未真正退出时强制关闭。
- Trigger / Guarantee / Boundaries: 强制中止框架 Future 不证明外部副作用结束，不调用忙碌资源的普通 teardown，不返回 GracefullyStopped。
- Test ID: CT-LIFE-004
- Test: force_shutdown_never_reports_a_graceful_stop_or_releases_busy_owners（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## LIFE-005：重启代际屏障

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 已启用 FailurePolicy::Restart 且组件故障。
- Trigger / Guarantee / Boundaries: 先封锁故障依赖闭包并排空旧消息、任务和租约，拆卸后推进代际并重建。旧句柄失效，不自动重放消息。累计预算耗尽转 FailFast。
- Test ID: CT-LIFE-005
- Test: restart_waits_for_old_handlers_and_leases_and_invalidates_old_handles（源码定位：`../crates/modorch/tests/supervision.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## LIFE-006：重建失败与预算

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 替代实例 setup 失败。
- Trigger / Guarantee / Boundaries: 部分实例先完整补偿，每次重建消耗应用生命周期内累计预算，不因短暂 Ready 重置；预算耗尽转 FailFast。
- Test ID: CT-LIFE-006
- Test: replacement_setup_failure_is_cleaned_before_retry_and_budget_is_bounded（源码定位：`../crates/modorch/tests/supervision.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
