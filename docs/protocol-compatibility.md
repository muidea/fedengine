# PostgreSQL 协议兼容

pgwire 锁定 0.42.1。实现 Startup、SCRAM-SHA-256、SSLRequest/TLS、CancelRequest、Simple Query，以及 Parse/Bind/Describe/Execute/Flush/Sync/Close。每个 Session 限制 Statement、Portal 和执行并发；连接数、认证/查询/发送时限可配置。协议版本声明为 PostgreSQL 16 子集。

## 会话与目录

Startup database 可对应有效且授权的数据源 catalog_alias 或显式配置的业务虚拟库。未提供或显式传空字符串时使用 Startup user；非空名称精确保留，不重定向到 postgres 或其他源。凭据验证后校验数据库：不存在返回 FATAL/3D000，无 CONNECT 权限返回 FATAL/42501，禁止连接返回 FATAL/55000；错误密码/未知认证用户返回 FATAL/28P01。失败后关闭连接，不发送 ReadyForQuery。

账号库独立持久化，拥有空 public 与显式配置的 schema 映射；CONNECT 不授予源对象权限，映射对象仍取绑定和现有 SELECT grants 的交集。原数据源 database 保留独立入口。零数据源且未配置虚拟库时无可连接 PG database；显式配置的空业务虚拟库可以连接。内部管理库不公开，标准 pg_catalog/information_schema 保留系统查询能力，由客户端默认关闭的“显示系统对象”控制导航展示。

两段名称表示当前库的 schema.table；单段名称按当前有效 search_path 解析；当前数据库.schema.table 同样有效。跨 database 引用默认返回 0A000，显式启用 pgwire.allow_cross_database 才提供原联邦三段扩展。账号库默认 search_path 为 "$user", public，不把所有业务 schema 自动放入路径。原源 database 保留其配置默认 schema。支持引号精确匹配。SET/RESET search_path 推进命名空间版本，使旧 Prepared Statement 返回 0A000，需重新 Parse/Bind；Parse/Describe 不执行 SET，Execute 才生效。切库通过重新连接，不提供 USE。

统一参数注册表支持 search_path、application_name、client_encoding=UTF8、TimeZone=UTC、DateStyle=ISO/MDY、extra_float_digits、client_min_messages、standard_conforming_strings 等受限值；只读 server_version/server_version_num、server_encoding、integer_datetimes 反映兼容契约。未知或不支持参数/值明确报错，具体值约束见 `fedengine-pg-compat/src/session.rs`。

兼容目录包括 PG16 字段契约的 database/namespace/class/attribute/type、constraint/index/attrdef/description，以及 information_schema 的基础表列与约束视图。所支持的目录清单由 `fedengine-pg-compat/catalog/pg16-schema.json` 固定。部分辅助目录为空（例如没有采集的触发器、规则、分区和列分布统计），不能据此推断远端不存在这些能力。

支持客户端所需的 format_type、pg_get_constraintdef、pg_get_indexdef、pg_get_viewdef、pg_get_expr、obj_description、col_description、current_database/current_schema/current_schemas/current_setting、可见性和权限辅助查询。`pg_get_keywords()` 来自固定 PG16 基线。`pg_class.reltuples/relpages/relallvisible` 可返回刷新时采集的远端规划估计；MySQL 页数未知。它们不是实时行数或本地存储统计。

源数据库连接内，`pg_relation_size`（main/fsm/vm/init，支持 text 参数及表达式）、`pg_table_size/pg_indexes_size/pg_total_relation_size`、表及索引的 `pg_stat_get_numscans` 读取已发布 PostgreSQL 源统计并返回 int8；查询不访问远端。索引单体大小和扫描计数也绑定稳定索引身份。NULL/未知 OID 返回 NULL；可见已知关系的非法 fork 返回 22023。MySQL 的 PG 大小、虚拟库本地 foreign table 统计、未采集值及数据库大小保持有类型的 NULL。`pg_stat_all_tables/pg_stat_user_tables/pg_stat_all_indexes/pg_stat_user_indexes` 填入已采集计数，其他字段 NULL；`pg_stats` 仅公开 null_frac、avg_width、n_distinct 等非值指标，不公开 MCV/直方图业务值。具体字段与刷新边界见[管理 API](management-api.md)。

