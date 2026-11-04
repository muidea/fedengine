> Generated snapshot; source: contracts/resource-contract.md; source sha256: 78dbf4ee1b4c562666c489c4830840ad2ec5f0630a2f5457fdc8bba6c918be07; revision 1.0.5. Do not edit.

# Resource Contract

适用于 modorch 0.0.2 当前开发树，Contract revision 1.1.0。维护责任：Framework Maintainer + QA。发布前必须以验证报告确认；规则文本和源码冲突时阻断发布。

## RES-001：服务访问与身份

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: Initiator 以 Owner + 名称 + Rust TypeId 发布服务。
- Trigger / Guarantee / Boundaries: 相同键重复发布 InvalidConfig；不同类型是不同键。Module 不能获取 ServicePublisher；读取仅限自己与声明依赖，越界 AccessDenied，不可用或类型键不存在 ServiceUnavailable。
- Test ID: CT-RES-001
- Test: services_enforce_declared_dependencies_isolation_and_leases（源码定位：`../crates/modorch-registry/tests/registry.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## RES-002：租约屏障

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 读取方 acquire ServiceHandle。
- Trigger / Guarantee / Boundaries: ServiceLease 同时保护读取组件与资源 Owner，实际归还前不拆卸资源；Handle 不暴露无计数 Arc。旧代际句柄失效。
- Test ID: CT-RES-002
- Test: externally_held_service_leases_block_owner_teardown（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## RES-003：Borrowed 所有权

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 宿主注入 Arc 并 publish_borrowed。
- Trigger / Guarantee / Boundaries: 框架撤销应用内可用性；底层资源由宿主最终关闭，Owned 由 Initiator 释放。Rust Arc 存活不代表资源逻辑可用。
- Test ID: CT-RES-003
- Test: borrowed_resources_remain_host_owned_but_handles_are_generation_scoped（源码定位：`../crates/modorch-registry/tests/registry.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
