> Generated snapshot; source: contracts/scheduler-contract.md; source sha256: 30a5a18eade561c9ce3dbc34f48f13b160e833cf7f1d0c3a87ce9504b4182aba; revision 1.0.3. Do not edit.

# Scheduler Contract

适用于 modorch 0.0.1 当前开发树，Contract revision 1.0.0。维护责任：Framework Maintainer + QA。发布前必须以验证报告确认；规则文本和源码冲突时阻断发布。

## SCH-001：Owner 与取消

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 使用 Context 的 TaskScheduler 派生任务。
- Trigger / Guarantee / Boundaries: 任务属于组件及代际，cancel 仅发协作信号，退出后才归还操作计数。不可用裸 tokio::spawn 遗留框架外后台工作。错误/Panic 是失败并进入监督。
- Test ID: CT-SCH-001
- Test: tasks_have_results_bounds_and_cooperative_shutdown（源码定位：`../crates/modorch-scheduler/tests/tasks.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-012
- Responsibility: Framework Maintainer / QA
- Additional Test ID: CT-SCH-001-PANIC；task_panic_is_failure_and_never_success（源码定位：`../crates/modorch-scheduler/tests/tasks.rs`；从匹配框架源码查询）


## SCH-002：定时器与重叠

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: delay 或 interval 注册 TaskFactory。
- Trigger / Guarantee / Boundaries: TimerDriver 独立计数、不占执行槽；周期首次在 period 后触发，错过 Tick 使用 Skip。SkipIfRunning 跳过重叠，QueueBounded 限 backlog，AllowOverlap 需限额；Timer.wait 只确认 driver 退出。
- Test ID: CT-SCH-002
- Test: timers_do_not_consume_task_slots_and_skip_overlapping_ticks（源码定位：`../crates/modorch-scheduler/tests/tasks.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
- Additional Test ID: CT-SCH-002-QUEUE；bounded_timer_queue_preserves_only_its_configured_backlog（源码定位：`../crates/modorch-scheduler/tests/tasks.rs`；从匹配框架源码查询）
- Additional Test ID: CT-SCH-002-OVERLAP；timer_overlap_has_an_explicit_limit（源码定位：`../crates/modorch-scheduler/tests/tasks.rs`；从匹配框架源码查询）


## SCH-003：CPU / 阻塞 I/O 与真实完成

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 提交拥有输入的同步闭包。
- Trigger / Guarantee / Boundaries: 两类独立有界池，共享全局和 Owner 额度。丢弃结果或 force 不能强杀同步执行；wait_finished 确认任务自身租约、捕获与额度释放，不能保证调用方保留的外部值释放。
- Test ID: CT-SCH-003
- Test: finished_ack_returns_all_permits_before_a_capacity_one_resubmission（源码定位：`../crates/modorch-scheduler/tests/tasks.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
