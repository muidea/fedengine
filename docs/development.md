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

## 容器

构建上下文为同时包含 fedengine 和 modorch-rs 的父目录：

```sh
docker build -f fedengine/docker/Dockerfile -t fedengine:dev .
```

镜像内默认开发配置监听 localhost。部署时挂载 `/etc/fedengine/application.toml` 并配置容器内监听 `0.0.0.0:15432`、TLS、SecretRef、扩展绝对路径和持久化临时目录。镜像不包含源密码和测试证书。运行时需要保护配置、扩展目录和 TLS key，应用采用单一可信安全域，不提供强多租户隔离。

`docker/docker-compose.test.yml` 仅用于隔离联调，固定测试口令不是生产凭证。测试结束使用专用 project 的 `down -v` 清理，不影响其他容器。
