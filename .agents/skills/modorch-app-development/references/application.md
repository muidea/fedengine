> Generated snapshot; source: docs/guides/application.md; source sha256: 443c2d932feb6abf70159473ebe24e833891f0acedf814c611ae9088dce82d42; revision 1.0.3. Do not edit.

# 装配和运行应用

> Applies to: modorch 0.0.1 开发树，按框架指纹匹配
> Capability status: Verified（仅本次验证（源码定位：`../validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询）列出的命令与 Linux / Rust 1.94 范围）
> Last validated: 2026-11-02；本地日志，远程 CI NotRun

## Goal

装配和运行应用并取得可复核的构建、运行和退出证据。

## Prerequisites

Rust >= 1.85、Cargo、完整仓库、锁定依赖缓存或首次联网获取依赖。应用拥有 Tokio Runtime；命令在仓库根目录运行。默认 Facade Feature 包含 runtime/eventhub/scheduler，配置和观测按需开启。首次接入先阅读入门（源码定位：`../getting-started.md`；从匹配框架源码查询）。

## When to use / When not to use

使用独立 main、配置和 Runtime 装配组件；不用于接管宿主全局资源。

## Relevant contracts

[LIFE-001 / COMP-002](contracts/lifecycle-contract.md)。规范性行为以 Contract 为单一维护点；下文为任务步骤。

## Steps

先注册无副作用 Factory，再以 ComponentSelection 显式启用；build 返回未启动对象，start 执行 Setup / Start / Ready。正常与失败退出都检查 shutdown_checked；启动失败读取 StartupError 的主因、cleanup 和 handle。

## Complete runnable example

完整来源：examples/basic-app/src/main.rs（源码定位：`../../examples/basic-app/src/main.rs`；从匹配框架源码查询） 和 Cargo.toml（源码定位：`../../examples/basic-app/Cargo.toml`；从匹配框架源码查询）。依赖仅 Facade、Tokio 及标准库，无数据库、凭据或公网服务。

## Expected observable behavior

ready 通过后才能接入外部流量；示例打印 basic-app: stopped。

## Failure cases and limitations

默认空选择不会创建任何组件；缺失依赖在 build 报错。start 被取消仍需让宿主 Runtime 存活到清理结束。

## Validation commands

```bash
cargo check -p basic-app --all-targets --locked
cargo run -p basic-app --locked
cargo test --workspace --all-features --locked
```

命令非零或断言失败即未通过。缺工具或离线缺缓存标记 NotRun，不能据静态阅读报告 Passed。

## Related API / Examples / Tests

API 源码（源码定位：`../../crates/modorch-runtime/src/lib.rs`；从匹配框架源码查询）、行为测试（源码定位：`../../crates/modorch/tests/application.rs`；从匹配框架源码查询）、限制（源码定位：`../reference/limitations.md`；从匹配框架源码查询）、[兼容性](api-compatibility.md)。生成 API 参考：cargo doc --workspace --all-features --no-deps --locked，浏览 target/doc/modorch/index.html；doctest 需另行运行。
