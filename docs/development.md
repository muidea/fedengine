# 开发与部署

## 当前阶段的数据兼容规约

本项目处于开发阶段，没有历史数据兼容要求。只维护当前版本数据模型，不新增历史迁移链、旧格式回退、双格式读写、历史回填或自动修复逻辑。修改相关实现时，移除不再需要的历史兼容分支，直接使用当前结构；不受支持的存储版本明确报错，不自动转换或清空。

当前版本初始化、重复启动幂等、正常配置缺省值、来源事实未知状态、运行故障恢复、失败保留和稳定对象身份仍属于功能契约。部署前备份和匹配版本回退继续执行，不得借规约删除现有管理状态、凭据或业务数据。未来出现历史数据兼容需求时，单独确认范围和验收要求后再实施。本规约同时写入仓库根目录 [AGENTS.md](../AGENTS.md)，用于后续开发约束。

受支持 PostgreSQL/MySQL 源版本适配以及对外 PostgreSQL SQL/协议行为仍是产品要求，不属于历史存储兼容，不据此删减。

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
- `fedengine-duckdb/src/`：`instance` 打开唯一实例，`storage` 当前结构初始化与持久化，`control` 受控挂载及恢复，`credentials` 解析引用，`catalog` 扫描结构，`relational` 采集扩展关系事实，`identity` 维护稳定身份，`projection` 发布 PG 目录，`virtuals` 持久化逻辑数据库并同步外部表映射，`init` 装配运行时，`runtime` 管理执行准入，`worker` 执行和输出行批次，`codec` 转换数据，`metadata` 查询元数据，`extensions` 校验扩展，`error` 转换错误。
- `fedengine-pgwire/src/`：监听生命周期、认证、取消分别独立；`session/` 保存连接状态，并拆分游标、解析、Simple 和 Extended 协议实现。

上述适配 crate 的 `lib.rs` 声明内部模块并导出组件接口。内部模块保持私有，模块拆分不改变组件边界或 Modorch 生命周期。

`fedengine-management` 的 `module` 接收 Typed Request，`reducer` 校验意图与版本，`coordinator` 编排受管长任务，`api` 提供生命周期受管 HTTP 入口，`pagination` 绑定目录快照、过滤条件与续页参数。管理配置、凭据引用、首次导入及调用示例见 [管理 API](management-api.md)。

SQL 分析、配置和授权测试分别位于 ddl、core、policy 的 `tests/`。通过 `include_str!` 读取仓库配置示例，避免依赖测试进程工作目录。根目录 `tests/` 保留协议、引擎控制和在线管理系统测试；临时目录由作用域守卫自动清理。

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

验证产生的执行日志、临时验收报告、截图和运行环境快照不提交、不归档，完成验证后清理；运行期间使用被忽略的 `docs/validation/` 或仓库外临时目录。长期维护的客户端查询/预期结果位于 `tests/fixtures/metadata/`，能力边界维护在[协议兼容说明](protocol-compatibility.md)和[管理 API](management-api.md)中。

扩展通过 HTTPS 下载官方 `postgres_scanner`、`mysql_scanner`，保存核心版本、平台、来源 URL 和 SHA256；运行时在首次加载时以固定缓冲流式核对清单与文件，并保留 DuckDB 签名验证；已加载到同一实例的扩展不重复读盘校验。预置清单中的受支持扩展在开放会话前加载。业务 SQL 无法 INSTALL/LOAD，生产推荐预置离线扩展，`install_extensions=false`。升级核心/扩展/源版本后须重验对应能力矩阵。

## 规模与故障验证边界

