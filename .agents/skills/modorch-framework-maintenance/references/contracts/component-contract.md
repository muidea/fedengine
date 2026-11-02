> Generated snapshot; source: contracts/component-contract.md; source sha256: 5970b2a0312652329e2a283fa52664d213e4d1d35ec68360fb5dbc6ba37d12c9; revision 1.0.3. Do not edit.

# Component Contract

适用于 modorch 0.0.1 当前开发树，Contract revision 1.0.0。维护责任：Framework Maintainer + QA。发布前必须以验证报告确认；规则文本和源码冲突时阻断发布。

## COMP-001：选择 Initiator / Module

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 业务暴露共享资源或封装消息行为。
- Trigger / Guarantee / Boundaries: 共享资源以 Initiator 发布窄接口；Module 可持有私有状态，也可不安装消息端点。只有两种框架角色，不把有无状态作为判定依据。
- Test ID: CT-COMP-001
- Test: initiator_can_register_an_optional_event_endpoint（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-001
- Responsibility: Framework Maintainer / QA

## COMP-002：注册、启用与实例化分离

- Level: MUST
- Applies to: 0.0.1 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 注册 Factory，选择本次启用集合。
- Trigger / Guarantee / Boundaries: 默认选择为空；build 校验但不 create，start 只 create 解析后启用的组件。Factory::descriptor/validate_config 必须无业务资源副作用。
- Test ID: CT-COMP-002
- Test: only_enabled_factories_create_and_dag_orders_lifecycle（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-002
- Responsibility: Framework Maintainer / QA
