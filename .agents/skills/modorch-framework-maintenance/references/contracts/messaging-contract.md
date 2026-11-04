> Generated snapshot; source: contracts/messaging-contract.md; source sha256: 987a9e84b16c0746f066732de288f964de5d1454d3377bdaffccd0422b1b0e85; revision 1.0.5. Do not edit.

# Messaging Contract

适用于 modorch 0.0.2 当前开发树，Contract revision 1.1.0。维护责任：Framework Maintainer + QA。发布前必须以验证报告确认；规则文本和源码冲突时阻断发布。

## MSG-EVT-001：Event 入队报告

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 向多个订阅目标 publish。
- Trigger / Guarantee / Boundaries: PublishReport.deliveries 是每目标的准入结果；accepted/rejected/is_partial 不表示 Handler 完成。无订阅者 Unroutable；部分入队不具备原子性；Reject/DropNewest 都保留明确状态。
- Test ID: CT-MSG-EVT-001
- Test: bounded_fanout_reports_partial_delivery_and_ambiguous_requests（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-004
- Responsibility: Framework Maintainer / QA

## MSG-CMD-001：Command Accepted 边界

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Topic + Destination 唯一匹配。
- Trigger / Guarantee / Boundaries: command 仅返回 AdmissionReceipt；业务完成须用 Request 或业务确认事件。零目标 Unroutable，多目标 AmbiguousDestination，队列满 Backpressure。
- Test ID: CT-MSG-CMD-001
- Test: bounded_fanout_reports_partial_delivery_and_ambiguous_requests（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-005
- Responsibility: Framework Maintainer / QA

## MSG-REQ-004：Request 超时边界

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Request 成功准入，调用者等待期限到期。
- Trigger / Guarantee / Boundaries: 仍在 Queued 时取消并返回 CanceledBeforeStart；已经 Running 则返回 Indeterminate，不能推断未执行、回滚或无副作用。框架不自动重试。
- Test ID: CT-MSG-REQ-004
- Test: queued_timeout_cancels_but_running_timeout_is_indeterminate（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-006
- Responsibility: Framework Maintainer / QA

## MSG-EXEC-001：默认 FIFO 与执行域

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 组件默认 Serial，或显式选执行策略。
- Trigger / Guarantee / Boundaries: Serial 一个可变 Handler 按成功准入 FIFO；Partitioned 每 shard 独立 Handler，同 shard FIFO；Concurrent 严格限额但不承诺顺序；多个订阅共用一套组件执行域。
- Test ID: CT-MSG-EXEC-001
- Test: serial_fifo_dynamic_subscriptions_and_typed_requests（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-007
- Responsibility: Framework Maintainer / QA
- Additional Test ID: CT-MSG-EXEC-001-LIMIT；concurrent_workers_obey_strict_limit（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）


## MSG-LANE-001：固定与动态 Lane

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 显式 Partitioned 或 Dynamic 并携带 LaneKey。
- Trigger / Guarantee / Boundaries: LaneKey 不改变接收目标；Partitioned 同 Key FIFO、不同 Key 可能碰撞；Dynamic 限制构造中与活跃 Lane，空闲回收可重建，不承诺保存 Handler 私有状态。Concurrent 没有 per-key FIFO。
- Test ID: CT-MSG-LANE-001
- Test: dynamic_lanes_are_bounded_retire_when_idle_and_recreate_without_reordering（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-008
- Responsibility: Framework Maintainer / QA

## MSG-LANE-002：固定 shard 并行

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Partitioned 的 Key 映射固定 shard。
- Trigger / Guarantee / Boundaries: 同 Key 的消息成功准入顺序保持；独立 shard 可并行，不能保证任意不同 Key 都不碰撞。
- Test ID: CT-MSG-LANE-002
- Test: partitioned_same_key_is_fifo_and_independent_shards_progress（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## MSG-BOUND-001：进程内、有界和非持久

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 消息在当前 Application 内提交。
- Trigger / Guarantee / Boundaries: EventHub 没有持久化、跨进程或 exactly-once 保证。等待准入有期限和预算，取消等待不迟到提交；payload_bytes 是受信声明成本，不是深堆测量。
- Test ID: CT-MSG-BOUND-001
- Test: byte_budgets_and_waiter_limits_release_on_cancellation（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-011
- Responsibility: Framework Maintainer / QA