远端目录采集在独立 DuckDB 实例进行，本地批量身份分配与目录投影在不可见事务内准备，最终屏障只覆盖版本复核、提交及运行注册表切换。正式挂载与缓存清理仍有独立的排空窗口；长事务/悬停 Portal 可推迟管理操作完成，但等待操作不应阻挡新查询。锁顺序、资源预算与失败语义见[管理 API](management-api.md#目录刷新与资源预算)。

规模验收必须同时记录采集、类型探测、身份处理、投影、准备、正式挂载和最终屏障耗时，以及并发查询 p99、最大停顿与进程 RSS。DuckDB memory_limit 和采集字节预算不能替代进程内存测量。连续刷新应观察到内存稳定平台；不要求分配器每轮立即归还全部 RSS。

完整资源、网络、存储和进程强杀故障矩阵，以及 24 小时耐久验证仍需专项执行。单次目录发布成功或本地投影耗时不能代替全链路容量验证。后续工作见[剩余项方案](remaining-items-plan.md)。

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

镜像内默认开发配置监听 localhost。部署时挂载 `/etc/fedengine/application.toml` 并配置容器内监听 `0.0.0.0:15432`、TLS、SecretRef、扩展绝对路径和持久化临时目录。镜像不包含源密码和测试证书。运行时需要保护配置、扩展目录和 TLS key，应用采用单一可信安全域，不提供强多租户隔离。管理入口默认 loopback；默认 `duckdb.database_path` 为文件，部署时将数据库及 WAL 放入同一持久化卷，同时挂载受保护的凭据目录。

`docker/docker-compose.test.yml` 仅用于隔离联调，固定测试口令不是生产凭证。测试结束使用专用 project 的 `down -v` 清理，不影响其他容器。

## JDBC 与目录规模复现

真实 JDBC 程序及准备说明见 [tests/jdbc](../tests/jdbc/README.md)。目录 SQL 回放会验证规划、Describe、执行和结果转换；回放通过不能替代 GUI 实测。

```sh
FEDENGINE_BENCH_OBJECTS=1000 cargo test -p fedengine-duckdb catalog_scale_baseline -- --ignored --nocapture
FEDENGINE_BENCH_OBJECTS=10000 cargo test -p fedengine-duckdb catalog_scale_baseline -- --ignored --nocapture
```

该基线只测本地 synthetic 投影；每表一列，不包含远端扫描、远端请求、生产写锁竞争或业务负载影响。实际扫描还需要每对象一次扩展 DESCRIBE，不能据此推断远端请求数恒定。

隔离数据库验证默认使用 25432/23306；端口已占用时可设置 FEDENGINE_TEST_POSTGRES_PORT/FEDENGINE_TEST_MYSQL_PORT，例如 35432/33306。脚本只管理 fedengine-test 项目的临时容器，并在启动失败或测试退出时清理。

### 真实远端规模回归

规模验收使用 release 构建，常规功能回归仍使用默认测试构建。在专用一次性 PostgreSQL 容器上设置 `FEDENGINE_POSTGRES_DSN`，先准备锁定扩展，再单独执行以下用例。它创建独立测试 schema 和临时管理库，检查首次激活、连续刷新、映射目录、对象身份、并发无关及同源查询、RSS 平台及受管任务排空；不要指向业务数据库。测试失败时也应销毁一次性容器及卷。

```sh
python3 scripts/fetch-extensions.py --dest data/extensions
FEDENGINE_SCALE_OBJECTS=10000 FEDENGINE_SCALE_COLUMNS=1 FEDENGINE_SCALE_ROUNDS=10 \
  cargo test --release --locked --test management metadata_scale_refresh -- --ignored --nocapture --test-threads=1
FEDENGINE_SCALE_OBJECTS=10000 FEDENGINE_SCALE_COLUMNS=10 FEDENGINE_SCALE_ROUNDS=10 \
  cargo test --release --locked --test management metadata_scale_refresh -- --ignored --nocapture --test-threads=1
```

输入限制为最多 10k 表/100k 列；正式实例使用 256MB DuckDB memory_limit，一列表场景操作额度为 256MiB，多列表场景在运行前固定为 512MiB（共享额度 512MiB，因此仅一项采集可运行）；独立采集实例的 DuckDB 内存上限为 128MiB。默认 RSS 上限为 1024MiB，可通过 `FEDENGINE_SCALE_RSS_MB` 显式调整，不能在失败后悄悄改变验收预算。用例断言查询无错误、最大停顿不超过 1 秒、p99 相比空载基线增加不超过 100ms、最后三轮 RSS 波动不超过 64MiB，并检查进程峰值预算。它使用闭环查询加 20ms 间隔，不代表开放环吞吐量或生产容量认证。正式屏障及各阶段耗时从 debug 日志单独核对；Apply 的正式挂载窗口与普通刷新分别评估。临时日志验证后清理，不归档。
