> Generated snapshot; source: docs/guides/messaging.md; source sha256: f3202f68703b7b9440849e05f9dd35ebf2b397f0165e8477b3e65c35022f75ae; revision 1.0.3. Do not edit.

# Event、Command 与 Request

> Applies to: modorch 0.0.1 开发树，按框架指纹匹配
> Capability status: Verified（仅本次验证（源码定位：`../validation/2026-11-02/release-0.0.1/README.md`；从匹配框架源码查询）列出的命令与 Linux / Rust 1.94 范围）
> Last validated: 2026-11-02；本地日志，远程 CI NotRun

## Goal

使用 Event、Command 与 Request，验证消息准入、处理结果和超时边界。

## Prerequisites

Rust >= 1.85、Cargo、完整仓库、锁定依赖缓存或首次联网获取依赖。应用拥有 Tokio Runtime；命令在仓库根目录运行。默认 Facade Feature 包含 runtime/eventhub/scheduler，配置和观测按需开启。首次接入先阅读入门（源码定位：`../getting-started.md`；从匹配框架源码查询）。

## When to use / When not to use

控制消息、业务命令和有结果调用适用；大批量流数据由应用另建有界数据通道。

## Relevant contracts

[MSG-EVT-001 / MSG-CMD-001 / MSG-REQ-004](contracts/messaging-contract.md)。规范性行为以 Contract 为单一维护点；下文为任务步骤。

## Steps

为 DTO 定义版本化 Topic；订阅通过 HandlerRegistration 安装。Event 查看逐目标报告，Command 只确认 Accepted；需要结果用 typed RequestHandle::wait。外部入口使用 hub.external()；Handler 内子调用用 ctx.event_hub。立即准入超额处理 Backpressure，外部可用 *_wait 设单独准入期限。

## Complete runnable example

完整来源：examples/event-messaging/src/main.rs（源码定位：`../../examples/event-messaging/src/main.rs`；从匹配框架源码查询） 和 Cargo.toml（源码定位：`../../examples/event-messaging/Cargo.toml`；从匹配框架源码查询）。依赖仅 Facade、Tokio 及标准库，无数据库、凭据或公网服务。

## Expected observable behavior

两订阅者均完成事件处理并在 shutdown_checked 后读取计数；publish 的 accepted 不当完成计数。

## Failure cases and limitations

超时先区分 Queued / Running，非幂等操作不能无条件重试。Handler 内 *_wait 被拒绝，框架 Request 环会 CircularWait；业务 Mutex 等待不被推断。

## Validation commands

```bash
cargo check -p event-messaging --all-targets --locked
cargo run -p event-messaging --locked
cargo test --workspace --all-features --locked
```

命令非零或断言失败即未通过。缺工具或离线缺缓存标记 NotRun，不能据静态阅读报告 Passed。

## Related API / Examples / Tests

API 源码（源码定位：`../../crates/modorch-eventhub/src/lib.rs`；从匹配框架源码查询）、行为测试（源码定位：`../../crates/modorch-eventhub/tests/concurrency.rs`；从匹配框架源码查询）、限制（源码定位：`../reference/limitations.md`；从匹配框架源码查询）、兼容性（源码定位：`../reference/compatibility.md`；从匹配框架源码查询）。生成 API 参考：cargo doc --workspace --all-features --no-deps --locked，浏览 target/doc/modorch/index.html；doctest 需另行运行。
