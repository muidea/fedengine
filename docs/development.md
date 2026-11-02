# 开发与部署

## 锁定依赖

- `rust-toolchain.toml` 固定 Rust/Cargo 1.94.0，并声明 rustfmt、Clippy；edition 2024。Workspace 的 `rust-version = "1.94"` 是当前支持基线，尚未承诺更低版本；内部包统一 `publish = false`。
- Modorch-RS Facade 0.0.1，本地 `../modorch-rs/crates/modorch`。匹配源码 revision `ccc77ce9501cd3e942677bba51b6ff3efb1c63f6`，实际文件指纹由 managed skill preflight 核对。设计中的 v0.3 不视为已发布 API。
- duckdb-rs 1.20000.1、核心 v2.0.0；pgwire 0.42.1；Cargo.lock 固定传递依赖。
- 使用 duckdb-rs 官方 download-lib 特性获取对应原生库，避免每次编译庞大的 bundled C++。构建首次需要网络。

当前交付采用 FedEngine 与 Modorch 两个同级仓库；CI 和 Docker 同样准备这两个目录。克隆 Modorch 到同级目录并 checkout 上述 revision。不要把路径依赖替换成未经验证的同名 crate。Linux 动态库位于 `target/duckdb-download/<triple>/v2.0.0/libduckdb.so`；部署需携带该库并设置加载路径，或使用提供的 Dockerfile。

## 目录与测试归属

根目录同时是可执行包和 workspace，`src/main.rs` 只负责宿主入口，`crates/fedengine-app` 装配应用，其他组件库位于 `crates/`。无需额外的 `apps/` 层。

`config/application.toml` 是默认本地运行配置；`examples/config/federation.toml` 是包含 PostgreSQL/MySQL 数据源的完整示例，通过 `--config examples/config/federation.toml` 显式选择。程序一次只读取一份配置，不做文件合并。配置内的相对路径以进程工作目录为基准，文档中的启动命令从仓库根目录执行。

- `fedengine-ddl/src/`：`analysis` 负责语句分析，`binding` 负责对象及约束绑定，`model` 定义领域类型，`index` 生成受控模板，`parameters` 处理参数 AST。
- `fedengine-duckdb/src/`：`init` 管理组件及实例初始化，`runtime` 管理执行准入，`worker` 执行和输出行批次，`codec` 转换数据，`metadata` 查询元数据，`extensions` 校验扩展，`error` 转换错误。
- `fedengine-pgwire/src/`：监听生命周期、认证、取消分别独立；`session/` 保存连接状态，并拆分游标、解析、Simple 和 Extended 协议实现。

三个 crate 的 `lib.rs` 仅声明模块并导出原有公开接口。内部模块保持私有，模块拆分不改变组件边界或 Modorch 生命周期。

SQL 分析、配置和授权测试分别位于 ddl、core、policy 的 `tests/`。通过 `include_str!` 读取仓库配置示例，避免依赖测试进程工作目录。根目录 `tests/` 保留协议和联邦查询系统测试；临时目录由作用域守卫自动清理。

```sh
cargo test -p fedengine-ddl --locked
cargo test -p fedengine-core --locked
cargo test -p fedengine-policy --locked
```

根目录 `cargo test` 默认只测试根包；完整检查使用 `cargo test --workspace`。首次配置工具链及下载 DuckDB 原生库需要网络。

## skill-hub

所有使用的 skill 通过 skill-hub 托管，项目目录为 `.agents/skills/`。应用、组件、框架兼容检查分别使用 modorch-app-development、modorch-module-development、modorch-framework-maintenance；没有修改框架核心，不虚构框架发布或回归验收。

```sh
skill-hub status --json
skill-hub repo sync --json
skill-hub list
skill-hub use --pattern 'modorch-*' --repo skills-repo --non-interactive
skill-hub apply
python3 .agents/skills/modorch-app-development/scripts/preflight.py --manifest-path Cargo.toml
skill-hub validate --pattern 'modorch-*' --links
```

