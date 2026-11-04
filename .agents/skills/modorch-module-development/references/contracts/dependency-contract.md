> Generated snapshot; source: contracts/dependency-contract.md; source sha256: 85672b411b21ec3db9600be4c18af29492704c91d109414ccfe96c317cc74bb9; revision 1.0.5. Do not edit.

# Dependency Contract

适用于 modorch 0.0.2 当前开发树，Contract revision 1.1.0。维护责任：Framework Maintainer + QA。发布前必须以验证报告确认；规则文本和源码冲突时阻断发布。

## DEP-001：依赖闭包与拓扑

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 选中的组件声明 requires / optional。
- Trigger / Guarantee / Boundaries: RequireSelected 默认拒绝已注册但禁用的强依赖；IncludeRequired 显式补齐强依赖。optional 仅在已启用时参与排序。Initiator 的两类依赖均不能指向 Module；环在 build 阶段拒绝。
- Test ID: CT-DEP-001
- Test: rejects_cycles_cross_layer_dependencies_and_invalid_capacities（源码定位：`../crates/modorch-registry/tests/registry.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: SEM-003
- Responsibility: Framework Maintainer / QA

## DEP-002：缺失依赖与显式选择

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 启用组件引用强依赖。
- Trigger / Guarantee / Boundaries: 未注册的强依赖返回 MissingDependency，不能靠 IncludeRequired 创建未知组件；已注册禁用返回 DisabledDependency。
- Test ID: CT-DEP-002
- Test: missing_dependencies_are_reported_before_instantiation（源码定位：`../crates/modorch-registry/tests/registry.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## DEP-003：身份与重复注册

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: 向同一 Builder 注册或选择组件。
- Trigger / Guarantee / Boundaries: 身份为 ComponentKind + id；重复相同 ComponentRef 拒绝，选择未注册组件返回 ComponentUnavailable，不在运行后静默补装。
- Test ID: CT-DEP-003
- Test: default_selection_is_empty_duplicates_and_unavailable_are_errors（源码定位：`../crates/modorch/tests/application.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA

## DEP-004：可选依赖顺序

- Level: MUST
- Applies to: 0.0.2 + 本次框架指纹
- Status: Implemented；Verified 范围由能力矩阵（源码定位：`../docs/reference/capability-matrix.md`；从匹配框架源码查询）与执行日志限定
- Precondition: optional 依赖恰好已启用。
- Trigger / Guarantee / Boundaries: 依赖先启动，依赖方后启动，停机反向；optional 不启用缺失项，显式 IncludeRequired 只补 requires。
- Test ID: CT-DEP-004
- Test: validates_dependencies_and_orders_optional_selected_dependencies（源码定位：`../crates/modorch-registry/tests/registry.rs`；从匹配框架源码查询）
- Evidence: 本次验证（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询），不得用测试源码存在代替执行。
- Semantic ID: 补充规则
- Responsibility: Framework Maintainer / QA
