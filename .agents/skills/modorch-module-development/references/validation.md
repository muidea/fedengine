> Generated snapshot; source: docs/testing-guide.md; source sha256: 1b8158f57b26bba165cf0f27392086774c4dd97fd85397a8cb99147bbaec26a6; revision 1.0.3. Do not edit.

# 验证与证据维护

从仓库根执行 CONTRIBUTING（源码定位：`../CONTRIBUTING.md`；从匹配框架源码查询） 中命令；文档、契约、Skill 的静态检查由 tools/doc-validation/run.py 提供。脚本仅使用 Python 3.11 标准库，不默认联网。

Contracts 的 Test ID 映射在 tests/contracts/mapping.json。修改行为时先确认对应断言，再执行测试；check-contracts.sh --results <log> 检查实际 Passed 行，禁止用测试数量推断全部能力。

Rustdoc 缺少公共条目说明在 CI 作为错误；cargo doc 不运行 doctest。模板在仓库内单独 check/test，也必须初始化到干净外部目录后运行。Skill 安装需检测哈希篡改、路径穿越、符号链接和冲突，重复安装应幂等。

Agent E2E 与 fixture replay 分开记录。tests/agent-evals/cases.json 固定输入、范围与验收；run.py 记录实际宿主命令、版本、diff、日志和报告。缺少独立 Agent 执行证据保留 NotRun，release-gate.py 返回非零并维持 Candidate。
