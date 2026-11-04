# 账号同名虚拟库与 PostgreSQL 行为对齐方案

状态：用户已确认并实施；本地部署的协议/JDBC 验收通过，本次未重新执行 DBeaver GUI 验收。本文修订既有方案的 database/schema、名称解析及登录失败契约。

## 1. 设计目标与参考边界

对外维持 PostgreSQL Server 的数据库、会话及协议模型，联邦能力通过库内 schema 映射提供。以 PostgreSQL 16 真实行为和固定 JDBC/DBeaver 客户端作为验收基准。

参考工程 `/home/fedquery/下载/federated-query-engine-9.00.00.00` 的事实采集、稳定身份、原子发布、授权过滤和 AST 规划继续适用。但其 `internal/modules/kernel/pgserver/biz/fed/startupMessage.go` 会把空 database **或显式等于用户名的 database** 改成 `postgres`，且使用明文密码认证；本方案不采用这些连接行为，保留 SCRAM-SHA-256。

## 2. 独立业务虚拟库与本地业务范围

显式配置并持久化虚拟数据库 `fedengine`，owner 为现有登录账号 `fedengine`。这是账号业务库，在 `pg_database` 中正常显示，与内部 DuckDB 管理库无关。不因任何账号登录而自动建库，也不自动创建 `postgres`、模板数据库或 `fedengine_metadata`。

首期给该库绑定下列已启用且已有授权的业务源。local schema 使用明确的小写名称，源 catalog_alias 保留原有大小写：

| 虚拟 database | 本地 schema | 来源 catalog_alias | 来源 schema |
| --- | --- | --- | --- |
| fedengine | insight_demo | insight_demo | insight_demo |
| fedengine | postgres17 | PostgreSQL17 | public |
| fedengine | mysql57 | MySQL57 | lwltest |
| fedengine | mysql8 | MySQL8 | LWLTEST |
| fedengine | toolkit_mysql | toolkit_mysql | smart_factory |

另提供空的 `public` schema；本期不提供本地建表/持久化写入功能。演示源 `pg_demo/mysql_demo` 不绑定到账号业务库；停用、不可用及尚未启用的数据源不纳入。后续新增源须显式添加 schema 绑定，不随账号权限扩大自动加入。

保留现有各数据源 database 入口，避免已配置的客户端连接失效；这些入口维持原有远端 schema 名称。账号同名库与源 database 是不同的数据库对象，不是别名或登录重定向。默认数据库列表增加账号业务库，不增加系统管理库。

库 CONNECT 权限独立于业务对象权限。映射后的可读对象范围为“库/schema 绑定 ∩ 当前账号既有源对象授权 ∩ 源可用状态”；owner 不因此获得源超级用户权限。源停用不删除持久化绑定和对象身份，但不再暴露其可用业务对象；访问返回明确错误，不能回退到其他源。业务库即使暂时没有可用源也可连接，保留 public 和标准目录。

## 3. 名称解析与跨源查询

- 未指定或显式传空 database：使用登录用户名；显式非空值精确保留，不裁剪空白、不折叠大小写，不静默改到其他库。
- 单段 `table`：按当前库有效 search_path 解析。
- 两段 `schema.table`：只解析当前数据库的 schema 映射。
- 三段 `当前数据库.schema.table`：按 PostgreSQL 行为允许。
- 三段 `其他数据库.schema.table`：默认返回 `0A000`，说明跨 database 引用不支持。跨源查询改用当前库的两个映射 schema。
- 保留旧联邦三段语法的能力作为显式兼容选项，默认关闭；启用时文档和验收标记为 FedEngine 扩展。本地最新部署采用默认关闭。
- 标识符无引号时转小写，有双引号时精确匹配。切库通过重新连接，不增加 USE 或跨库 SET。

例如，登录 fedengine 库后，`insight_demo.<表>` 与 `mysql57.<表>` 可以在同一条 SELECT 中 JOIN，无须使用不同 database 的限定名。

新账号库默认 `search_path = "$user", public`；不存在的用户 schema 被跳过，因此当前设计初始 `current_schema()` 为 public。`pg_catalog` 遵循 PG 的隐式搜索规则。业务源 schema 不自动全部放入 search_path，避免同名表解析漂移；用户可显式 SET，并使用同一规则查询 SHOW/current_setting/current_schemas。旧源 database 的默认 search_path 继续使用现有配置，以保持连接兼容。

## 4. 元数据与对象身份

增加持久化的虚拟数据库、schema 绑定及映射对象身份。数据库 OID、schema OID、关系 OID 和列 attnum 在刷新、重启后稳定；不同逻辑库中的映射对象具有独立身份，不能直接复用远端对象 OID。源 incarnation、映射身份及源对象身份共同确定映射对象，删除后重建不继承旧对象身份。

账号库的 pg_namespace/pg_class/pg_attribute/information_schema/JDBC 结果只描述当前库中的本地映射名称。绑定同时用于目录和 SQL AST 解析，解析后转成现有源对象引用，再执行授权、stale 检查和既有联邦执行流程；不能只加一行 pg_database。

账号库内映射按外部表语义呈现（pg_class.relkind=f），包括从远端视图读取的映射关系；不把远端视图定义伪装成本地可执行视图。远端主外键、索引、表达式仍由管理 API 保存为来源事实；仅在能够忠实表达 PG 外部表语义时投影到标准目录，不能虚构本地索引或本地强制执行的约束。源 database 入口原有的远端结构展示保持独立契约。JDBC/DBeaver 验收须包含外部表分类与浏览。

