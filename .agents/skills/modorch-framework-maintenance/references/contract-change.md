> Generated snapshot; source: CONTRIBUTING.md; source sha256: f112e083203ec6c07c3d6780d567802d07625af3c314c3b8229cc5d34bf6ddfd; revision 1.0.5. Do not edit.

# 贡献与检查

依赖 Rust >= 1.85、Cargo 和 Python 3.11+。先阅读 AGENTS.md（源码定位：`AGENTS.md`；从匹配框架源码查询）、[Contracts](contracts/README.md)与能力矩阵（源码定位：`docs/reference/capability-matrix.md`；从匹配框架源码查询）。对外 API 只经 modorch Facade，注册/启用/实例化分离。

变更时：核对解析版本与源码 → 定位 Rule ID / Test ID → 修改代码及 Rustdoc → 同步 Contract、指南、迁移和 Skill revision → 刷新生成快照 → 执行验证并保留日志。不得将计划或未运行测试写为 Passed。

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo test --doc --workspace --all-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
cargo +1.85.0 check --workspace --all-targets --all-features --locked
python3 tools/doc-validation/run.py
python3 -m unittest discover -s tests/agent-evals -p 'test_*.py'
```

新环境先获取依赖；离线环境可加 --offline，缺缓存记录 NotRun。模板/示例 smoke 与打包验证见 分发工具（源码定位：`tools/skill-packaging/README.md`；从匹配框架源码查询）。发布门禁缺 Agent 证据时应失败，维护者不能用 skip 或 fixture replay 降低门槛。发布到远程平台仍需显式授权。

Skill 使用开放格式；项目维护可选 skill-hub。先 skill-hub validate --pattern 'modorch-*' --links，再 feedback 到本地归档；不自动 push。外部分发必须带齐参考快照、脚本、模板与逐文件哈希，不能引用框架仓库外的必需相对文件。