仅在 status 显示过期时通过 apply 刷新；不要手工复制或修改 managed skill。当前项目还托管了 skill-hub-project-usage、skill-capability-index。远端发布未执行。

## 检查

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo doc --workspace --no-deps --locked
cargo build --locked
python3 scripts/smoke.py
scripts/test-federation.sh
```

验证产生的日志、报告、环境快照和文件摘要属于本地临时结果；`docs/validation/` 已加入 Git 忽略规则，不提交或额外归档。

扩展通过 HTTPS 下载官方 `postgres_scanner`、`mysql_scanner`，保存核心版本、平台、来源 URL 和 SHA256；运行时核对清单与文件，并保留 DuckDB 签名验证。业务 SQL 无法 INSTALL/LOAD，生产推荐预置离线扩展，`install_extensions=false`。升级核心/扩展/源版本后须重验对应能力矩阵。

## CI 执行策略

Actions 在 PR、`master` 分支 push 和手动触发时执行。普通分支 push 由对应 PR 承接验证；同一 PR 或分支的新运行会取消旧运行。

`changes` 判断改动范围，`validate` 在一个 job 内共享构建目录，最终 `check` 汇总结果。分支保护继续使用 `check`：只有改动判断成功，且代码验证成功或纯文档变更按规则跳过时才通过；失败、取消和意外跳过都不能伪报通过。

| 改动 / 触发方式 | 执行内容 |
|---|---|
| 仅 `README.md`、`LICENSE`、`docs/**/*.md` | 跳过 Rust 构建和数据库集成，完成最终 `check` |
| 其他路径，包括代码、配置、示例、依赖、脚本、skill、CI，以及未知新路径 | fmt、依赖准备、框架指纹、Clippy、workspace 测试、真实进程 smoke、一次性数据库联邦集成 |
| 上述验证中的 Rust 源码、包清单、锁文件、工具链、Cargo 设置、skill 或 CI 变更 | 额外执行 Rustdoc，警告视为错误；保守覆盖公共 API 文档变更 |
| `master` 上的非纯文档变更 | 完整验证，包含 Rustdoc |
| 手动触发 | 无论改动范围如何都执行完整验证 |

CI 删除独立 `cargo check`，由同范围的 Clippy 完成编译检查。根包集成测试会使 `cargo test` 构建 `target/debug/fedengine`，smoke 直接使用该程序，因此删除独立 `cargo build`。本地仍可使用上文列出的完整检查命令。

Cargo 依赖及编译产物通过 Rust cache 复用，key 包含固定的 Modorch revision；DuckDB 原生库和扩展通过单独缓存复用，key 包含 runner 平台、架构、Cargo.lock 和扩展下载脚本。扩展脚本在命中缓存后仍校验 manifest 与文件 SHA256。只有 `master` 运行保存缓存，PR 使用缓存并在缺失时正常下载和构建。

恢复缓存后先执行 `cargo fetch --locked`，再执行离线指纹预检，保证冷缓存也能工作。格式检查放在缓存恢复和依赖下载之前，以便尽早失败。未增加构建矩阵或将 Rust 检查拆成多个重复编译的 job；实际节省时间以 Actions 分步耗时为准。

## 容器

构建上下文为同时包含 fedengine 和 modorch-rs 的父目录：

```sh
docker build -f fedengine/docker/Dockerfile -t fedengine:dev .
```

镜像内默认开发配置监听 localhost。部署时挂载 `/etc/fedengine/application.toml` 并配置容器内监听 `0.0.0.0:15432`、TLS、SecretRef、扩展绝对路径和持久化临时目录。镜像不包含源密码和测试证书。运行时需要保护配置、扩展目录和 TLS key，应用采用单一可信安全域，不提供强多租户隔离。

`docker/docker-compose.test.yml` 仅用于隔离联调，固定测试口令不是生产凭证。测试结束使用专用 project 的 `down -v` 清理，不影响其他容器。