内部管理库和 `_fedengine` 存储不开放。pg_catalog/information_schema 保留标准查询能力，导航显示由 DBeaver 默认关闭的“显示系统对象”控制；正常业务虚拟库 fedengine 应可见。

## 5. 登录、错误与会话协议

有效 Startup 先完成凭据验证，再进行数据库存在性、接入状态和 CONNECT 授权检查；协议非法、TLS 要求等前置校验仍在相应阶段执行。认证后授权成功才能发送 AuthenticationOk、ParameterStatus、BackendKeyData 和 ReadyForQuery；SCRAM 交换按机制处理。

| 场景 | 目标响应 |
| --- | --- |
| 错误密码/未知认证用户 | FATAL，28P01 |
| 凭据正确、目标数据库不存在 | FATAL，3D000 |
| 凭据正确、数据库存在但未授予 CONNECT | FATAL，42501 |
| 数据库明确禁止连接 | FATAL，55000 |
| 登录成功后的普通 SQL 错误 | ERROR，按具体 SQLSTATE 和 simple/extended 恢复规则处理 |

登录失败发送 ErrorResponse 后关闭连接，不发送 ReadyForQuery。当前 pgwire 0.42.1 的通用错误路径即使 FATAL 仍可能发送 ReadyForQuery，实施时必须修正实际终结路径；只改 severity 不算完成。若需依赖补丁，使用仓库内可审计、版本固定的补丁，不修改 cargo 缓存来构造不可复现的运行版本。

current_database/current_user/session_user、Startup 参数、SHOW/SET/RESET、current_setting 和 RowDescription 必须与真实会话一致。simple 与 extended 共用语义；会话变化后的预编译处理、参数类型、text/binary、取消、Sync 恢复、Portal 关闭继续覆盖回归。配置保持每连接隔离。

## 6. 兼容范围与明确缺口

优先对齐连接、认证、数据库/schema 名称解析、目录、常用 PG SQL、类型编码和错误序列。业务 SQL 按 PG 方言做 AST 解析和受控转换，不能把 DuckDB 能执行直接等同于 PG 语义一致。

完整 PostgreSQL 内核能力不在本次范围。显式多语句事务、分布式事务、COPY、本地 CREATE DATABASE/CREATE TABLE，以及尚未实现的 PG 扩展/类型/表达式，不伪造成功；进入处理路径后返回相应明确错误，功能不支持通常为 0A000。现有远端数据源保持 read_only=true、ddl_enabled=false。

## 7. 实施与验收

确认后按顺序实施：

1. 增加虚拟库/绑定模型、配置校验、当前管理结构初始化和稳定身份；区分 CONNECT 与对象授权，保留原源 database。
2. 贯通会话 namespace、AST 绑定、授权和目录投影；以同库 schema JOIN 提供跨源查询，并默认关闭跨库三段扩展。
3. 对齐 SCRAM 后数据库校验及 FATAL 终结路径，保持会话参数一致。
4. 更新原始协议、JDBC 和固定 DBeaver 客户端验证，修改旧部署探针中“只允许七个源库”的断言。
5. 备份本地配置与管理库，部署并验证；保留可回滚 release，不改远端数据结构或业务数据，不重新尝试已确认不可用的源。

验收必须覆盖：缺失/空/显式 database；错误密码与缺库的组合；无 CONNECT 与禁止连接；失败后的完整报文顺序和 EOF；当前库三段名与跨库拒绝；search_path、引号及同名表；绑定授权不扩大；源停用与恢复；映射身份跨刷新/重启稳定；账号库的外部表浏览与跨源只读 JOIN；旧源 database 独立连接；系统对象默认导航隐藏且标准目录仍可查询。

以 PostgreSQL 16 的原始报文和 SQL 对照验证标准行为，以固定 PostgreSQL JDBC 42.7.8 / DBeaver CE 25.2.5 验证客户端兼容。未覆盖或存在差异的项目记录为缺口，不通过改写版本号宣称已支持。

## 8. 配置与升级

```toml
[pgwire]
# 其余 pgwire 参数沿用现有配置。
allow_cross_database = false

[[virtual_databases]]
name = "fedengine"
owner = "fedengine"
connect = []
allow_connections = true

[[virtual_databases.schemas]]
name = "insight_demo"
source = "ds_14337600961314816"
remote_schema = "insight_demo"
```

每个映射用一个 `virtual_databases.schemas` 条目；source 使用管理源 ID，而非可改动的显示别名。owner 自动具有 CONNECT，connect 列表显式开放给其他已配置用户；业务 SELECT 仍须原有 grants。定义可由 TOML 显式维护，启动时在本地事务中与持久化定义对齐；这不同于 datasources 的一次性首次导入。新增在线虚拟库 API 后，API 覆盖/删除墓碑优先于同名 TOML 定义，在线变更无需重启，参见[管理 API](management-api.md#在线业务虚拟库)。删除后重建库或 schema 绑定分配新 OID；仅源刷新或服务重启保持存活 OID。

当前结构保存虚拟库、覆盖/墓碑、幂等回执和必需的 revision；不提供历史迁移或旧格式回退，遵循[开发规约](development.md#当前阶段的数据兼容规约)。部署前正常停服务并备份管理库及配置；回退时使用匹配版本备份。

## 9. 实现与验证状态

账号同名库、在线虚拟库管理、会话身份、Prepared 失效及停用恢复已实现；客户端与统计支持范围见[协议兼容说明](protocol-compatibility.md)，后续工作见[剩余项方案](remaining-items-plan.md)。未变更定义的普通停用恢复保留存活身份；可选 MySQL strict 模式的边界见[管理 API](management-api.md)。完整 PostgreSQL 行为仍未全部实现。