## MSG-ROUTE-001：控制面与类型边界

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 订阅增删与请求路由竞争。
- Trigger / Guarantee / Boundaries: 回执返回即变更生效，取消订阅不撤销已准入消息；Topic 模式有方向，Command/Request 唯一目标；请求或回复类型错误保留 TypeMismatch 根因。
- Test ID: CT-MSG-ROUTE-001
- Test: control_plane_changes_never_publish_stale_routes（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## MSG-WAIT-001：同步等待环与等待准入

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Handler 发起框架内 Request 或 *_wait。
- Trigger / Guarantee / Boundaries: Request 等待域成环返回 CircularWait；Concurrent 保守使用组件域。Handler 内 *_wait 返回 InvalidConfig，避免以受限执行域等待邮箱。业务锁等待不在检测范围。
- Test ID: CT-MSG-WAIT-001
- Test: handler_cannot_deadlock_by_waiting_for_mailbox_capacity（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
- Additional Test ID: CT-MSG-WAIT-001-CYCLE；rejects_self_and_cross_component_wait_cycles（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Additional Test ID: CT-MSG-WAIT-001-ROOTS；detects_wait_cycles_between_independent_roots（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）


## MSG-REPLY-001：回复所有权

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 等待完成、取消或丢弃 RequestHandle。
- Trigger / Guarantee / Boundaries: wait 消费结果并返回 Arc<R>；Observation 只持状态不保留回复。运行中放弃等待不代表执行退出。
- Test ID: CT-MSG-REPLY-001
- Test: status_observers_never_retain_consumed_or_abandoned_replies（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## MSG-TOPIC-001：Name / Filter 校验与字节边界

- Level: MUST
- Applies to: 0.0.2 开发树；Contract revision 1.1.0
- Status: Implemented；Verified 范围以本轮日志为准
- Precondition: 订阅或任意 publish / command / request（含 external / wait）入站。
- Trigger / Guarantee / Boundaries: Name 与 Filter 分离；二者非空、无 NUL、≤65535 UTF-8 字节（Rust &str 保证有效 UTF-8）。Name 不含 +/#；Filter 的 + 独占层、# 独占末层。非法输入 InvalidConfig；标准合法但超本地 max_topic_bytes 返回 ResourceLimit，默认预算 1024。Destination::Named 独立于 Topic 协议。helper 非法输入 false，不施加本地预算；不 trim、不归一化。
- Test ID: CT-MSG-TOPIC-001；CT-MSG-TOPIC-001-INGRESS
- Test: mqtt_utf8_byte_ceiling_is_separate_from_local_budget / mqtt_invalid_names_are_rejected_by_every_ingress（源码定位：`../crates/modorch-eventhub/tests/topic_conformance.rs`；从匹配框架源码查询）
- Evidence: 本轮验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询）
- Responsibility: Framework Maintainer / QA

## MSG-TOPIC-002：MQTT 单向匹配

- Level: MUST
- Applies to: MQTT 3.1.1 / 5.0 §4.7；0.0.2 开发树
- Status: Implemented；Verified 范围以本轮日志为准
- Precondition: 合法 Filter 匹配合法 Name。
- Trigger / Guarantee / Boundaries: + 恰好一层（含空层）；末尾 # 零或多层。保留前后 / 和空层、大小写敏感；根 +/# 排除 $ 起始 Name，$SYS/# 可以。:id 为普通字面量。$share/... 只按基础 Filter 路径匹配，不解释为共享组；不承诺 MQTT 网络/QoS/Session/Retained。
- Test ID: CT-MSG-TOPIC-002
- Test: mqtt_vectors_match_real_subscription_and_delivery（源码定位：`../crates/modorch-eventhub/tests/topic_conformance.rs`；从匹配框架源码查询），共享独立 fixture MR-T01—T20 与额外向量。
- Evidence: 本轮验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询）
- Responsibility: Framework Maintainer / QA