原始 SQL 在兼容改写前确定 ParameterDescription 与 Bind 契约；执行 SQL 另按 AST 压缩参数并映射回原槽。直接改写为 NULL 的辅助函数仍保留原参数并完整校验。`pg_database_size($1)` 的 unknown 参数按 PG16 默认解析为 name（OID 19），显式声明 oid 则保留 OID 26；未解析的参数间隙返回 42P18。支持 PG 允许的已声明但未使用槽。

regclass 名称按当前库、有效 search_path 和权限解析，支持引号和当前库三段名称。文本参数在 Bind 完成前解析成 OID，Execute 保留代际校验；文本结果输出可见简写名或正确引用的限定名，二进制输出 OID。不存在的必需关系名返回 42P01；to_regclass 对名称不存在、无效名称和数值 OID 越界返回 NULL，跨库仍返回 0A000。显式 regclass::text 使用同一作用域规则。regproc/regtype 的完整名称解析、重载和文本显示仍未完成，不能由 regclass 或目录 OID 编解码推断。

目录由统一已发布快照本地读取，并按 principal、database、授权对象过滤。不可见 FK 目标和不安全定义不会从标准目录或定义函数暴露。快照时间、stale 与分类状态仅通过管理 API 提供，不创建 PG 状态 schema；读取旧目录发送有界去重 Notice，业务查询仍服从 stale 阻断。管理 HTTP 位于既有可信部署边界，可展示全部登记状态。

系统查询使用 AST 受控规划。直接访问 `_fedengine`、任意系统/业务混合、危险函数及 SELECT INTO 明确拒绝；未知系统目录/表达式不自动透传远端。

## 类型与流控

支持原有标量 OID：bool、int2/int4/int8、float4/float8、numeric、text/varchar、bytea、date/time、timestamp/timestamptz、uuid、json，提供文本/二进制编解码，NULL 使用 -1 长度。numeric 二进制采用 base-10000，DuckDB 执行受 DECIMAL 精度限制；numeric 参数要求显式 DECIMAL(p,s) cast。时间按 UTC、微秒精度处理；每个克隆的执行连接独立设置 UTC，不依赖根连接配置继承。timestamptz 文本结果带 `+00`，timestamp 不附加时区。

