# PostgreSQL 协议兼容

pgwire 锁定 0.42.1。实现 Startup、SCRAM-SHA-256、SSLRequest/TLS、CancelRequest、Simple Query，以及 Parse/Bind/Describe/Execute/Flush/Sync/Close。每个 Session 对 Statement、Portal 与执行并发进行限制；连接数、认证时限、查询时限、发送时限可配置。

Portal Execute 的 maxRows 限制使用保留的执行流；挂起后继续原游标，不重新运行 SQL，也不把整个结果集预先收集到 Vec。错误后按协议等待 Sync 恢复。取消由独立 CancelRequest 连接和匹配后端 key 触发，不代表远端写入回滚。

支持标量 OID：bool、int2/int4/int8、float4/float8、numeric、text/varchar、bytea、date/time、timestamp/timestamptz、uuid、json。提供文本/二进制编解码，NULL 使用 -1 长度。numeric 二进制采用 base-10000，保留精度与 scale；DuckDB 执行本身仍限其 DECIMAL 精度。numeric 参数要求显式 DECIMAL(p,s) cast，不隐式选择可能损失精度的 scale。无法映射的数组、结构等复杂结果类型返回 0A000。

时间按 UTC 处理；时间戳执行精度为微秒，DuckDB 纳秒结果转换到微秒。结果中的 OID 与对应 codec 一致。配置的用户密码从环境读取并在启动时生成带随机盐的 SCRAM verifier。

实测客户端为 tokio-postgres 与原始协议报文（含真实 TLS）。未完成 JDBC、psql、GUI 元数据查询和所有 PostgreSQL catalog/SET 兼容矩阵。应用须使用 autocommit，不可把 ReadyForQuery 的 Idle 视作支持显式多语句事务。

同名的命名 Statement/Portal 分别返回 42P05/42P03；Close 释放配额。未开启显式事务时 Sync 结束隐式事务并关闭全部 Portal；需要继续挂起 Portal 时使用 Flush，在 Sync 前完成 Execute。JSONB OID 当前明确拒绝，避免把带版本字节的 JSONB 二进制当作 JSON 文本。
