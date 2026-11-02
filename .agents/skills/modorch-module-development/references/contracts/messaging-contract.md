> Generated snapshot; source: contracts/messaging-contract.md; source sha256: 05bfc6e8296412a381358736f75c64aabd2861693bd8b3f016d000fe85e82d0e; revision 1.0.3. Do not edit.

# Messaging Contract

适用于 modorch 0.0.1 当前开发树，Contract revision 1.0.0。维护责任：Framework Maintainer + QA。发布前必须以验证报告确认；规则文本和源码冲突时阻断发布。

## MSG-EVT-001：Event 入队报告

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 向多个订阅目标 publish。
- Trigger / Guarantee / Boundaries: PublishReport.deliveries 是每目标的准入结果；accepted/rejected/is_partial 不表示 Handler 完成。无订阅者 Unroutable；部分入队不具备原子性；Reject/DropNewest 都保留明确状态。
- Test ID: CT-MSG-EVT-001
- Test: bounded_fanout_reports_partial_delivery_and_ambiguous_requests（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-004
- Responsibility: Framework Maintainer / QA

## MSG-CMD-001：Command Accepted 边界

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Topic + Destination 唯一匹配。
- Trigger / Guarantee / Boundaries: command 仅返回 AdmissionReceipt；业务完成须用 Request 或业务确认事件。零目标 Unroutable，多目标 AmbiguousDestination，队列满 Backpressure。
- Test ID: CT-MSG-CMD-001
- Test: bounded_fanout_reports_partial_delivery_and_ambiguous_requests（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-005
- Responsibility: Framework Maintainer / QA

## MSG-REQ-004：Request 超时边界

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Request 成功准入，调用者等待期限到期。
- Trigger / Guarantee / Boundaries: 仍在 Queued 时取消并返回 CanceledBeforeStart；已经 Running 则返回 Indeterminate，不能推断未执行、回滚或无副作用。框架不自动重试。
- Test ID: CT-MSG-REQ-004
- Test: queued_timeout_cancels_but_running_timeout_is_indeterminate（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-006
- Responsibility: Framework Maintainer / QA

## MSG-EXEC-001：默认 FIFO 与执行域

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 组件默认 Serial，或显式选执行策略。
- Trigger / Guarantee / Boundaries: Serial 一个可变 Handler 按成功准入 FIFO；Partitioned 每 shard 独立 Handler，同 shard FIFO；Concurrent 严格限额但不承诺顺序；多个订阅共用一套组件执行域。
- Test ID: CT-MSG-EXEC-001
- Test: serial_fifo_dynamic_subscriptions_and_typed_requests（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-007
- Responsibility: Framework Maintainer / QA
- Additional Test ID: CT-MSG-EXEC-001-LIMIT；concurrent_workers_obey_strict_limit（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）


## MSG-LANE-001：固定与动态 Lane

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 显式 Partitioned 或 Dynamic 并携带 LaneKey。
- Trigger / Guarantee / Boundaries: LaneKey 不改变接收目标；Partitioned 同 Key FIFO、不同 Key 可能碰撞；Dynamic 限制构造中与活跃 Lane，空闲回收可重建，不承诺保存 Handler 私有状态。Concurrent 没有 per-key FIFO。
- Test ID: CT-MSG-LANE-001
- Test: dynamic_lanes_are_bounded_retire_when_idle_and_recreate_without_reordering（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-008
- Responsibility: Framework Maintainer / QA

## MSG-LANE-002：固定 shard 并行

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Partitioned 的 Key 映射固定 shard。
- Trigger / Guarantee / Boundaries: 同 Key 的消息成功准入顺序保持；独立 shard 可并行，不能保证任意不同 Key 都不碰撞。
- Test ID: CT-MSG-LANE-002
- Test: partitioned_same_key_is_fifo_and_independent_shards_progress（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## MSG-BOUND-001：进程内、有界和非持久

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 消息在当前 Application 内提交。
- Trigger / Guarantee / Boundaries: EventHub 没有持久化、跨进程或 exactly-once 保证。等待准入有期限和预算，取消等待不迟到提交；payload_bytes 是受信声明成本，不是深堆测量。
- Test ID: CT-MSG-BOUND-001
- Test: byte_budgets_and_waiter_limits_release_on_cancellation（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-011
- Responsibility: Framework Maintainer / QA

## MSG-ROUTE-001：控制面与类型边界

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 订阅增删与请求路由竞争。
- Trigger / Guarantee / Boundaries: 回执返回即变更生效，取消订阅不撤销已准入消息；Topic 模式有方向，Command/Request 唯一目标；请求或回复类型错误保留 TypeMismatch 根因。
- Test ID: CT-MSG-ROUTE-001
- Test: control_plane_changes_never_publish_stale_routes（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## MSG-WAIT-001：同步等待环与等待准入

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Handler 发起框架内 Request 或 *_wait。
- Trigger / Guarantee / Boundaries: Request 等待域成环返回 CircularWait；Concurrent 保守使用组件域。Handler 内 *_wait 返回 InvalidConfig，避免以受限执行域等待邮箱。业务锁等待不在检测范围。
- Test ID: CT-MSG-WAIT-001
- Test: handler_cannot_deadlock_by_waiting_for_mailbox_capacity（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
- Additional Test ID: CT-MSG-WAIT-001-CYCLE；rejects_self_and_cross_component_wait_cycles（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Additional Test ID: CT-MSG-WAIT-001-ROOTS；detects_wait_cycles_between_independent_roots（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）


## MSG-REPLY-001：回复所有权

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 等待完成、取消或丢弃 RequestHandle。
- Trigger / Guarantee / Boundaries: wait 消费结果并返回 Arc<R>；Observation 只持状态不保留回复。运行中放弃等待不代表执行退出。
- Test ID: CT-MSG-REPLY-001
- Test: status_observers_never_retain_consumed_or_abandoned_replies（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