## MSG-TOPIC-003：helper / index / delivery 一致

- Level: MUST
- Applies to: 0.0.2 开发树
- Status: Implemented；Verified 范围以本轮日志为准
- Precondition: 固定 Destination、组件就绪和足够准入容量。
- Trigger / Guarantee / Boundaries: helper、预编译 RouteIndex 与真实订阅/投递共享解析和匹配实现，同一独立向量结果相同。只有 +/# 进入通配索引；:id 进入精确索引。订阅删除清理两类索引。
- Test ID: CT-MSG-TOPIC-003；CT-MSG-TOPIC-003-DELIVERY
- Test: mqtt_vectors_agree_with_helper_and_route_index（源码定位：`../crates/modorch-eventhub/src/routing.rs`；从匹配框架源码查询） / mqtt_vectors_match_real_subscription_and_delivery（源码定位：`../crates/modorch-eventhub/tests/topic_conformance.rs`；从匹配框架源码查询）
- Evidence: 本轮验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询）
- Responsibility: Framework Maintainer / QA

## MSG-TOPIC-004：订阅变更原子性

- Level: MUST
- Applies to: 0.0.2 开发树
- Status: Implemented；Verified 范围以本轮日志为准
- Precondition: subscribe / 初始 install 校验或与准入竞争的 unsubscribe。
- Trigger / Guarantee / Boundaries: 非法 Filter 不修改 registry、endpoint 或 generation；批量初始安装不部分写入。订阅变更与准入线性化；回执后的新准入不得命中过期索引，已准入消息不撤销；Owner 恢复继续经过旧代际资源屏障。
- Test ID: CT-MSG-TOPIC-004；CT-MSG-TOPIC-004-CHURN；CT-MSG-TOPIC-004-OWNER
- Test: mqtt_invalid_subscriptions_leave_registry_and_generation_unchanged / mqtt_subscription_churn_never_leaves_stale_routes（源码定位：`../crates/modorch-eventhub/tests/topic_conformance.rs`；从匹配框架源码查询） / owner_recovery_waits_for_construction_and_never_installs_a_stale_lane（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本轮验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询）
- Responsibility: Framework Maintainer / QA

## MSG-TOPIC-005：目标 / Lane 与消息语义保持

- Level: MUST
- Applies to: 0.0.2 开发树
- Status: Implemented；Verified 范围以本轮日志为准
- Precondition: MQTT Filter 命中多订阅或不同 Destination / Lane。
- Trigger / Guarantee / Boundaries: Topic 与 Destination 正交，Lane 不选目标；同组件多 Filter 去重。Event 逐目标准入，Command/Request 唯一目标；背压、等待环、回复所有权、超时和生命周期合同全部保留，不将 Accepted 当完成。
- Test ID: CT-MSG-TOPIC-005；并复用 CT-MSG-EVT-001 / CT-MSG-REQ-004 / CT-MSG-WAIT-001 与 Lane/背压回归。
- Test: mqtt_destination_lane_and_component_dedup_remain_orthogonal（源码定位：`../crates/modorch-eventhub/tests/topic_conformance.rs`；从匹配框架源码查询） 和 原消息回归（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本轮验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询）
- Responsibility: Framework Maintainer / QA

## MSG-TOPIC-006：交付一致性

- Level: MUST
- Applies to: 本次公开 Topic 语义变更
- Status: Implemented；工具执行范围以本轮日志为准
- Precondition: MQTT Topic 行为变更交付。
- Trigger / Guarantee / Boundaries: Rustdoc、Contract、指南、示例、兼容/迁移、能力矩阵、Skill revision/快照和实际日志一致。历史发布与测量不改写；fixture 不冒充实际 Agent，缺独立 P0 仍 Candidate。
- Test ID: CT-MSG-TOPIC-006（mapping.json 的 tooling_tests）
- Test: test_drift_detects_stale_snapshot（源码定位：`../tests/agent-evals/test_tooling.py`；从匹配框架源码查询） 及 run.py 的 links/contracts/skills/matrix/drift 门禁、verify.py 全量验证。
- Evidence: 本轮验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询）
- Responsibility: Framework / Docs / Skill Maintainers
