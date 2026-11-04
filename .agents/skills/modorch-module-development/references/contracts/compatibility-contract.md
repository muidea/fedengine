> Generated snapshot; source: contracts/compatibility-contract.md; source sha256: fb0eb6208ce196b1d72a4e33bf70684fafbe2dec7be9362c0c0d0088b303a94b; revision 1.0.5. Do not edit.

# Compatibility Contract

## COMPAT-001：版本与来源匹配

- Level: MUST
- Applies to: 0.0.2 开发树；Skill revision 1.0.5
- Status: Implemented；执行证据见本次验证报告
- Precondition: 外部应用准备运行 Skill 或安装 Bundle。
- Trigger: preflight 通过 Cargo metadata --locked --offline 读取实际解析依赖。
- Guarantee: 必须唯一定位 modorch，核对版本、所有 modorch-* 源码/manifest 指纹与已验证 Feature；仅 Cargo.toml 的版本要求不足以确认兼容。
- Boundaries: 0.x 不默认跨 minor 兼容；本次不声称 crates.io / docs.rs 已发布。未知来源或变更过的同版本源码必须 Blocked，缺工具为 NotRun，不能猜测 API。
- Test ID: CT-COMPAT-001
- Test: test_preflight_rejects_mismatch（源码定位：`../tests/agent-evals/test_tooling.py`；从匹配框架源码查询）
- Evidence: 验证报告（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询）
- Responsibility: Skill Maintainer / Release Owner

## COMPAT-002：公共变更与破坏性演进

- Level: MUST
- Applies to: 所有开发期变更
- Status: Implemented
- Precondition: 公共 API、Feature 或行为承诺发生变化。
- Trigger: 维护者提交变更。
- Guarantee: 更新 Rustdoc、调用示例、Contract、兼容说明、迁移记录和 Skill 快照，执行完整门禁。MQTT Topic 迁移须覆盖 `:id`→`+`、空层/零层、`$` 根通配排除与本地预算错误变更；无 Legacy matcher。Cargo Feature 是加法；MSRV 为 1.85。文档/测试冲突时阻断发布，不由任何单一文件自证正确。
- Boundaries: 自动 affected-files 检查只定位影响与必跑项，不能替代人工语义复核；远程发布需显式授权。
- Test ID: CT-COMPAT-002
- Test: test_drift_detects_stale_snapshot（源码定位：`../tests/agent-evals/test_tooling.py`；从匹配框架源码查询）
- Evidence: 验证报告（源码定位：`../docs/validation/2026-11-04/release-0.0.2/README.md`；从匹配框架源码查询）
- Responsibility: Framework / Docs / Skill Maintainers

## CFG-001：严格配置转换

- Level: MUST
- Applies to: 0.0.2 开发树
- Status: Implemented，Verified 范围见能力矩阵
- Precondition: 应用使用对应 Feature 或指标。
- Trigger / Guarantee / Boundaries: 未知字段、非法容量及非 reject 发布策略在实例创建前拒绝；只有 enabled=true 转换为选择，配置文件加载和环境合并由宿主负责。
- Test ID: CT-CFG-001
- Test: parses_explicit_selection_and_rejects_bad_configuration（源码定位：`../crates/modorch-config/tests/composition.rs`；从匹配框架源码查询）
- Evidence: 本次验证报告
- Responsibility: Framework Maintainer / QA

## OBS-001：有界诊断标签

- Level: MUST
- Applies to: 0.0.2 开发树
- Status: Implemented，Verified 范围见能力矩阵
- Precondition: 应用使用对应 Feature 或指标。
- Trigger / Guarantee / Boundaries: 超过精确标签或字符串预算后聚合 overflow；已有标签继续精确累计，总量保持，快照不承诺跨字段原子性。
- Test ID: CT-OBS-001
- Test: metric_label_overflow_preserves_totals_and_existing_labels（源码定位：`../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）
- Evidence: 本次验证报告
- Responsibility: Framework Maintainer / QA