PostgreSQL 源端未声明精度和 scale 的 `numeric`（包括生成列）存在经当前锁定扫描路径映射为 `DOUBLE` 的情况，JDBC 元数据和 wire 均按 `float8`（OID 701）展示；高精度源值会在读取时丢失有效数字。已验证的 `numeric(18,2)` 对照列保持 numeric（OID 1700）及精确值，不能由此推断其他精度范围均受支持。该源类型保真缺口留待后续扩展专项统一处理，任意精度数值执行仍按[剩余项方案](remaining-items-plan.md#51-精确-numeric)单独收口。

除法在授权和绑定后统一进行 AST 转换，prepare/describe/execute 使用相同表达式。整数除法向零截断并保持整数类型，浮点除法保持浮点结果；非 NULL 操作数除以零返回 `22012`，`NULL::integer/0` 返回 NULL。精确 numeric 除法尚未实现，DECIMAL 及引擎超宽整数参与的非浮点除法明确返回 `0A000`，避免以双精度浮点数冒充精确结果；除零仍优先报告 `22012`。该限制不等同于拒绝全部 numeric 编解码或其他运算，也不代表其他数值表达式已全面对齐 PG。

`decode(text,text)` 支持不区分大小写的 hex/base64/escape，返回 bytea（OID 17），NULL 参数返回 NULL；非法编码名及 hex/base64 内容返回 `22023`，非法 escape 内容返回 `22P02`。函数在授权后改写到私有实现，嵌套表达式与参数执行沿用同一路径；私有函数不能由用户 SQL 直接调用。

已收口的常见失败路径：非法整数字符串返回 `22P02`，数值越界返回 `22003`，不支持的 JSONB cast 返回 `0A000`；错误消息不透出原始 DuckDB SQL 或连接信息。数据库权限辅助函数的权限名按不区分大小写处理。

目录补充 unsigned oid/regclass/regproc、name、char、pg_node_tree、int2vector/oidvector 及所需的一维数组编码。vector 下标从 0 开始，普通数组编解码保留下界；尚不能忠实执行的非 1 下界返回 0A000。覆盖 NULL、空数组、转义、文本/二进制和非法输入。目录结果类型根据字段及表达式来源推导；无法证明类型的表达式要求显式 cast。任意业务数组、结构和多维复杂结果不因此获得全面支持。JSONB OID 仍明确拒绝。

Portal 的 maxRows 保留同一执行流；挂起后不重跑 SQL，也不预先收集整个结果。目录 worker 同样使用有界批次、本地读事务、取消与资源租约。错误后等待 Sync 恢复。同名命名 Statement/Portal 分别返回 `42P05`/`42P03`，不存在的 Statement 为 `26000`，Execute/Describe 不存在的 Portal 为 `34000`；Close 释放配额。autocommit 下 Sync 结束隐式事务并关闭 Portal；续读使用 Flush，在 Sync 前完成 Execute。

服务内 DDL 确认提交后先释放执行租约，再等待目录刷新；正常 CommandComplete 是新结构的可查询屏障。刷新失败/等待超时发送已提交 DDL 的成功响应和 Notice，不表示远端回滚；继续查询仍须遵守 stale 阻断。

CancelRequest 使用匹配后端 key 取消，不能推断远端写入回滚。取消监控持续至执行 worker 实际退出，跨内部 setup/describe/execute 语句重新发送中断，避免 DuckDB 重置单次中断后遗留后台计算；客户端收到 57014 不代表租约已经归还，checked shutdown 仍核对实际排空。应用须使用 autocommit；ReadyForQuery 的 Idle 不代表支持显式多语句事务。

## 实测范围

tokio-postgres 与原始协议覆盖真实 TLS、simple/extended、类型/OID、会话、Portal、取消和清理。PostgreSQL JDBC 42.7.8 已实际执行 catalogs/schemas/tables/columns/PK/FK/cross-reference/index/type 元数据方法；DBeaver CE 25.2.5 已实际浏览 PG/MySQL 目录、关系及数据，并执行跨源 SELECT。复现入口见[固定客户端验证](../tests/jdbc/README.md)，查询与基线夹具位于 `tests/fixtures/metadata/`。SQL 回放不能替代真实 GUI 验证。不承诺其他驱动版本、完整 psql 或完整 PostgreSQL 行为。

## 默认登录数据库

Startup 中 database 缺失或为空字符串时，取 Startup user 的精确名称；非空值不做空白裁剪或大小写转换。账号、源 catalog 与虚拟数据库各自独立，默认选择不创建数据库。配置示例、五个本地业务源绑定以及迁移/验收规则见[账号同名虚拟库方案](virtual-database-pg-alignment-plan.md)。

原始协议回归直接构造缺失、空值和显式 database 的 Startup 报文，覆盖认证优先、FATAL SQLSTATE 与 EOF，并验证拒绝后没有 AuthenticationOk/ReadyForQuery；不依赖驱动先补默认值。虚拟库内映射按外部表展示，远端索引/主外键不伪装为本地强制对象，来源事实继续由管理 API 与原源 database 契约提供。

在线虚拟库 API 可修改显式业务库和 schema 映射。登录会话绑定数据库 OID；源数据库或虚拟库删除后同名重建都不能接管旧连接，旧连接查询返回 3D000，需重新连接。虚拟库定义 revision 变化使旧 Prepared 在 Bind/Describe/Execute 显式失效，即使源 generation 相同；重新 Parse/Bind 后使用最新映射。撤销 CONNECT 或关闭新连接不会取消既有会话的 CONNECT 准入。发布屏障冲突、重试、删除策略和持久化优先级见[管理 API](management-api.md#在线业务虚拟库)。
