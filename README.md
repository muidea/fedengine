# FedEngine

基于 [详细设计 v1.1](docs/FedEngine_详细设计文档_v1.1.md) 的 Rust 联邦 SQL 服务。使用 Modorch-RS 管理组件、PGWire 提供 PostgreSQL 协议子集，DuckDB 负责本地计算及 PostgreSQL/MySQL 远端访问。

## 本地启动

需要 Rust 1.94.0（由 `rust-toolchain.toml` 固定）、C/C++ 运行库，以及同级目录的 `modorch-rs`（真实 Facade 版本 0.0.2）。准确依赖和指纹见 [开发说明](docs/development.md)。

```sh
cargo check --workspace --locked
cargo run --locked -- --check-config
export FEDENGINE_PASSWORD='replace-with-a-local-password'
cargo run --locked -- --config config/application.toml
```

开发配置的 PGWire 监听 `127.0.0.1:15432`，管理 API 监听 `127.0.0.1:15433`，源定义持久化到 `./data/fedengine.duckdb`。用户为 `fedengine`，首次启动未接入远端源，可执行 `SELECT 1` 并通过管理 API 在线接入。退出使用 SIGINT/SIGTERM。生产配置须提供 TLS 证书、私钥并设置 `tls_required=true`。

## 接入数据源

```sh
python3 scripts/fetch-extensions.py --dest data/extensions
```

以 `examples/config/federation.toml` 为模板，将脚本输出的绝对路径配置为 `[duckdb].extension_directory`，通过环境变量设置 `FEDENGINE_POSTGRES_DSN`、`FEDENGINE_MYSQL_DSN`。源连接串从环境变量或凭据目录内的 `file:` 引用读取，不能提交到版本库。远端账号应限制实际对象权限。

`config/application.toml` 是默认启动配置，`examples/config/federation.toml` 是独立的联邦配置示例。每次启动加载一份完整配置，不会合并这两个文件。准备好扩展和上述环境变量后，从仓库根目录运行：

```sh
cargo run --locked -- --config examples/config/federation.toml
```

配置中的相对路径以进程工作目录为基准。`[[datasources]]` 仅作首次导入；后续启动以管理库为准，不覆盖在线修改。启用管理服务后，在浏览器打开 `http://127.0.0.1:15433/manage/` 使用内置轻量 Web 管理端，无需独立前端服务或构建步骤。登记、发现、Schema 选择、激活、刷新及停用示例见[管理 API](docs/management-api.md)。

显式配置源、namespace allowlist、对象 grants 后才允许访问。DDL 还要求精确版本与验收证据对应的 `[[capabilities]]`；示例默认没有能力记录，因此不会提前开放 DDL。

```sql
SELECT a.id, b.amount
FROM pg_crm.public.customers a
JOIN mysql_erp.erp.orders b ON a.id=b.customer_id;
```

跨源 SELECT/JOIN 可用；写入和 DDL 必须完整绑定到一个已注册的 DataSourceId。显式事务、COPY、业务 SQL 中的 ATTACH/LOAD/CALL、任意文件和外部函数默认拒绝。

## DBeaver 浏览

使用 PostgreSQL 驱动连接 PGWire，database 可填写已启用且授权的 `catalog_alias`，也可填写显式配置的业务虚拟库。Startup 未提供或传空 database 时使用登录用户名；同名库须通过配置或[管理 API](docs/management-api.md#在线业务虚拟库)显式创建。账号虚拟库通过多个本地 schema 映射业务源，使用标准 `schema.table` 完成跨源查询；跨 database 三段引用默认返回 `0A000`，仅在显式开启 `pgwire.allow_cross_database` 时作为联邦扩展提供。原数据源 database 仍保留远端 schema。DBeaver 可开启“显示所有数据库”，并保持“显示系统对象”关闭；内部管理库不公开。虚拟库配置与业务范围见[账号虚拟库方案](docs/virtual-database-pg-alignment-plan.md)，客户端支持范围与限制见[协议兼容说明](docs/protocol-compatibility.md)。

## 验证与文档

```sh
cargo test --workspace --locked
scripts/test-federation.sh
```

第二条命令仅使用专用一次性 PostgreSQL/MySQL 容器，结束后删除这些容器及卷。需要 Docker、Compose、curl 与网络。不要把测试 DSN 指向业务数据库。

- [架构与生命周期](docs/architecture.md)
- [数据源与 Schema 在线管理方案](docs/datasource-management.md)、[管理 API 与调用示例](docs/management-api.md)
- [SQL 能力](docs/sql-capability.md)、[DDL 能力门禁](docs/ddl-capability-matrix.md)
- [协议与类型兼容](docs/protocol-compatibility.md)
- [开发、部署和 skill-hub](docs/development.md)
- [已确认的剩余项方案](docs/remaining-items-plan.md)
- [Git 提交与本地文件分类](docs/git-submission.md)

当前为开发实现。JDBC 元数据接口及指定版本 DBeaver 流程已有实测；完整 PostgreSQL/psql 兼容、生产长时负载和全面故障注入仍未验收。具体边界以能力文档和实际证据为准。
