# 在线数据源管理 API

默认配置使用 `./data/fedengine.duckdb`，管理入口为 `http://127.0.0.1:15433/api/v1`。业务 SQL 继续使用 PGWire。管理入口属于可信部署边界，不增加账号、角色或数据授权接口；跨机器部署时由部署侧控制入口访问。既有 SQL 授权和 DDL capability 校验继续生效，可在启动配置中为将来登记的 source id 预置既有 grants。

`[management]` 控制启用、监听地址、凭据目录、操作队列上限、并发数、排空等待和 HTTP 等待时限。启用时禁止 `:memory:`；关闭时仍可使用内存实例。队列满返回 `503`。排空超时不会强制卸载，操作保持 `WaitingForDrain`，关闭或完成挂起 Portal 后继续执行。

## 首次导入与存储

当前开发阶段只支持当前存储结构，不提供历史数据迁移或旧格式回退；不匹配的版本明确拒绝。规约见 [开发说明](development.md#当前阶段的数据兼容规约)。

`[[datasources]]` 只在管理库第一次初始化时导入；空列表也会写入完成标记。之后以管理表为准，编辑 TOML 不会覆盖在线配置或复活已删除的源。联邦配置示例使用独立的 `./data/federation.duckdb`，避免与已初始化的默认零源数据库混淆。

部署时将数据库文件及其 WAL 放在同一持久化卷中，同时保留凭据目录。不要同时启动两个进程使用同一管理库。下载扩展、临时文件、管理数据库和凭据不纳入 Git。

凭据引用兼容环境变量名，也支持 `file:crm.dsn`。文件路径相对于 `management.credentials_directory`，拒绝绝对路径、上级目录和逃出目录的符号链接。文件保存完整连接串，每次探测或挂载重新解析，不写入管理表或操作结果。只编辑文件不会切换当前连接；凭据变更应显式 PATCH 同一引用再 apply，或对仍在运行的源重新 apply 已生效版本，创建新 revision 并重新解析。未变更定义的 Disabled 源恢复复用原 revision 和连接身份，不作为凭据轮换操作；若停用期间修改凭据，恢复前也须显式 PATCH。恢复上一版本需要其引用仍可解析；恢复失败时源进入 `Degraded` 并关闭准入。

## 可复制调用

以下命令从仓库根目录执行，示例依赖 `curl`、`jq`。预先将完整 PostgreSQL 连接串保存到 `secrets/crm.dsn`，保护文件读取权限，并按 `docs/development.md` 准备匹配版本的扩展。自动安装继续由 `duckdb.install_extensions` 控制，在线登记不会自行开启。

```sh
base=http://127.0.0.1:15433/api/v1
curl -sS -X POST "$base/sources" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: register-crm-1' \
  -d '{"id":"crm","kind":"postgres","catalog_alias":"pg_crm","secret_ref":"file:crm.dsn","default_schema":"public","allowed_schemas":["public"],"read_only":false,"ddl_enabled":false}'
# 返回操作记录，其中 id 是操作编号；登记完成后状态为 Registered。
curl -sS "$base/operations/替换为返回的操作编号"
curl -sS "$base/sources/crm"

# 后续写请求同时比较 incarnation 与 desired_revision。
target=$(curl -sS "$base/sources/crm" | jq '{incarnation,expected_revision:.desired_revision}')
body=$(jq -n --argjson target "$target" '{target:$target}')
curl -sS -X POST "$base/sources/crm/discover" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: discover-crm-1' -d "$body"
# 等待发现操作完成；result_schemas 是发现结果，不会自动纳入。

# 完整提交绑定。只纳入已经存在的 schema；null 表示显式清除默认绑定。
body=$(jq -n --argjson target "$target" '{target:$target,bindings:[{remote_schema:"public",enabled:true}],default_schema:"public"}')
curl -sS -X PUT "$base/sources/crm/schemas" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: schemas-crm-1' -d "$body"
# 等待该操作成功后读取新的 revision，再激活。
target=$(curl -sS "$base/sources/crm" | jq '{incarnation,expected_revision:.desired_revision}')
body=$(jq -n --argjson target "$target" '{target:$target}')
curl -sS -X POST "$base/sources/crm/apply" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: apply-crm-1' -d "$body"
curl -sS "$base/sources/crm/catalog?limit=100&offset=0"
```

MySQL 登记使用 `kind:"mysql"`，连接串必须包含明确的 `database`，`default_schema` 与所选 schema 必须是该 database 名称。发现仅返回当前 database；不能通过该数据源纳入同服务器上的其他 database。SQL 使用 `catalog_alias.database.table`。

## 请求与结果

所有写接口要求 `Idempotency-Key`。同键同参数返回原操作，同键不同参数返回 `409`，已完成的失败操作也不会被同键重放。重试新意图时使用新键并重新读取目标版本。除登记外，请求体都包含 `target:{incarnation,expected_revision}`。源 id 和别名复用后，旧 incarnation 仍然冲突。

| 请求 | 请求体中除 target 外的字段 | 结果 |
| --- | --- | --- |
| `POST /sources` | 完整源定义，见上例；enabled 不会自动激活新登记 | 登记操作 |
| `GET /sources`、`GET /sources/{id}` | 无 | 配置、版本、实际状态、当前操作、最近错误、目录时间 |
| `PATCH /sources/{id}` | `secret_ref` | 新期望版本，不立即切换 |
| `POST /sources/{id}/probe` | 无 | 连接探测操作 |
| `POST /sources/{id}/discover` | 无 | 发现操作，结果在 `result_schemas` |
| `PUT /sources/{id}/schemas` | `bindings:[{remote_schema,enabled}]`、`default_schema`（字符串或 null） | 新期望版本，显式 apply 后生效 |
| `POST /sources/{id}/apply` | 无 | 激活或应用期望版本；运行中重应用当前版本创建新 revision，未变更定义的 Disabled 恢复复用原 revision |
| `POST /sources/{id}/disable` | 无 | 保存停用意图并等待真实排空 |
| `POST /sources/{id}/refresh` | 无 | 清理扩展缓存，保存并发布完整目录 |
| `GET /sources/{id}/catalog` | 查询参数 `schema`、`object`、`object_type`、`include`、`snapshot_version`/`snapshot_id`、`page_token`、`offset`、`limit`（1–1000） | 分页对象、完整 schema 范围、版本、generation 与 stale 标记 |
| `DELETE /sources/{id}` | 无 | 删除已停用的登记，保留历史，不执行远端 DROP |
| `GET /operations/{id}` | 无 | 操作状态、阶段、结果及脱敏错误 |

写请求返回 `202` 前，操作和期望状态已经提交；必须读取操作至 `Succeeded` / `Failed` / `Interrupted` 才能确认结果。读请求返回 `200`；输入错误为 `400`，不存在为 `404`，版本/幂等/状态冲突为 `409`。接口超时或 HTTP 连接断开不代表远端工作已取消，可用原幂等键找回操作。操作完成前，同一源不能接受其他变更或探测。

目录包括表/视图、列序号、远端类型、映射类型、可空性及索引定义；无法映射的类型和特殊索引通过 `supported:false` 明确展示。刷新仅扫描有效绑定范围。结构变化会使旧 Prepared Statement 失效，需要重新 Parse/Bind。服务确认远端 DDL 成功后立即标记旧绑定失效，释放执行 worker 的源锁与实例租约，再等待同一刷新流程发布结果。正常情况下 CommandComplete 后可以立即查询新结构；等待受 management.request_timeout_ms 和语句剩余期限限制。刷新失败或超时仍返回已提交的 DDL 成功，并发送明确 Notice；源保持 stale 或继续后台刷新，可通过管理状态/refresh 恢复，不能重放 DDL。

停用 schema 通过提交完整 bindings 并 apply 完成；移出时从 bindings 删除。停用当前默认 schema 时必须指定另一个启用的默认值或传 null，未指定默认绑定的短名称不会回退到本地 DuckDB。停用/移除仅改变登记和挂载，远端数据保持原样。

## 关系元数据与一致分页

基础对象字段继续保留 `schema/name/object_type/columns/indexes`。`metadata` 包含本地稳定 OID、远端 source_id、列 attnum/default/identity/generated/comment、PK/UNIQUE/FK/CHECK、索引键项/INCLUDE/排序/谓词、视图定义及分类状态。`remote_type` 保留来源类型，`mapped_type` 是锁定扩展实际 DESCRIBE 的结果，二者不混用。

```sh
curl -sS "$base/sources/crm/catalog?schema=public&object=orders&include=core,column_properties,constraints,indexes,comments,view_definitions,statistics,storage,activity,column_statistics,status&limit=1"
```

`schema` 与 `object` 是独立参数，必须按 URL 规则编码。`include` 可选择上述类别，省略时返回完整对象；基础对象和列列表始终返回，未选择的扩展字段从 JSON 中移除。选择 `column_properties` 返回列属性整体，其中也可能含列注释。对象详情、约束、索引和状态范围读取使用相同端点与 object/include 组合，不另建一套事实服务。

返回 `snapshot_id`、同值的 `snapshot_version`、refreshed_at、revision、generation、stale、total、offset、objects 及 `next_page_token`。最后一页 token 为 null。续页须带回原 schema/object/object_type/include 过滤条件及 token，可以调整 limit；token 绑定 source、过滤条件、snapshot 与下一 offset。它不承载凭据，也不是授权令牌。

```sh
curl -sS --get "$base/sources/crm/catalog" \
  --data-urlencode 'schema=public' \
  --data-urlencode 'include=core,status' \
  --data-urlencode 'limit=100' \
  --data-urlencode "page_token=$next_page_token"
```

跨页变更过滤条件、请求旧 snapshot_version 或发布了新结构均返回 HTTP `409` / SQLSTATE `40001`，调用者从第一页重新读取。非法 token/include 返回 `400`。无变化刷新复用 snapshot_id，只更新 freshness；不能用时间变化推断结构版本改变。旧快照、失败扫描和已停用源的 HTTP 管理状态仍可观察，PG 可连接范围另按有效源和会话权限限制。

每类状态包含 `support`（supported/partial/unsupported/unknown）、`scan_state`、complete、stale、尝试/成功时间、脱敏 error_code 和 unsupported_fields。未扫描或能力不足不会伪装成功空集；失败/不完整扫描保留旧事实并标记状态。comments 与 view_definitions 从组合 details/列属性采集派生。

表达式保留 `source_text/source_dialect`，只有安全、可展示的 PG 文本才填 `pg_text`。MySQL 生成式、表达式索引等保留原始事实，未实现 PG 翻译的部分标 partial；标准 PG 目录返回 NULL。默认值/定义涉及不可安全公开的用户函数或对象时也可能为 NULL，不代表远端没有定义。MySQL 5.7/8 的文本（含空字符串）、整数及普通十进制字面量默认值按类型转换；函数默认值、二进制/时间默认值、生成式仍保守保留原文，未证明的 PG 文本为 NULL。

`metadata.statistics` 采集远端目录的 `estimated_rows/pages/all_visible_pages`，PG 目录投影至 `pg_class.reltuples/relpages/relallvisible`。PG 的 -1 行数估计保留其“未统计”含义；MySQL 的 table_rows 是存储引擎估计，页数未知并标记 partial/page_counts。

`include=storage` 返回 `metadata.storage`：PostgreSQL 的 `main_bytes/fsm_bytes/vm_bytes/init_bytes/table_bytes/indexes_bytes/total_bytes` 来自对应的源端大小函数；MySQL 的 `data_length/index_length/data_free` 来自 information_schema.tables，保留存储引擎自身含义，不能加总为独占空间或转换为 PG relation fork 大小，PG 大小字段保持 NULL，状态为 partial/postgresql_storage_sizes。

`include=activity` 返回 `metadata.activity`：PostgreSQL 的 `seq_scan/seq_tup_read/idx_scan/idx_tup_fetch/n_tup_ins/n_tup_upd/n_tup_del/n_tup_hot_upd/n_live_tup/n_dead_tup` 来自 pg_stat_all_tables，计数随源端重启或统计重置可能减少；MySQL 此分类为 unsupported/not_scanned，字段保持 NULL。没有统计行的对象也保留 NULL。

以上均为 refresh 时的源端快照，不执行 COUNT、业务扫描或 ANALYZE，不代表 fedengine 本地 DuckDB 占用或实时查询次数。未采集的事实保留 NULL；各分类独立发布，采集失败保留旧事实、last_success_at 并标 stale；成功采集到 NULL 可清除旧值。统计值变化也会产生新 snapshot_id，即使业务结构未变。

`include=indexes` 同时返回各索引的 `storage`（与表相同的七项 PG 大小）和 `activity`（idx_scan/idx_tup_read/idx_tup_fetch）。源数据库连接中的 `pg_relation_size(regclass[,text_fork])/pg_table_size/pg_indexes_size/pg_total_relation_size` 和表/索引 `pg_stat_get_numscans(oid)` 读取已发布、经 SELECT 权限过滤的统计。fork 支持 main/fsm/vm/init 及 text 参数、表达式；可见已知关系的非法 fork 返回 22023，NULL/未知 OID 返回 NULL。虚拟库映射是本地 foreign table，不把源端物理统计伪装成本地统计；数据库总大小仍未知。

`include=column_statistics` 返回 `metadata.column_statistics`，按列名保存 PostgreSQL pg_stats 的 null_frac、avg_width、n_distinct。未授权、未统计或未采集的指标保留未知；MySQL 为 unsupported/non_value_column_statistics。标准 `pg_stat_all_tables/pg_stat_user_tables/pg_stat_all_indexes/pg_stat_user_indexes` 填入上述已采集的远端计数，其余字段 NULL；`pg_stats` 只公开非值指标，most_common_vals/histogram_bounds 等包含业务值的字段不采集、不公开。源端列统计不存在时不伪造统计行。

当前结构持久化连接身份 epoch。默认 `management.mysql_identity_policy="preserve"`：无变化刷新、重启、仅修改 schema/default 绑定以及未变更定义的 disable→apply 保留存活身份。删除列保留编号墓碑，PG 可观察的对象删除重建分配新身份；运行中重 apply 或显式 PATCH 凭据后 apply 切换 epoch。MySQL 缺少可靠源身份，分类状态明确标记 partial/offline_identity_continuity_unproven。

可选 `management.mysql_identity_policy="strict"` 在 MySQL 重启、停用恢复和 MetadataStale 后恢复刷新时切换 revision/epoch，使旧 Prepared 与旧对象身份失效；严格切换失败关闭准入，不恢复旧身份继续提供业务查询。该模式整体切换身份，不能精确识别哪些表在离线期间同名重建。默认策略不变，两种策略均有真实数据库回归。

采集在前后两次基础结构枚举发现 DDL 漂移时最多重试三次；持续漂移返回冲突并保留旧发布。多次远端调用不构成全局一致事务。当前默认仍为手动 refresh 和服务内 DDL 后刷新，不启用周期扫描。

账号业务虚拟库及 schema 映射可通过 TOML virtual_databases 或下述在线管理 API 显式维护。业务源 HTTP 目录继续描述远端事实，不将本地映射对象当成新数据源。见[账号虚拟库方案](virtual-database-pg-alignment-plan.md)。

## 在线业务虚拟库

与 sources 共用既有可信管理 HTTP 边界。虚拟库变更是本地同步发布事务，成功返回 `200` 和持久化回执，不创建远端异步 source operation，不执行远端 SQL。

| 接口 | 请求/返回 |
| --- | --- |
| `GET /virtual-databases` | 所有有效虚拟库定义、OID、revision、schema 映射与快照指针 |
| `GET /virtual-databases/{name}` | 单个有效虚拟库；不存在返回 `404` |
| `POST /virtual-databases` | 完整 `VirtualDatabaseConfig`；创建独立业务库 |
| `PUT /virtual-databases/{name}` | `{ "target": { "oid": 200000000, "expected_revision": 1 }, "definition": { ... } }`；完整替换，不能改名 |
| `DELETE /virtual-databases/{name}` | `{ "target": { "oid": 200000000, "expected_revision": 1 } }`；删除本地虚拟库和投影 |

所有变更必须携带 `Idempotency-Key`（1～128 个可打印 ASCII 非空白字符）。回执包含 `idempotency_key/name/oid/revision/deleted`。同键同请求返回历史提交回执，重放删除或创建不会恢复旧对象；同键不同请求、OID 或 revision 不匹配返回 `409/40001`。幂等键在虚拟库接口内独立保存；请求超时或断开不能推断回滚，应保留原键重试，并 GET 当前状态核对。完全相同的 definition 替换保留 revision，其余定义变化递增 revision；删除回执使用下一 revision，同名重建分配新 OID、revision 从 1 开始。

```sh
curl -sS -X POST "$base/virtual-databases" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: create-account-v1' \
  -d '{"name":"account","owner":"fedengine","connect":[],"allow_connections":true,"schemas":[{"name":"business","source":"crm","remote_schema":"public"}]}'
```

owner/connect 必须是已配置的 PG 账号；本地库与 schema 名称不超过 63 字节，系统命名空间与重复映射拒绝。源必须已登记且映射在其 desired 启用 schema 绑定内；远端源未激活时可以保留映射，但只在有效快照及 SELECT 授权交集内对 PG 可见。CONNECT 和 owner 不授予源 SELECT，虚拟库映射仍只读。每库最多 256 个映射、最多 256 个有效虚拟库；幂等回执最多 10,000 条、覆盖/墓碑最多 4,096 个名称，达到限额返回 `503/54000`，不静默清除回执或复用身份。

发布遵循实例写屏障 → 存储 writer → 本地事务的顺序。活跃 worker 或悬停 portal 持有实例读租约时，返回 `409/55P03`，不等待客户端，也不写入定义或回执；排空/关闭 portal 后使用原键重试。该屏障覆盖整个实例，其他数据库上的活跃执行也可能导致冲突。定义、身份、覆盖/墓碑、目录投影及回执原子提交，阻塞 I/O 任务持有屏障和 ServiceLease 至实际退出；HTTP 等待者消失不会提前释放屏障。

PG 登录绑定当时数据库 OID。定义变化后，旧 Prepared 的 Bind、Describe、Execute 返回 `0A000`，新 SQL 使用新映射；源 generation 不变也会失效。CONNECT 撤销和 allow_connections=false 仅限制新登录，已接入连接保留 CONNECT 准入，源对象权限仍逐次检查。删除虚拟库后旧连接查询返回 `3D000`；同名重建也不能重新绑定，必须重新连接。这是管理 API 的删除策略，不提供 PostgreSQL `DROP DATABASE`/FORCE 的完整会话终止语义。

当前结构保存虚拟库覆盖与回执，revision 为必需字段，不读取缺失 revision 的历史格式。API 接管的同名定义优先于 TOML，删除墓碑阻止 TOML 在重启时复活该库；未被 API 接管的定义继续按 TOML 重启对齐。API 覆盖不提供在线清除接口，需要继续变更时使用 API。部署前正常停服务并备份管理库和配置；程序回退必须使用匹配的管理库备份。

## 目录刷新与资源预算

普通刷新在独立的 DuckDB 采集实例内完成远端枚举、类型映射和扩展事实采集，不修改正式实例的挂载或缓存。旧的有效目录在采集和本地准备期间仍可读取；已标记 stale 的源继续服从原有业务阻断规则。Apply/Disable 仍关闭目标源准入并排空，正式 ATTACH/DETACH 和缓存清理保留短实例独占窗口。

远端事实使用压缩暂存供本地重试，每次准备只解码一份新目录，不额外保留完整事实图。目录准备使用目标源当前快照和批量身份索引，在独立本地事务中生成不可见投影；普通状态更新只读写管理记录与目录头。对象事实按对象压缩保存，分页与计数使用独立的快照/schema/名称/种类列筛选后才解码命中事实，禁止对全量 JSON 做目录筛选。当前存储版本为 6，仅接受新库或同版本初始化，不迁移旧库。准备结束后以非排队方式尝试实例屏障，复核源代际后提交，并在释放屏障前更新运行注册表。提交任务持有屏障、内存额度和服务租约直到实际退出；之后由另一个受管任务执行非强制 checkpoint，并保留 writer、内存额度和准备阶段服务租约到其实际退出。文件库自动 checkpoint 阈值为 1GB；主动 checkpoint 避开实例屏障，冲突时保留已提交 WAL，后续发布重试或由自动阈值兜底，不回滚已确认提交。准备失败或取消回滚事务；代际变化拒绝过期结果，保留旧快照并明确标记失败。准备事务持有 writer 时只允许最多 100ms 的非排队尝试；其间持实例屏障的其他写入必须立即尝试 writer，禁止排队等待。超过窗口先回滚再退让，最多进行三次本地准备，不重复远端采集。持续冲突返回操作失败 `55P03`，重试需要新管理操作键。

虚拟库变更仍遵循实例屏障 → writer → 本地事务；writer 忙时立即返回 `409/55P03`，可用原 Idempotency-Key 重试，不持屏障排队等待目录准备。查询绑定的关系名称按不可变快照共享缓存，最多保留 16 份/总计 100k 个名称；目录头与新快照名称索引在准备阶段生成，成功提交后在屏障内切换内存视图，普通状态/虚拟库写入同样更新视图；查询逐次使用最新提交目录头，复核权限、库修订与源代际，不缓存授权结论。查询及挂起 Portal 的真实租约不被强制释放。等待实例空闲的管理操作通过非排队尝试进行退让，不在读写锁内排队阻挡后续查询。

`management.metadata_memory_budget_mb` 默认 512，`metadata_operation_memory_mb` 默认 256；操作先预留完整额度，并受 `max_concurrent_operations` 共同约束。独立采集实例的 DuckDB memory_limit 为操作额度的一半且最高为 128MiB，核心与分类采集以及累计序列化事实的字节限额为操作额度的八分之一，超限以 `54000` 失败，不发布截断目录。字节限额不等于进程 RSS 硬上限；Rust 容器、正式实例、分配器与原生库内存仍需通过独立 RSS 测量和部署资源限制管理。
