//! Disposable PostgreSQL/MySQL integration. Never point the DSNs at business databases.
mod common;
use common::TestDirectory;
use fedengine_app::FedEngine;
use fedengine_core::*;
use std::{collections::BTreeSet, time::Duration};
use tokio_postgres::NoTls;
#[tokio::test]
#[ignore = "requires isolated databases and extension downloads; run scripts/test-federation.sh"]
async fn remote_crud_federation_ddl_index_generation_and_zero_cross_source_effects() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("fedengine=info")
        .with_test_writer()
        .try_init();
    let temporary = TestDirectory::new();
    let mut config = Config::load("examples/config/federation.toml").unwrap();
    config.pgwire.listen = "127.0.0.1:0".into();
    config.users[0].password_secret_ref = "USER".into();
    config.duckdb.install_extensions = false;
    config.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    config.duckdb.max_workers = 4;
    config.duckdb.threads = 2;
    config.duckdb.temp_directory = temporary.path().to_string_lossy().into_owned();
    config.ddl.allow_drop_schema = true;
    config.datasources[1]
        .allowed_schemas
        .push("fedengine_test_ns".into());
    config.grants[1]
        .actions
        .extend(["CREATE_SCHEMA".into(), "DROP_SCHEMA".into()]);
    // Discover the exact tuple in a separate in-memory admin instance, then enable tested operations.
    let admin = duckdb::Connection::open_in_memory().unwrap();
    admin
        .execute_batch(&format!(
            "SET extension_directory={}; LOAD postgres; LOAD mysql",
            quote_literal(config.duckdb.extension_directory.as_ref().unwrap())
        ))
        .unwrap();
    for source in &config.datasources {
        let ext = source.kind.extension();
        let dsn = std::env::var(&source.secret_ref).expect("isolated test DSN required");
        admin
            .execute_batch(&format!(
                "ATTACH {} AS {} (TYPE {})",
                quote_literal(&dsn),
                quote_identifier(&source.catalog_alias),
                ext
            ))
            .unwrap();
        admin
            .execute_batch(&format!(
                "DROP TABLE IF EXISTS {}.{}.fedengine_test_items",
                quote_identifier(&source.catalog_alias),
                quote_identifier(&source.default_schema)
            ))
            .unwrap();
        let extension_version: String = admin
            .query_row(
                "SELECT extension_version FROM duckdb_extensions() WHERE extension_name=?",
                [format!("{ext}_scanner")],
                |r| r.get(0),
            )
            .unwrap();
        let version_sql = if source.kind == DataSourceKind::Postgres {
            "SELECT current_setting('server_version') AS version"
        } else {
            "SELECT VERSION() AS version"
        };
        let remote_version: String = admin
            .query_row(
                &format!(
                    "SELECT version FROM {ext}_query({}, {})",
                    quote_literal(&source.catalog_alias),
                    quote_literal(version_sql)
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        eprintln!(
            "verified tuple candidate: source={} duckdb=v2.0.0 extension={} remote={}",
            source.id.0, extension_version, remote_version
        );
        config.capabilities.push(CapabilityRecord {
            source: source.id.clone(),
            duckdb_version: "v2.0.0".into(),
            extension_version,
            remote_version,
            evidence: "tests/federation.rs disposable-database execution".into(),
            operations: BTreeSet::from_iter(
                [
                    "CREATE_SCHEMA",
                    "DROP_SCHEMA",
                    "CREATE_TABLE",
                    "CREATE_TABLE_AS",
                    "ALTER_TABLE",
                    "ALTER_TABLE_ADD_COLUMN",
                    "ALTER_TABLE_DROP_COLUMN",
                    "CREATE_INDEX_BTREE",
                    "CREATE_INDEX_UNIQUE_BTREE",
                    "DROP_TABLE",
                    "CREATE_INDEX",
                    "DROP_INDEX",
                ]
                .map(String::from),
            ),
            index_adapter: true,
        });
    }
    let engine = FedEngine::start(config).await.unwrap();
    let mut connection_config = tokio_postgres::Config::new();
    connection_config
        .host("127.0.0.1")
        .port(engine.address().unwrap().port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap());
    let (client, connection) = connection_config.connect(NoTls).await.unwrap();
    let driver = tokio::spawn(async move { connection.await.unwrap() });
    for (alias, schema) in [("pg_crm", "public"), ("mysql_erp", "erp")] {
        let table = format!("{alias}.{schema}.fedengine_test_items");
        client
            .simple_query(&format!(
                "CREATE TABLE {table}(id INTEGER PRIMARY KEY, name VARCHAR, amount INTEGER)"
            ))
            .await
            .unwrap();
        let insert = client
            .prepare_typed(
                &format!("INSERT INTO {table} VALUES($1,$2,$3)"),
                &[
                    tokio_postgres::types::Type::INT4,
                    tokio_postgres::types::Type::TEXT,
                    tokio_postgres::types::Type::INT4,
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            client
                .execute(&insert, &[&1i32, &"Alice", &10i32])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            client
                .execute(&insert, &[&2i32, &"Bob", &20i32])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            client
                .execute(&format!("UPDATE {table} SET amount=30 WHERE id=1"), &[])
                .await
                .unwrap(),
            1
        );
        let row = client
            .query_one(&format!("SELECT name,amount FROM {table} WHERE id=1"), &[])
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "Alice");
        assert_eq!(row.get::<_, i32>(1), 30);
        let prepared = client
            .prepare(&format!("SELECT * FROM {table}"))
            .await
            .unwrap();
        client
            .simple_query(&format!("ALTER TABLE {table} ADD COLUMN note VARCHAR(255)"))
            .await
            .unwrap();
        assert_eq!(
            client
                .query(&prepared, &[])
                .await
                .unwrap_err()
                .code()
                .unwrap()
                .code(),
            "0A000"
        );
        client
            .simple_query(&format!("ALTER TABLE {table} DROP COLUMN note"))
            .await
            .unwrap();
        client
            .simple_query(&format!(
                "CREATE INDEX fedengine_test_amount_idx ON {table}(amount)"
            ))
            .await
            .unwrap();
        // Verify a real remote index, not a DuckDB-local catalog artifact.
        let metadata = if alias == "pg_crm" {
            "SELECT count(*) FROM postgres_query('pg_crm', 'SELECT indexname FROM pg_indexes WHERE indexname=''fedengine_test_amount_idx''')"
        } else {
            "SELECT count(*) FROM mysql_query('mysql_erp', 'SELECT index_name FROM information_schema.statistics WHERE table_schema=''erp'' AND table_name=''fedengine_test_items'' AND index_name=''fedengine_test_amount_idx''')"
        };
        assert_eq!(
            admin
                .query_row(metadata, [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        client
            .simple_query(&format!(
                "DROP INDEX {alias}.{schema}.fedengine_test_amount_idx"
            ))
            .await
            .unwrap();
        assert_eq!(
            admin
                .query_row(metadata, [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        client
            .simple_query(&format!(
                "CREATE UNIQUE INDEX fedengine_unique_amount_idx ON {table}(amount)"
            ))
            .await
            .unwrap();
        assert!(
            client
                .execute(
                    &format!("INSERT INTO {table}(id,name,amount) VALUES(3,'Eve',30)"),
                    &[]
                )
                .await
                .is_err()
        );
        assert_eq!(
            client
                .query_one(&format!("SELECT count(*)::BIGINT FROM {table}"), &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            2
        );
        if alias == "pg_crm" {
            client
                .simple_query("DROP INDEX pg_crm.public.fedengine_unique_amount_idx")
                .await
                .unwrap();
        } else {
            assert!(
                client
                    .simple_query("DROP INDEX mysql_erp.erp.fedengine_unique_amount_idx")
                    .await
                    .is_err()
            );
        }
        let no_key = format!("{alias}.{schema}.fedengine_no_key");
        client
            .simple_query(&format!("CREATE TABLE {no_key}(value INTEGER)"))
            .await
            .unwrap();
        assert_eq!(
            client
                .execute(
                    &format!("INSERT INTO {no_key} VALUES(NULL),(NULL),(1)"),
                    &[]
                )
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            client
                .execute(
                    &format!("UPDATE {no_key} SET value=7 WHERE value IS NULL"),
                    &[]
                )
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            client
                .execute(&format!("DELETE FROM {no_key} WHERE value=7"), &[])
                .await
                .unwrap(),
            2
        );
        client
            .simple_query(&format!("DROP TABLE {no_key}"))
            .await
            .unwrap();
        assert_eq!(
            client
                .execute(&format!("DELETE FROM {table} WHERE id=2"), &[])
                .await
                .unwrap(),
            1
        );
    }
    let rows=client.query("SELECT a.name,b.amount FROM pg_crm.public.fedengine_test_items a JOIN mysql_erp.erp.fedengine_test_items b ON a.id=b.id",&[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "Alice");
    for sql in [
        "INSERT INTO pg_crm.public.fedengine_test_items(id,name,amount) SELECT id,name,amount FROM mysql_erp.erp.fedengine_test_items",
        "CREATE TABLE pg_crm.public.cross_source_must_not_exist AS SELECT * FROM mysql_erp.erp.fedengine_test_items",
        "CREATE TABLE pg_crm.public.cross_source_must_not_exist(id INTEGER REFERENCES mysql_erp.erp.fedengine_test_items(id))",
    ] {
        assert!(client.simple_query(sql).await.is_err());
    }
    assert!(
        client
            .query(
                "SELECT * FROM pg_crm.public.cross_source_must_not_exist",
                &[]
            )
            .await
            .is_err()
    );
    client.simple_query("CREATE TABLE pg_crm.public.fedengine_copy AS SELECT * FROM pg_crm.public.fedengine_test_items").await.unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT count(*)::BIGINT FROM pg_crm.public.fedengine_copy",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    client
        .simple_query("DROP TABLE pg_crm.public.fedengine_copy")
        .await
        .unwrap();
    client
        .simple_query("CREATE SCHEMA mysql_erp.fedengine_test_ns")
        .await
        .unwrap();
    client
        .simple_query("DROP SCHEMA mysql_erp.fedengine_test_ns")
        .await
        .unwrap();
    client
        .simple_query("CREATE SCHEMA pg_crm.sales")
        .await
        .unwrap();
    client
        .simple_query("DROP SCHEMA pg_crm.sales")
        .await
        .unwrap();
    assert!(
        client
            .simple_query("CREATE SCHEMA mysql_erp.unapproved_namespace")
            .await
            .is_err()
    );
    for table in [
        "pg_crm.public.fedengine_test_items",
        "mysql_erp.erp.fedengine_test_items",
    ] {
        client
            .simple_query(&format!("DROP TABLE {table}"))
            .await
            .unwrap();
    }
    drop(client);
    driver.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), engine.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(engine.handle.health().pending.operations, 0);
    let metrics = engine.metrics();
    assert_eq!(metrics.active_workers, 0);
    assert_eq!(metrics.active_sessions, 0);
    assert_eq!(metrics.metadata_stale, 0);
    assert!(metrics.ddl_confirmed >= 20);
    assert!(metrics.unknown_writes >= 2);
    assert_eq!(metrics.admitted, metrics.completed + metrics.failed);
}
