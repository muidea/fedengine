//! Real-engine contracts for the conservative, single-instance control boundary.
use duckdb::Connection;
use fedengine_core::{quote_identifier, quote_literal};

#[test]
#[ignore = "requires disposable PostgreSQL/MySQL and pinned extensions"]
fn attachments_are_visible_to_old_and_new_connections_and_cache_can_be_refreshed() {
    let options = duckdb::Config::default()
        .with("autoload_known_extensions", "false")
        .unwrap()
        .with("autoinstall_known_extensions", "false")
        .unwrap()
        .with("extension_directory", "./data/extensions")
        .unwrap();
    let root = Connection::open_in_memory_with_flags(options).unwrap();
    let before = root.try_clone().unwrap();
    for (extension, variable, schema) in [
        ("postgres", "FEDENGINE_POSTGRES_DSN", "public"),
        ("mysql", "FEDENGINE_MYSQL_DSN", "erp"),
    ] {
        let dsn = std::env::var(variable).expect("test connection reference");
        let alias = format!("control_{extension}");
        root.execute_batch(&format!(
            "LOAD {extension}; ATTACH {} AS {} (TYPE {extension})",
            quote_literal(&dsn),
            quote_identifier(&alias)
        ))
        .expect("load and attach after zero-source initialization");
        let after = root.try_clone().unwrap();
        let table = format!("{alias}.{schema}.fedengine_control_test");
        root.execute_batch(&format!(
            "CREATE TABLE {table}(id INTEGER); INSERT INTO {table} VALUES (1),(2)"
        ))
        .unwrap();
        for connection in [&before, &after] {
            assert_eq!(
                connection
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap(),
                2
            );
        }
        // The stream and its connection are released before the instance changes.
        // Production keeps an execution read lease for this entire lifetime.
        {
            let mut statement = before.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let mut stream = statement.stream_arrow([]).unwrap();
            assert!(stream.next().unwrap().is_ok());
        }
        root.execute_batch(&format!("CALL {extension}_clear_cache()"))
            .unwrap();
        before
            .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN note VARCHAR(30)"))
            .unwrap();
        root.execute_batch(&format!("CALL {extension}_clear_cache()"))
            .unwrap();
        assert_eq!(
            after
                .prepare(&format!("SELECT * FROM {table}"))
                .unwrap()
                .column_count(),
            2
        );
        root.execute_batch(&format!("DROP TABLE {table}; DETACH {alias}"))
            .unwrap();
        assert!(before.prepare(&format!("SELECT * FROM {table}")).is_err());
        assert!(after.prepare(&format!("SELECT * FROM {table}")).is_err());
    }
}
