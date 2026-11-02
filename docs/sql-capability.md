# SQL 能力与边界

使用 sqlparser 的 DuckDB AST 解析整条 SQL，明确绑定注册源的 catalog/schema/object。未识别语法或能力一律拒绝；不会按前缀或字符串分号判断安全性。

| 范围 | 行为 |
|---|---|
| SELECT、聚合、CTE、子查询、JOIN | 可跨 PostgreSQL/MySQL；函数为显式安全白名单 |
| INSERT/UPDATE/DELETE | 完整对象和依赖必须属于一个注册源；使用官方扩展原生集合操作 |
| 无主键表 | 不自行逐行拼装写入，原生路径的重复行、NULL 与影响行数由集成测试验证 |
| 多语句 Simple Query | 仅允许全为只读 SELECT；带写入、DDL 或事务时整条消息执行前拒绝 |
| 参数 | 显式 PG OID 或 SQL cast；无法推断返回 42P18；不猜测未知类型 |
| 显式事务 BEGIN/COMMIT/ROLLBACK | 0A000；ReadyForQuery 保持 Idle |
| COPY、RETURNING、临时对象、视图、任意管理 SQL | 拒绝 |
| 文件函数、远端执行函数、INSTALL/LOAD/ATTACH | 业务 SQL 拒绝；仅内部受控初始化/索引模板可用 |
| pg_catalog / information_schema | 不开放任意系统目录访问；驱动复杂元数据发现未承诺兼容 |

跨源写入、CTAS、外键依赖在远端执行前拒绝。CREATE TABLE 只接受内置标量列类型和明确白名单选项，ALTER 逐动作验收。非唯一对象名、未知源、未授权 namespace 都会拒绝。

MySQL UPDATE/DELETE 的准确行数从扩展返回的 Count 结果读取；DuckDB C API 的 rows_changed 对当前 MySQL 扩展可能返回 0，不能直接作为 PostgreSQL CommandComplete 的行数。执行不会为计算行数重复发送写入。

单语句写入不等于全局事务。不同源没有全局一致快照；不承诺跨源原子提交。MySQL 表须使用支持事务的引擎（验收使用 InnoDB）。触发器、外键并发、非事务存储引擎和故障时原子性仍需按实际版本专项验收。
