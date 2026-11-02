# DDL 能力矩阵

生产启用必须同时满足 grants、源可写、ddl_enabled、namespace allowlist、全局 DDL 开关、精确能力记录。记录匹配 DuckDB 核心、已加载扩展和远端版本；启动仅查询版本，不对业务源执行破坏性探测。

| 操作 | 内部路径 | 门禁 / 边界 |
|---|---|---|
| CREATE/DROP SCHEMA | DuckDB 原生扩展 | 明确 namespace 授权；DROP 默认禁用；无 CASCADE |
| CREATE/DROP TABLE | DuckDB 原生扩展 | 内置标量和约束白名单；无临时表及任意选项 |
| CREATE TABLE AS SELECT | DuckDB 原生扩展 | CREATE_TABLE_AS 单独能力；读取依赖也必须同源 |
| ALTER TABLE | DuckDB 原生扩展 | ALTER_TABLE 加细分动作能力；禁止跨 namespace 移动和任意变种 |
| CREATE INDEX | 内部生成 postgres_execute/mysql_execute 模板 | 普通/唯一 B-tree，标识符严格引用；无表达式/部分/排序选项 |
| DROP INDEX | 查询真实远端目录并生成模板 | 必须唯一定位表；DDL 锁内再验证；保护索引拒绝 |

ALTER 细分能力为 `ALTER_TABLE_ADD_COLUMN`、`ALTER_TABLE_DROP_COLUMN`、`ALTER_TABLE_RENAME_COLUMN`、`ALTER_TABLE_COLUMN_TYPE`、`ALTER_TABLE_COLUMN_DEFAULT`、`ALTER_TABLE_COLUMN_NULLABILITY`。普通索引要求 `CREATE_INDEX` 与 `CREATE_INDEX_BTREE`，唯一索引要求 `CREATE_INDEX` 与 `CREATE_INDEX_UNIQUE_BTREE`。`index_adapter=true` 也是必需条件。

默认配置不包含能力记录。测试数据库中的能力记录只是验收输入，不能作为其他源的授权。把对应测试结果中的精确版本及证据路径填写到配置后才开放已验证的动作。当前 PostgreSQL 能删除普通和独立唯一索引；MySQL 保守保护全部唯一索引（包括 PRIMARY），不开放其 DROP。MySQL ALTER 新增字符串列使用显式 VARCHAR 长度，例如 `VARCHAR(255)`，当前扩展对无长度 VARCHAR 会向远端发送无效语法。

DDL 成功后增加源代际并调用 `postgres_clear_cache()` / `mysql_clear_cache()`。缓存失败记录已成功事实并将源标记 stale；结果未知不会自动重试。相同名字的 MySQL 索引若在多个表中出现，将因归属歧义拒绝 DROP。IF EXISTS/IF NOT EXISTS 的未验收变体不会自动推广为幂等承诺。

具体版本组合需在目标环境运行 `scripts/test-federation.sh` 验证；未单独验收的变种保持能力门禁关闭。

当前 MySQL 原生 CREATE TABLE 会把无长度 VARCHAR 映射为 TEXT；普通 B-tree 索引模板不提供 TEXT/BLOB prefix length。为该类列创建索引需另行验收，不属于当前普通标量索引的保证范围。联调索引使用 INTEGER 列，并从真实远端目录核对存在、删除和唯一性约束。
