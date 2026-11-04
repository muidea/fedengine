# 固定客户端验证

`MetadataProbe.java` 实际调用 PostgreSQL JDBC 的十组 DatabaseMetaData 接口，输出 JSONL。使用 JDBC 42.7.8，jar SHA256 和 JDK/DBeaver 制品信息见 [clients.json](../fixtures/metadata/clients.json)。探针从 JDBC_USER/JDBC_PASSWORD 读取凭据，不输出凭据。

在专用一次性 PG16 库执行 [baseline.sql](../fixtures/metadata/baseline.sql)，在 MySQL 8.0.44 执行 [mysql8.sql](../fixtures/metadata/mysql8.sql)。通过管理 API 分别以 pg_baseline/sales 和 mysql_metadata/metadata 接入；为测试用户配置相应 SELECT grants。不要对业务数据库运行 fixture。

```sh
java --class-path /path/postgresql-42.7.8.jar tests/jdbc/MetadataProbe.java jdbc:postgresql://127.0.0.1:15432/pg_baseline
java --class-path /path/postgresql-42.7.8.jar tests/jdbc/MetadataProbe.java jdbc:postgresql://127.0.0.1:15432/mysql_metadata metadata
```

`NamespaceProbe.java` 另外验证同名 schema/table 隔离、两段/三段名称、search_path 顺序、带引号的 schema/table，以及 limited 用户的目录/FK/视图定义隐藏。在 PG 基线库执行 [namespace.sql](../fixtures/metadata/namespace.sql)，为 pg_baseline 纳入 sales、metadata、alternate、MixedCase，并保持默认 sales。limited 用户与全权限测试用户使用同一临时口令，但仅授予 baseline.sales.orders SELECT。

```sh
javac -d /tmp/fedengine-jdbc-probe tests/jdbc/MetadataProbe.java tests/jdbc/NamespaceProbe.java
java -cp /tmp/fedengine-jdbc-probe:/path/postgresql-42.7.8.jar NamespaceProbe jdbc:postgresql://127.0.0.1:15432
```

`LoadProbe.java` 用新的 Statement 连续执行给定业务 SQL，记录延迟与 SQLSTATE。它用于刷新并发的受控测量，不是完整负载测试：

```sh
java --class-path /path/postgresql-42.7.8.jar tests/jdbc/LoadProbe.java jdbc:postgresql://127.0.0.1:15432/pg_baseline 'SELECT id FROM sales.orders WHERE id=21' 500
```

运行产生的元数据结果、namespace 断言及环境快照放在被忽略的 `docs/validation/` 或仓库外；不提交 `*-actual.*`。PG16 直连基线与固定目录/查询夹具保留在 `tests/fixtures/metadata/`。客户端能力边界见[协议兼容说明](../../docs/protocol-compatibility.md)。临时 GUI 截图和验收报告不提交、不归档。目录 SQL 回放位于 `projection::tests::jdbc_catalog_statements_bind_and_describe`，不将业务 SQL 混入目录回放，也不以回放替代 GUI 证据。

虚拟 Server 目录验证使用 `ServerCatalogProbe.java`；参数为 catalog/schema1,schema2。账号业务库需包含 public 和所有已绑定本地 schema。探针检查各库 schema、getTables/getColumns（包括 FOREIGN TABLE）、源对象读数、内部对象拒绝、账号库不伪造本地 PK/索引，以及当前库三段引用。

```sh
javac -d /tmp/fedengine-jdbc-probe tests/jdbc/ServerCatalogProbe.java tests/jdbc/VirtualDatabaseProbe.java
java -cp /tmp/fedengine-jdbc-probe:/path/postgresql-42.7.8.jar ServerCatalogProbe jdbc:postgresql://127.0.0.1:15432 fedengine/public,insight_demo,postgres17,mysql57,mysql8,toolkit_mysql insight_demo/insight_demo PostgreSQL17/public MySQL57/lwltest MySQL8/LWLTEST toolkit_mysql/smart_factory pg_demo/sales mysql_demo/metadata
java -cp /tmp/fedengine-jdbc-probe:/path/postgresql-42.7.8.jar VirtualDatabaseProbe jdbc:postgresql://127.0.0.1:15432
```

VirtualDatabaseProbe 另验证驱动缺省登录、库内跨源只读 JOIN、search_path、默认拒绝跨 database 三段扩展，并输出映射 OID 供重启/刷新前后比对。探针不输出凭据。DBeaver 默认关闭“显示系统对象”；“显示所有数据库”可独立开启。导航显示偏好不改变标准元数据事实。
