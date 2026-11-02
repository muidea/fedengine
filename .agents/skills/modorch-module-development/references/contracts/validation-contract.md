> Generated snapshot; source: contracts/validation-contract.md; source sha256: cbfefd88f53a1276d715023574618cfe5324d9e716cc38ce5a683edd24897228; revision 1.0.3. Do not edit.

# Validation Contract

## VAL-001：规则到执行证据

- Level: MUST
- Applies to: Contract revision 1.0.0
- Status: Implemented
- Precondition: 声称当前能力已通过验收。
- Trigger: 执行 tools/doc-validation/check-contracts.sh。
- Guarantee: 机器映射（源码定位：`../tests/contracts/mapping.json`；从匹配框架源码查询）中每个 Test ID 唯一，指向真实 Rust test；覆盖 SEM-001 至 SEM-012，规则正文必须含相同 ID。带 --results <cargo-test.log> 才能按实际执行输出判定 Passed；单独检查映射只表示静态校验通过。
- Boundaries: 日志中的测试函数必须出现唯一、完整的 test ... ok 结果；只找到源码或编译通过不能替代行为测试。补充兼容/安全/门禁测试由 Python unittest 执行并独立记录。
- Test ID: CT-VAL-001
- Test: test_contract_mapping_rejects_missing_test（源码定位：`../tests/agent-evals/test_tooling.py`；从匹配框架源码查询）
- Evidence: 验证报告（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询）
- Responsibility: QA

## VAL-002：发布状态与不可绕过项

- Level: MUST
- Applies to: 文档与 Skill 分发
- Status: Implemented
- Precondition: 生成发布工件或声称 Release Ready。
- Trigger: tools/doc-validation/release-gate.py 读取验证及 Agent 评估结果。
- Guarantee: 所有强制命令、P0 Agent 用例、Bundle 完整性/安装/兼容、公开 API 文档检查必须实际 Passed；Failed/NotRun/缺失项都返回非零并保留 Candidate。平台支持声明须有对应宿主版本和执行证据。
- Boundaries: 确定性 fixture replay 不能替代独立 Agent 任务；本次未完成跨宿主 Agent 评估时必须显式阻断，不因文件存在自动通过。
- Test ID: CT-VAL-002
- Test: test_gate_rejects_notrun_or_missing（源码定位：`../tests/agent-evals/test_tooling.py`；从匹配框架源码查询）
- Evidence: 验证报告（源码定位：`../docs/validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询）
- Responsibility: Release Owner / QA
