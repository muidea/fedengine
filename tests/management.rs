//! Public HTTP and persistent single-instance management contracts.
mod common;
#[path = "common/protocol_engine.rs"]
mod protocol_engine;
use common::TestDirectory;
use fedengine_app::FedEngine;
use fedengine_core::*;
use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

fn configuration(temporary: &TestDirectory) -> Config {
    let mut config = Config::load("config/application.toml").unwrap();
    config.pgwire.listen = "127.0.0.1:0".into();
    config.users[0].password_secret_ref = "USER".into();
    config.duckdb.database_path = temporary
        .path()
        .join("management.duckdb")
        .to_string_lossy()
        .into();
    config.duckdb.temp_directory = temporary.path().join("tmp").to_string_lossy().into();
    config.duckdb.threads = 1;
    config.duckdb.max_workers = 2;
    config.management.enabled = true;
    config.management.listen = "127.0.0.1:0".into();
    config.management.credentials_directory = temporary.path().to_string_lossy().into();
    config.management.drain_timeout_ms = 100;
    config
}
async fn http(
    address: SocketAddr,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Value,
) -> (u16, Value) {
    let mut socket = TcpStream::connect(address).await.unwrap();
    let body = body.to_string();
    let key = key
        .map(|key| format!("Idempotency-Key: {key}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} /api/v1/{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\n{key}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body =
        serde_json::from_str(body).unwrap_or_else(|_| panic!("invalid JSON response: {response}"));
    (status, body)
}
async fn virtual_mutation(
    address: SocketAddr,
    method: &str,
    path: &str,
    key: &str,
    body: Value,
) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (status, value) = http(address, method, path, Some(key), body.clone()).await;
            if status == 409 && value["code"] == "55P03" {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            assert_eq!(status, 200, "{value}");
            return value;
        }
    })
    .await
    .expect("virtual publication drain")
}
async fn get(address: SocketAddr, path: &str) -> Value {
    let (status, value) = http(address, "GET", path, None, Value::Null).await;
    assert_eq!(status, 200, "{value}");
    value
}
async fn finish(address: SocketAddr, operation: &Value) -> Value {
    let id = operation["id"].as_str().expect("operation id");
    let mut last = Value::Null;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = get(address, &format!("operations/{id}")).await;
            last = result.clone();
            if ["Succeeded", "Failed", "Interrupted"].contains(&result["status"].as_str().unwrap())
            {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("operation completion deadline for {id}; last state: {last}"))
}
async fn submit(address: SocketAddr, method: &str, path: &str, key: &str, body: Value) -> Value {
    let (status, operation) = http(address, method, path, Some(key), body).await;
    assert_eq!(status, 202, "{operation}");
    eprintln!("management accepted {method} {path}: {}", operation["id"]);
    let completed = finish(address, &operation).await;
    eprintln!(
        "management completed {method} {path}: {}",
        completed["status"]
    );
    assert_eq!(completed["status"], "Succeeded", "{completed}");
    completed
}
fn definition() -> Value {
    json!({"id":"online","kind":"postgres","catalog_alias":"pg_online","secret_ref":"file:dsn","default_schema":"public","allowed_schemas":["public"],"read_only":true})
}
fn target(source: &Value) -> Value {
    json!({"incarnation":source["incarnation"],"expected_revision":source["desired_revision"]})
}

async fn browser_http(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> (u16, String, String) {
    let mut socket = TcpStream::connect(address).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{headers}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head.to_ascii_lowercase(), body.into())
}

#[tokio::test]
async fn embedded_web_assets_and_browser_origin_boundary() {
    let temporary = TestDirectory::new();
    let engine = FedEngine::start(configuration(&temporary)).await.unwrap();
    let address = engine.management_address().unwrap();
    for path in ["/", "/manage", "/manage/"] {
        let (status, headers, body) = browser_http(address, "GET", path, "", "").await;
        assert_eq!(status, 200);
        assert!(headers.contains("content-type: text/html; charset=utf-8"));
        assert!(headers.contains("cache-control: no-store"));
        assert!(headers.contains("frame-ancestors 'none'"));
        assert!(headers.contains("x-content-type-options: nosniff"));
        assert!(body.contains("lang=\"zh-CN\""));
        assert!(body.contains("/manage/app.js"));
    }
    for (path, content_type) in [
        ("/manage/app.js", "text/javascript"),
        ("/manage/style.css", "text/css"),
    ] {
        let (status, headers, body) = browser_http(address, "GET", path, "", "").await;
        assert_eq!(status, 200);
        assert!(headers.contains(content_type));
        assert!(!body.is_empty());
    }
    assert_eq!(
        browser_http(address, "GET", "/manage/secrets/credentials", "", "")
            .await
            .0,
        404
    );
    let body = definition().to_string();
    for origin in ["http://untrusted.example", "http://127.0.0.1:1", "null"] {
        let headers = format!(
            "Origin: {origin}\r\nContent-Type: application/json\r\nIdempotency-Key: browser-register\r\n"
        );
        assert_eq!(
            browser_http(address, "POST", "/api/v1/sources", &headers, &body)
                .await
                .0,
            403
        );
    }
    let cross_site = format!(
        "Origin: http://{address}\r\nSec-Fetch-Site: cross-site\r\nIdempotency-Key: browser-register\r\n"
    );
    assert_eq!(
        browser_http(address, "POST", "/api/v1/sources", &cross_site, &body)
            .await
            .0,
        403
    );
    assert_eq!(get(address, "sources").await, json!([]));
    let same_origin = format!(
        "Origin: http://{address}\r\nSec-Fetch-Site: same-origin\r\nContent-Type: application/json\r\nIdempotency-Key: browser-register\r\n"
    );
    let (status, _, accepted) =
        browser_http(address, "POST", "/api/v1/sources", &same_origin, &body).await;
    assert_eq!(status, 202, "{accepted}");
    let operation = finish(address, &serde_json::from_str(&accepted).unwrap()).await;
    assert_eq!(operation["status"], "Succeeded");
    assert_eq!(get(address, "sources/online").await["status"], "Registered");
    engine.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
async fn registration_versions_idempotency_and_deletion_survive_restart_without_reimport() {
    let temporary = TestDirectory::new();
    let mut config = configuration(&temporary);
    let engine = FedEngine::start(config.clone()).await.unwrap();
    let address = engine.management_address().unwrap();
    assert_eq!(get(address, "sources").await, json!([]));
    let first = submit(address, "POST", "sources", "register", definition()).await;
    let (status, repeated) = http(address, "POST", "sources", Some("register"), definition()).await;
    assert_eq!(status, 202);
    assert_eq!(first["id"], repeated["id"]);
    let mut other = definition();
    other["id"] = json!("different");
    assert_eq!(
        http(address, "POST", "sources", Some("register"), other)
            .await
            .0,
        409
    );
    assert_eq!(
        http(address, "POST", "sources", None, definition()).await.0,
        400
    );
    let source = get(address, "sources/online").await;
    let old_incarnation = source["incarnation"].clone();
    assert_eq!(source["status"], "Registered");
    assert_eq!(source["applied_revision"], Value::Null);
    let old_target = target(&source);
    submit(
        address,
        "PATCH",
        "sources/online",
        "patch",
        json!({"target":old_target,"secret_ref":"file:rotated"}),
    )
    .await;
    assert_eq!(
        http(
            address,
            "PATCH",
            "sources/online",
            Some("stale"),
            json!({"target":old_target,"secret_ref":"file:dsn"})
        )
        .await
        .0,
        409
    );
    let source = get(address, "sources/online").await;
    assert_eq!(source["desired_revision"], 2);
    assert_eq!(http(address,"PUT","sources/online/schemas",Some("bad-default"),json!({"target":target(&source),"bindings":[{"remote_schema":"public","enabled":false}],"default_schema":"public"})).await.0,400);
    submit(address,"PUT","sources/online/schemas","schemas",json!({"target":target(&source),"bindings":[{"remote_schema":"sales","enabled":true}],"default_schema":"sales"})).await;
    let source = get(address, "sources/online").await;
    assert_eq!(
        source["desired"]["definition"]["allowed_schemas"],
        json!(["sales"])
    );
    assert_eq!(
        http(
            address,
            "DELETE",
            "sources/online",
            Some("early-delete"),
            json!({"target":target(&source)})
        )
        .await
        .0,
        409
    );
    submit(
        address,
        "POST",
        "sources/online/disable",
        "disable",
        json!({"target":target(&source)}),
    )
    .await;
    submit(
        address,
        "DELETE",
        "sources/online",
        "delete",
        json!({"target":target(&source)}),
    )
    .await;
    engine.shutdown_until_drained().await.unwrap();
    drop(engine);
    // Non-empty TOML must not resurrect a deleted registration after first import.
    config.datasources = vec![serde_json::from_value(definition()).unwrap()];
    let engine = FedEngine::start(config.clone()).await.unwrap();
    let address = engine.management_address().unwrap();
    assert_eq!(get(address, "sources").await, json!([]));
    submit(address, "POST", "sources", "new-lifetime", definition()).await;
    let source = get(address, "sources/online").await;
    assert_ne!(source["incarnation"], old_incarnation);
    assert_eq!(
        http(
            address,
            "POST",
            "sources/online/disable",
            Some("old-lifetime"),
            json!({"target":old_target})
        )
        .await
        .0,
        409
    );
    engine.shutdown_until_drained().await.unwrap();
    assert_eq!(engine.handle.task_metrics().tasks, 0);
}

#[path = "common/wire.rs"]
mod wire;
#[tokio::test]
#[ignore = "requires isolated PostgreSQL/MySQL; run scripts/test-federation.sh"]
async fn zero_source_online_activation_catalog_refresh_rollback_portal_drain_and_recovery() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("fedengine=info")
        .with_test_writer()
        .try_init();
    let temporary = TestDirectory::new();
    let mut config = configuration(&temporary);
    config.pgwire.allow_cross_database = true; // Explicitly exercise the legacy federation extension.
    config.virtual_databases = vec![VirtualDatabaseConfig {
        name: "fedengine".into(),
        owner: "fedengine".into(),
        connect: vec![],
        allow_connections: true,
        schemas: vec![
            VirtualSchemaConfig {
                name: "crm".into(),
                source: DataSourceId("crm".into()),
                remote_schema: "public".into(),
            },
            VirtualSchemaConfig {
                name: "erp".into(),
                source: DataSourceId("erp".into()),
                remote_schema: "erp".into(),
            },
        ],
    }];
    config.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into(),
    );
    let admin = duckdb::Connection::open_in_memory().unwrap();
    admin
        .execute_batch(&format!(
            "SET extension_directory={}; LOAD postgres; LOAD mysql",
            quote_literal(config.duckdb.extension_directory.as_ref().unwrap())
        ))
        .unwrap();
    for (id, kind, schema, variable) in [
        ("crm", "postgres", "public", "FEDENGINE_POSTGRES_DSN"),
        ("erp", "mysql", "erp", "FEDENGINE_MYSQL_DSN"),
    ] {
        let dsn = std::env::var(variable).unwrap();
        std::fs::write(temporary.path().join(id), &dsn).unwrap();
        admin.execute_batch(&format!("ATTACH {} AS admin_{id} (TYPE {kind}); DROP TABLE IF EXISTS admin_{id}.{schema}.online_items; CREATE TABLE admin_{id}.{schema}.online_items(id INTEGER, name VARCHAR(30)); INSERT INTO admin_{id}.{schema}.online_items VALUES (1,'online'),(2,'test')",quote_literal(&dsn))).unwrap();
        config.grants.push(Grant {
            principal: "fedengine".into(),
            source: DataSourceId(id.into()),
            schema: "*".into(),
            object: "*".into(),
            actions: std::collections::BTreeSet::from(["SELECT".into()]),
        });
    }
    admin
        .execute_batch(&format!(
            "CALL postgres_execute('admin_crm', {}); CALL postgres_execute('admin_crm', {}); CALL mysql_execute('admin_erp', {})",
            quote_literal("CREATE INDEX online_items_id_idx ON public.online_items(id)"),
            quote_literal("ANALYZE public.online_items"),
            quote_literal("ALTER TABLE erp.online_items MODIFY COLUMN name VARCHAR(30) DEFAULT '', MODIFY COLUMN id INTEGER DEFAULT -7")
        ))
        .unwrap();
    admin.execute_batch("CREATE SCHEMA IF NOT EXISTS admin_crm.sales; CREATE TABLE IF NOT EXISTS admin_crm.sales.online_sales(id INTEGER); DROP TABLE IF EXISTS admin_crm.sales.online_items; CREATE TABLE admin_crm.sales.online_items(id INTEGER); INSERT INTO admin_crm.sales.online_items VALUES (99)").unwrap();
    config.ddl.allow_drop_schema = true;
    config.grants[0]
        .actions
        .extend(["ALTER_TABLE".into(), "DROP_SCHEMA".into()]);
    let extension_version: String = admin.query_row("SELECT extension_version FROM duckdb_extensions() WHERE extension_name='postgres_scanner'", [], |r| r.get(0)).unwrap();
    let remote_version: String = admin.query_row("SELECT version FROM postgres_query('admin_crm', 'SELECT current_setting(''server_version'') AS version')", [], |r| r.get(0)).unwrap();
    config.capabilities.push(CapabilityRecord {
        source: DataSourceId("crm".into()),
        duckdb_version: "v2.0.0".into(),
        extension_version,
        remote_version,
        evidence: "tests/management.rs real disposable PostgreSQL".into(),
        operations: std::collections::BTreeSet::from([
            "ALTER_TABLE".into(),
            "ALTER_TABLE_ADD_COLUMN".into(),
            "DROP_SCHEMA".into(),
        ]),
        index_adapter: false,
    });
    config.users.push(UserConfig {
        name: "virtual_reader".into(),
        password_secret_ref: "USER".into(),
    });
    let engine = FedEngine::start(config.clone()).await.unwrap();
    let address = engine.management_address().unwrap();
    let mut pg = tokio_postgres::Config::new();
    pg.host("127.0.0.1")
        .port(engine.address().unwrap().port())
        .user("fedengine")
        .dbname("pg_crm")
        .password(std::env::var("USER").unwrap());
    // No virtual database exists before the first catalog is activated.
    let error = match pg.connect(tokio_postgres::NoTls).await {
        Ok(_) => panic!("unpublished catalog accepted"),
        Err(error) => error,
    };
    assert_eq!(error.code().unwrap().code(), "3D000");
    let notices = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut live = None;
    for (id, kind, schema, alias) in [
        ("crm", "postgres", "public", "pg_crm"),
        ("erp", "mysql", "erp", "mysql_erp"),
    ] {
        submit(address,"POST","sources",&format!("register-{id}"),json!({"id":id,"kind":kind,"catalog_alias":alias,"secret_ref":format!("file:{id}"),"default_schema":schema,"allowed_schemas":[schema],"ddl_enabled":id == "crm"})).await;
        let source = get(address, &format!("sources/{id}")).await;
        let discovery = submit(
            address,
            "POST",
            &format!("sources/{id}/discover"),
            &format!("discover-{id}"),
            json!({"target":target(&source)}),
        )
        .await;
        assert!(
            discovery["result_schemas"]
                .as_array()
                .unwrap()
                .contains(&json!(schema))
        );
        assert_eq!(
            get(address, &format!("sources/{id}")).await["applied_revision"],
            Value::Null
        );
        submit(
            address,
            "POST",
            &format!("sources/{id}/apply"),
            &format!("apply-{id}"),
            json!({"target":target(&source)}),
        )
        .await;
        let catalog = get(address, &format!("sources/{id}/catalog?limit=1&offset=0")).await;
        assert_eq!(catalog["stale"], false);
        assert!(catalog["total"].as_u64().unwrap() >= 1);
        assert_eq!(catalog["schemas"], json!([schema]));
        let detail = get(address, &format!("sources/{id}/catalog?object=online_items&include=core,column_properties,statistics,storage,activity,indexes,column_statistics,status")).await;
        let metadata = &detail["objects"][0]["metadata"];
        assert!(
            metadata["status"]["statistics"]["complete"]
                .as_bool()
                .unwrap(),
            "{metadata}"
        );
        if id == "crm" {
            assert_eq!(metadata["statistics"]["estimated_rows"], 2);
            assert!(metadata["statistics"]["pages"].is_number());
            assert_eq!(metadata["status"]["storage"]["complete"], true);
            assert_eq!(metadata["status"]["activity"]["complete"], true);
            assert!(metadata["storage"]["main_bytes"].as_i64().unwrap() > 0);
            assert!(metadata["storage"]["indexes_bytes"].as_i64().unwrap() > 0);
            assert!(metadata["activity"]["seq_scan"].as_i64().unwrap() >= 0);
            assert_eq!(
                metadata["status"]["column_statistics"]["complete"], true,
                "{metadata}"
            );
            assert_eq!(metadata["column_statistics"]["id"]["avg_width"], 4);
            assert!(
                metadata["column_statistics"]["id"]["null_frac"]
                    .as_f64()
                    .unwrap()
                    == 0.0
            );
            let index = metadata["indexes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["name"] == "online_items_id_idx")
                .unwrap();
            assert!(index["activity"]["idx_scan"].as_i64().unwrap() >= 0);
            let remote:i64 = admin.query_row("SELECT size FROM postgres_query('admin_crm','SELECT pg_relation_size(''public.online_items_id_idx'') AS size')",[],|r|r.get(0)).unwrap();
            assert_eq!(index["storage"]["main_bytes"], remote);
            for (field, expression) in [
                ("main_bytes", "pg_relation_size('public.online_items')"),
                ("fsm_bytes", "pg_relation_size('public.online_items','fsm')"),
                ("vm_bytes", "pg_relation_size('public.online_items','vm')"),
                (
                    "init_bytes",
                    "pg_relation_size('public.online_items','init')",
                ),
                ("table_bytes", "pg_table_size('public.online_items')"),
                ("indexes_bytes", "pg_indexes_size('public.online_items')"),
                (
                    "total_bytes",
                    "pg_total_relation_size('public.online_items')",
                ),
            ] {
                let sql = format!(
                    "SELECT size FROM postgres_query('admin_crm',{})",
                    quote_literal(&format!("SELECT {expression} AS size"))
                );
                let remote: i64 = admin.query_row(&sql, [], |r| r.get(0)).unwrap();
                assert_eq!(metadata["storage"][field], remote, "{field}");
            }
        } else {
            assert_eq!(metadata["columns"]["name"]["default"]["pg_text"], "''");
            assert_eq!(
                metadata["columns"]["name"]["default"]["translation_status"],
                "translated"
            );
            assert_eq!(metadata["columns"]["id"]["default"]["pg_text"], "-7");
            assert!(metadata["statistics"]["pages"].is_null());
            assert!(metadata["storage"]["data_length"].is_number());
            assert!(metadata["storage"]["index_length"].is_number());
            assert!(metadata["storage"]["main_bytes"].is_null());
            assert_eq!(metadata["status"]["storage"]["support"], "partial");
            assert_eq!(metadata["status"]["activity"]["support"], "unsupported");
        }

        fn facts(page: &Value) -> Value {
            let mut objects = page["objects"].clone();
            for object in objects.as_array_mut().unwrap() {
                for status in object["metadata"]["status"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                {
                    status.as_object_mut().unwrap().remove("last_attempt_at");
                    status.as_object_mut().unwrap().remove("last_success_at");
                }
            }
            json!([page["schemas"], page["stale"], objects])
        }
        let mut previous_facts = get(address, &format!("sources/{id}/catalog?limit=1000")).await;
        for round in 0..2 {
            let current = get(address, &format!("sources/{id}")).await;
            submit(
                address,
                "POST",
                &format!("sources/{id}/refresh"),
                &format!("unchanged-{id}-{round}"),
                json!({"target":target(&current)}),
            )
            .await;
            let unchanged = get(
                address,
                &format!("sources/{id}/catalog?limit=1&include=core,status"),
            )
            .await;
            assert!(unchanged["objects"][0]["metadata"].get("storage").is_none());
            assert!(
                unchanged["objects"][0]["metadata"]
                    .get("activity")
                    .is_none()
            );
            // Source counters can become visible asynchronously when a scanner's
            // independent PG connection closes. Reuse depends on facts, not DDL alone.
            let refreshed = get(address, &format!("sources/{id}/catalog?limit=1000")).await;
            assert_eq!(
                previous_facts["snapshot_id"] == refreshed["snapshot_id"],
                facts(&previous_facts) == facts(&refreshed)
            );
            previous_facts = refreshed;
            assert_eq!(
                catalog["objects"][0]["metadata"]["oid"],
                unchanged["objects"][0]["metadata"]["oid"]
            );
            assert!(
                unchanged["objects"][0]["metadata"]
                    .get("constraints")
                    .is_none()
            );
        }
        if live.is_none() {
            let (client, mut connection) = pg.connect(tokio_postgres::NoTls).await.unwrap();
            let captured_notices = notices.clone();
            let driver = tokio::spawn(async move {
                while let Some(message) =
                    std::future::poll_fn(|cx| connection.poll_message(cx)).await
                {
                    match message {
                        Ok(tokio_postgres::AsyncMessage::Notice(n)) => captured_notices
                            .lock()
                            .unwrap()
                            .push(n.message().to_owned()),
                        Err(_) => break,
                        _ => {}
                    }
                }
            });
            live = Some((client, driver));
        }
        // This session predates subsequent source attachments.
        let (client, _) = live.as_ref().unwrap();
        let table = format!("{alias}.{schema}.online_items");
        if id == "crm" {
            let size: i64 = client
                .query_one("SELECT pg_relation_size('public.online_items_id_idx')", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(
                size,
                metadata["indexes"][0]["storage"]["main_bytes"]
                    .as_i64()
                    .unwrap()
            );
            let row = client.query_one("SELECT idx_scan FROM pg_stat_user_indexes WHERE indexrelname='online_items_id_idx'",&[]).await.unwrap();
            assert_eq!(
                row.get::<_, Option<i64>>(0),
                metadata["indexes"][0]["activity"]["idx_scan"].as_i64()
            );
            let row = client.query_one("SELECT avg_width,most_common_vals, histogram_bounds FROM pg_stats WHERE tablename='online_items' AND attname='id'",&[]).await.unwrap();
            assert_eq!(row.get::<_, i32>(0), 4);
            assert!(row.columns()[1].type_().oid() == 2277);
            assert!(row.try_get::<_, Option<String>>(1).is_err()); // descriptor is anyarray, not a forged text field
            assert_eq!(client.query_one("SELECT count(*) FROM pg_stats WHERE tablename='online_items' AND (most_common_vals IS NOT NULL OR histogram_bounds IS NOT NULL)",&[]).await.unwrap().get::<_,i64>(0),0);
            assert_eq!(
                client
                    .query_one(
                        "SELECT count(*) FROM pg_stat_user_tables WHERE relname='online_items'",
                        &[]
                    )
                    .await
                    .unwrap()
                    .get::<_, i64>(0),
                1
            );
        }
        assert_eq!(
            client
                .query_one(&format!("SELECT count(*)::BIGINT FROM {table}"), &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            2
        );
    }
    let mut mysql_login = pg.clone();
    mysql_login.dbname("mysql_erp");
    let (mysql_client, mysql_connection) =
        mysql_login.connect(tokio_postgres::NoTls).await.unwrap();
    let mysql_driver = tokio::spawn(async move {
        mysql_connection.await.unwrap();
    });
    let unknown = mysql_client.query_one("SELECT pg_relation_size('erp.online_items'), pg_table_size('erp.online_items'), pg_total_relation_size('erp.online_items')", &[]).await.unwrap();
    for i in 0..3 {
        assert_eq!(unknown.get::<_, Option<i64>>(i), None);
    }
    drop(mysql_client);
    mysql_driver.await.unwrap();
    // A separately connected logical database sees both dynamically published sources.
    let mut logical = pg.clone();
    logical.dbname("fedengine");
    let (mapped, mapped_connection) = logical.connect(tokio_postgres::NoTls).await.unwrap();
    let mapped_driver = tokio::spawn(async move {
        mapped_connection.await.unwrap();
    });
    assert_eq!(mapped.query_one("SELECT count(*)::BIGINT FROM crm.online_items a JOIN fedengine.erp.online_items b USING(id)",&[]).await.unwrap().get::<_,i64>(0),2);
    let mapped_oid= mapped.query_one("SELECT oid FROM pg_class WHERE relname='online_items' AND relkind='f' ORDER BY oid LIMIT 1",&[]).await.unwrap().get::<_,u32>(0);
    assert!(mapped_oid > 0);
    let unknown = mapped.query_one("SELECT pg_relation_size('crm.online_items'), pg_table_size('crm.online_items'), pg_stat_get_numscans($1)", &[&mapped_oid]).await.unwrap();
    for index in 0..3 {
        assert_eq!(
            unknown.columns()[index].type_(),
            &tokio_postgres::types::Type::INT8
        );
        assert_eq!(unknown.get::<_, Option<i64>>(index), None);
    }
    assert_eq!(mapped.query_one("SELECT count(*)::BIGINT FROM pg_foreign_table t JOIN pg_foreign_server s ON s.oid=t.ftserver",&[]).await.unwrap().get::<_,i64>(0),2);
    assert_eq!(
        mapped
            .query_one("SELECT count(*)::BIGINT FROM pg_constraint", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    drop(mapped);
    mapped_driver.await.unwrap();
    let (client, driver) = live.unwrap();
    for (sql, oid) in [
        ("SELECT pg_relation_size($1)", 2205),
        ("SELECT pg_table_size($1)", 2205),
        ("SELECT pg_total_relation_size($1)", 2205),
        ("SELECT pg_stat_get_numscans($1)", 26),
        ("SELECT pg_relation_size($1::oid)", 26),
    ] {
        let statement = client.prepare(sql).await.unwrap();
        assert_eq!(statement.params()[0].oid(), oid, "{sql}");
        assert_eq!(
            statement.columns()[0].type_(),
            &tokio_postgres::types::Type::INT8
        );
    }
    assert_eq!(
        client
            .query_one("SELECT pg_relation_size($1::oid,NULL)", &[&0u32])
            .await
            .unwrap()
            .get::<_, Option<i64>>(0),
        None
    );
    let published = get(
        address,
        "sources/crm/catalog?schema=public&object=online_items&include=storage,activity",
    )
    .await;
    let metadata = &published["objects"][0]["metadata"];
    let sizes = client.query_one("SELECT pg_relation_size('public.online_items'), pg_table_size('public.online_items'), pg_indexes_size('public.online_items'), pg_total_relation_size('public.online_items'), pg_stat_get_numscans('public.online_items'::regclass)", &[]).await.unwrap();
    for (i, field) in ["main_bytes", "table_bytes", "indexes_bytes", "total_bytes"]
        .iter()
        .enumerate()
    {
        assert_eq!(
            sizes.columns()[i].type_(),
            &tokio_postgres::types::Type::INT8
        );
        assert_eq!(
            sizes.get::<_, i64>(i),
            metadata["storage"][field].as_i64().unwrap()
        );
    }
    assert_eq!(
        sizes.get::<_, Option<i64>>(4),
        metadata["activity"]["seq_scan"].as_i64()
    );
    for sql in [
        "SELECT pg_relation_size('public.missing')",
        "SELECT 'public.missing'::regclass",
    ] {
        assert_eq!(
            client
                .simple_query(sql)
                .await
                .unwrap_err()
                .code()
                .unwrap()
                .code(),
            "42P01"
        );
    }
    let crm = get(address, "sources/crm").await;
    let erp = get(address, "sources/erp").await;
    let (refresh, patch) = tokio::join!(
        http(
            address,
            "POST",
            "sources/crm/refresh",
            Some("parallel-refresh"),
            json!({"target":target(&crm)})
        ),
        http(
            address,
            "PATCH",
            "sources/erp",
            Some("parallel-write"),
            json!({"target":target(&erp),"secret_ref":"file:erp"})
        )
    );
    assert_eq!(refresh.0, 202);
    assert_eq!(patch.0, 202);
    let (refreshed, patched) = tokio::join!(finish(address, &refresh.1), finish(address, &patch.1));
    assert_eq!(refreshed["status"], "Succeeded");
    assert_eq!(patched["status"], "Succeeded");
    assert_eq!(client.query_one("SELECT count(*)::BIGINT FROM pg_crm.public.online_items p JOIN mysql_erp.erp.online_items m ON p.id=m.id",&[]).await.unwrap().get::<_,i64>(0),2);
    assert!(
        client
            .query("SELECT * FROM pg_crm.sales.online_sales", &[])
            .await
            .is_err()
    );
    assert!(
        client
            .query("SELECT * FROM _fedengine.data_sources", &[])
            .await
            .is_err()
    );
    assert!(
        client
            .simple_query("ATTACH ':memory:' AS arbitrary")
            .await
            .is_err()
    );
    let prepared = client
        .prepare("SELECT * FROM pg_crm.public.online_items")
        .await
        .unwrap();
    let old_snapshot = get(address, "sources/crm/catalog").await["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    admin
        .execute_batch("ALTER TABLE admin_crm.public.online_items ADD COLUMN note VARCHAR(30)")
        .unwrap();
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "POST",
        "sources/crm/refresh",
        "external-ddl-refresh",
        json!({"target":target(&source)}),
    )
    .await;
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
    let expired = http(
        address,
        "GET",
        &format!("sources/crm/catalog?snapshot_version={old_snapshot}"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(expired.0, 409);
    assert_eq!(expired.1["code"], "40001");
    let catalog = get(address, "sources/crm/catalog").await;
    let object = catalog["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == "online_items")
        .unwrap();
    assert_eq!(object["columns"].as_array().unwrap().len(), 3);
    submit(address,"PUT","sources/crm/schemas","include-sales",json!({"target":target(&source),"bindings":[{"remote_schema":"public","enabled":true},{"remote_schema":"sales","enabled":true}],"default_schema":"public"})).await;
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "POST",
        "sources/crm/apply",
        "apply-schemas",
        json!({"target":target(&source)}),
    )
    .await;
    client
        .query("SELECT * FROM pg_crm.sales.online_sales", &[])
        .await
        .unwrap();
    // Online replacement must invalidate a plan even when its source generation
    // is unchanged and only the remote schema of the same source is rebound.
    let source_generation = get(address, "sources/crm").await["generation"].clone();
    let definition = json!({"name":"api_mapping","owner":"fedengine","connect":["virtual_reader"],"schemas":[{"name":"mapped","source":"crm","remote_schema":"public"}]});
    let created = virtual_mutation(
        address,
        "POST",
        "virtual-databases",
        "mapping-create",
        definition.clone(),
    )
    .await;
    let mut mapped_pg = pg.clone();
    mapped_pg.dbname("api_mapping");
    let (mapped, mapped_connection) = mapped_pg.connect(tokio_postgres::NoTls).await.unwrap();
    let mapped_task = tokio::spawn(async move {
        mapped_connection.await.unwrap();
    });
    let prepared = mapped
        .prepare("SELECT id FROM mapped.online_items ORDER BY id")
        .await
        .unwrap();
    assert_eq!(
        mapped.query(&prepared, &[]).await.unwrap()[0].get::<_, i32>(0),
        1
    );
    let mut reader_pg = mapped_pg.clone();
    reader_pg.user("virtual_reader");
    let (reader, reader_connection) = reader_pg.connect(tokio_postgres::NoTls).await.unwrap();
    let reader_task = tokio::spawn(async move {
        reader_connection.await.unwrap();
    });
    assert_eq!(
        reader
            .query_one("SELECT count(*) FROM pg_class WHERE relkind='f'", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "CONNECT must not reveal source objects"
    );
    assert!(
        reader
            .query("SELECT id FROM mapped.online_items", &[])
            .await
            .is_err(),
        "CONNECT must not grant business SELECT"
    );
    drop(reader);
    reader_task.await.unwrap();
    let mut replacement = definition.clone();
    replacement["schemas"][0]["remote_schema"] = json!("sales");
    let updated=virtual_mutation(address,"PUT","virtual-databases/api_mapping","mapping-replace",json!({"target":{"oid":created["oid"],"expected_revision":created["revision"]},"definition":replacement})).await;
    assert_eq!(
        get(address, "sources/crm").await["generation"],
        source_generation
    );
    assert_eq!(
        mapped
            .query(&prepared, &[])
            .await
            .unwrap_err()
            .code()
            .unwrap()
            .code(),
        "0A000"
    );
    assert_eq!(
        mapped
            .query_one("SELECT id FROM mapped.online_items", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        99
    );
    drop(mapped);
    mapped_task.await.unwrap();
    virtual_mutation(
        address,
        "DELETE",
        "virtual-databases/api_mapping",
        "mapping-delete",
        json!({"target":{"oid":updated["oid"],"expected_revision":updated["revision"]}}),
    )
    .await;
    admin
        .execute_batch("DROP TABLE admin_crm.sales.online_items")
        .unwrap();
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "POST",
        "sources/crm/refresh",
        "cleanup-mapping-fixture",
        json!({"target":target(&source)}),
    )
    .await;
    let stats = client
        .query_one(
            "SELECT reltuples,relpages,relallvisible FROM pg_class WHERE relname='online_items'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        stats.columns()[0].type_(),
        &tokio_postgres::types::Type::FLOAT4
    );
    assert_eq!(stats.get::<_, f32>(0), 2.0);
    assert!(stats.get::<_, i32>(1) > 0);
    assert!(stats.get::<_, i32>(2) >= 0);
    let before_managed_ddl = client
        .prepare("SELECT * FROM pg_crm.public.online_items")
        .await
        .unwrap();
    client
        .simple_query("ALTER TABLE pg_crm.public.online_items ADD COLUMN managed_note VARCHAR(20)")
        .await
        .unwrap();
    // CommandComplete is a publication barrier: no caller-side retry/poll is needed.
    client
        .query(
            "SELECT managed_note FROM pg_crm.public.online_items LIMIT 1",
            &[],
        )
        .await
        .unwrap();
    let source_after_ddl = get(address, "sources/crm").await;
    assert_eq!(source_after_ddl["status"], "Active");
    assert!(source_after_ddl["current_operation"].is_null());
    let catalog_after_ddl = get(address, "sources/crm/catalog").await;
    assert!(
        catalog_after_ddl["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["name"] == "online_items" && o["columns"].as_array().unwrap().len() == 4)
    );
    assert_eq!(
        client
            .query(&before_managed_ddl, &[])
            .await
            .unwrap_err()
            .code()
            .unwrap()
            .code(),
        "0A000"
    );
    client
        .execute(
            "ALTER TABLE pg_crm.public.online_items ADD COLUMN extended_note INTEGER",
            &[],
        )
        .await
        .unwrap();
    client
        .simple_query("SELECT extended_note FROM pg_crm.public.online_items LIMIT 1")
        .await
        .unwrap();
    // Confirmed DDL remains successful even when its selected namespace disappears.
    admin
        .execute_batch("DROP TABLE admin_crm.sales.online_sales")
        .unwrap();
    client
        .execute("DROP SCHEMA pg_crm.sales", &[])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let source = get(address, "sources/crm").await;
            if source["status"] == "MetadataStale"
                && source["last_error"].is_string()
                && source["current_operation"].is_null()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failed DDL refresh must be observable");
    assert!(
        notices
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.starts_with("DDL committed; metadata refresh failed or is still pending."))
    );
    assert_eq!(get(address, "sources/crm/catalog").await["stale"], true);
    assert!(
        client
            .query("SELECT * FROM pg_crm.public.online_items", &[])
            .await
            .is_err()
    );
    // An unsuccessful apply cannot reopen an already stale previous version.
    std::fs::write(temporary.path().join("stale-bad"),"host=127.0.0.1 port=1 dbname=crm user=invalid password=never-expose-this connect_timeout=1").unwrap();
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "PATCH",
        "sources/crm",
        "patch-while-stale",
        json!({"target":target(&source),"secret_ref":"file:stale-bad"}),
    )
    .await;
    let source = get(address, "sources/crm").await;
    let (_, operation) = http(
        address,
        "POST",
        "sources/crm/apply",
        Some("failed-apply-while-stale"),
        json!({"target":target(&source)}),
    )
    .await;
    assert_eq!(finish(address, &operation).await["status"], "Failed");
    assert_eq!(get(address, "sources/crm").await["status"], "MetadataStale");
    assert!(
        client
            .query("SELECT * FROM pg_crm.public.online_items", &[])
            .await
            .is_err()
    );
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "PATCH",
        "sources/crm",
        "restore-stale-reference",
        json!({"target":target(&source),"secret_ref":"file:crm"}),
    )
    .await;
    admin
        .execute_batch(
            "CALL postgres_clear_cache(); CREATE SCHEMA admin_crm.sales; CREATE TABLE admin_crm.sales.online_sales(id INTEGER)",
        )
        .unwrap();
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "POST",
        "sources/crm/refresh",
        "recover-ddl-stale",
        json!({"target":target(&source)}),
    )
    .await;
    assert_eq!(get(address, "sources/crm").await["status"], "Active");
    // A failed new connection leaves the previous applied version usable.
    std::fs::write(temporary.path().join("bad"),"host=127.0.0.1 port=1 dbname=crm user=invalid password=never-expose-this connect_timeout=1").unwrap();
    let source = get(address, "sources/crm").await;
    let applied_revision = source["applied_revision"].clone();
    submit(
        address,
        "PATCH",
        "sources/crm",
        "bad-connection",
        json!({"target":target(&source),"secret_ref":"file:bad"}),
    )
    .await;
    let source = get(address, "sources/crm").await;
    let (status, operation) = http(
        address,
        "POST",
        "sources/crm/apply",
        Some("apply-bad"),
        json!({"target":target(&source)}),
    )
    .await;
    assert_eq!(status, 202);
    let failed = finish(address, &operation).await;
    assert_eq!(failed["status"], "Failed");
    assert!(!failed.to_string().contains("never-expose-this"));
    let source = get(address, "sources/crm").await;
    assert_eq!(source["status"], "Active");
    assert_eq!(source["applied_revision"], applied_revision);
    client
        .query("SELECT * FROM pg_crm.public.online_items", &[])
        .await
        .unwrap();
    submit(
        address,
        "PATCH",
        "sources/crm",
        "restore-reference",
        json!({"target":target(&source),"secret_ref":"file:crm"}),
    )
    .await;
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "POST",
        "sources/crm/apply",
        "apply-restored",
        json!({"target":target(&source)}),
    )
    .await;
    let (mapped, mapped_connection) = logical.connect(tokio_postgres::NoTls).await.unwrap();
    let mapped_driver = tokio::spawn(async move {
        mapped_connection.await.unwrap();
    });
    let before_resume_catalog = get(address, "sources/crm/catalog?limit=1000").await;
    let before_resume_mapped = mapped
        .query(
            "SELECT oid FROM pg_class WHERE relkind='f' ORDER BY oid",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, u32>(0))
        .collect::<Vec<_>>();
    let before_resume_revision = get(address, "sources/crm").await["desired_revision"].clone();
    // A suspended portal holds both execution and source leases until Close/Sync.
    let mut portal = wire::raw_start(engine.address().unwrap(), "pg_crm").await;
    wire::packet(
        &mut portal,
        b'P',
        b"stmt\0SELECT p.id FROM pg_crm.public.online_items p, range(1000000) r(i)\0\0\0",
    )
    .await;
    wire::packet(&mut portal, b'B', b"p\0stmt\0\0\0\0\0\0\0").await;
    let mut execute = b"p\0".to_vec();
    execute.extend_from_slice(&1i32.to_be_bytes());
    wire::packet(&mut portal, b'E', &execute).await;
    wire::packet(&mut portal, b'H', &[]).await;
    assert_eq!(wire::until_boundary(&mut portal).await.1, b's');
    let source = get(address, "sources/crm").await;
    let (_, operation) = http(
        address,
        "POST",
        "sources/crm/disable",
        Some("portal-disable"),
        json!({"target":target(&source)}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let waiting = get(
        address,
        &format!("operations/{}", operation["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(waiting["status"], "WaitingForDrain");
    // The source-backed database intentionally closes admission during disable.
    // An existing logical database remains open for queries that do not use that source.
    tokio::time::timeout(Duration::from_secs(1), mapped.simple_query("SELECT 1"))
        .await
        .expect("a suspended portal and pending disable must not block unrelated queries")
        .unwrap();
    assert_eq!(
        http(
            address,
            "POST",
            "sources/crm/apply",
            Some("concurrent-apply"),
            json!({"target":target(&source)})
        )
        .await
        .0,
        409
    );
    wire::packet(&mut portal, b'C', b"Pp\0").await;
    wire::packet(&mut portal, b'S', &[]).await;
    loop {
        if wire::message(&mut portal).await.0 == b'Z' {
            break;
        }
    }
    assert_eq!(finish(address, &operation).await["status"], "Succeeded");
    drop(portal);
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "POST",
        "sources/crm/apply",
        "reenable",
        json!({"target":target(&source)}),
    )
    .await;
    let after_resume_catalog = get(address, "sources/crm/catalog?limit=1000").await;
    let object_ids = |catalog: &Value| {
        catalog["objects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                (
                    o["schema"].clone(),
                    o["name"].clone(),
                    o["metadata"]["oid"].clone(),
                    o["metadata"]["columns"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        object_ids(&before_resume_catalog),
        object_ids(&after_resume_catalog)
    );
    assert_eq!(
        before_resume_revision,
        get(address, "sources/crm").await["desired_revision"]
    );
    assert_eq!(
        before_resume_mapped,
        mapped
            .query(
                "SELECT oid FROM pg_class WHERE relkind='f' ORDER BY oid",
                &[]
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, u32>(0))
            .collect::<Vec<_>>()
    );
    drop(mapped);
    mapped_driver.await.unwrap();
    // Removing a binding changes the effective scope; clearing the default never falls back locally.
    let source = get(address, "sources/crm").await;
    submit(address,"PUT","sources/crm/schemas","remove-public",json!({"target":target(&source),"bindings":[{"remote_schema":"sales","enabled":true}],"default_schema":null})).await;
    let source = get(address, "sources/crm").await;
    submit(
        address,
        "POST",
        "sources/crm/apply",
        "apply-remove-public",
        json!({"target":target(&source)}),
    )
    .await;
    assert!(
        client
            .query("SELECT * FROM pg_crm.public.online_items", &[])
            .await
            .is_err()
    );
    assert!(
        client
            .query("SELECT * FROM online_sales", &[])
            .await
            .is_err()
    );
    client
        .query("SELECT * FROM pg_crm.sales.online_sales", &[])
        .await
        .unwrap();
    let old_lifetime = client
        .prepare("SELECT * FROM pg_crm.sales.online_sales")
        .await
        .unwrap();
    let source = get(address, "sources/crm").await;
    let prior_incarnation = source["incarnation"].clone();
    submit(
        address,
        "POST",
        "sources/crm/disable",
        "disable-before-recreate",
        json!({"target":target(&source)}),
    )
    .await;
    submit(
        address,
        "DELETE",
        "sources/crm",
        "delete-before-recreate",
        json!({"target":target(&source)}),
    )
    .await;
    submit(address,"POST","sources","recreate-crm",json!({"id":"crm","kind":"postgres","catalog_alias":"pg_crm","secret_ref":"file:crm","default_schema":"sales","allowed_schemas":["sales"],"ddl_enabled":true})).await;
    let source = get(address, "sources/crm").await;
    assert_ne!(source["incarnation"], prior_incarnation);
    let recovery_template = submit(
        address,
        "POST",
        "sources/crm/apply",
        "apply-recreated-crm",
        json!({"target":target(&source)}),
    )
    .await;
    assert_eq!(
        client
            .query(&old_lifetime, &[])
            .await
            .unwrap_err()
            .code()
            .unwrap()
            .code(),
        "3D000"
    );
    // A source-backed database is also pinned to its admitted OID. A fresh
    // connection can enter the new incarnation; the old session cannot migrate.
    assert_eq!(
        client
            .simple_query("SELECT 1")
            .await
            .unwrap_err()
            .code()
            .unwrap()
            .code(),
        "3D000"
    );
    let (fresh, connection) = pg.connect(tokio_postgres::NoTls).await.unwrap();
    let fresh_task = tokio::spawn(async move {
        connection.await.unwrap();
    });
    fresh
        .query("SELECT * FROM online_sales", &[])
        .await
        .unwrap();
    drop(fresh);
    fresh_task.await.unwrap();
    drop(client);
    driver.await.unwrap();
    engine.shutdown_until_drained().await.unwrap();
    drop(engine);
    let engine = FedEngine::start(config.clone()).await.unwrap();
    let address = engine.management_address().unwrap();
    assert_eq!(get(address, "sources/crm").await["status"], "Active");
    assert_eq!(get(address, "sources/erp").await["status"], "Active");
    assert_eq!(
        get(address, "sources/crm/catalog").await["schemas"],
        json!(["sales"])
    );
    engine.shutdown_until_drained().await.unwrap();
    drop(engine);
    for phase in [
        "IntentCommitted",
        "Attaching",
        "SnapshotCommitted",
        "Publishing",
    ] {
        let operation_id =
            inject_interrupted_operation(&config.duckdb.database_path, &recovery_template, phase);
        let engine = FedEngine::start(config.clone()).await.unwrap();
        let address = engine.management_address().unwrap();
        assert_eq!(
            get(address, &format!("operations/{operation_id}")).await["status"],
            "Interrupted"
        );
        assert_eq!(get(address, "sources/crm").await["status"], "Active");
        assert_eq!(
            get(address, "sources/crm").await["current_operation"],
            Value::Null
        );
        engine.shutdown_until_drained().await.unwrap();
    }
    // One unavailable source must not prevent management or healthy-source recovery.
    let mysql_dsn = std::fs::read(temporary.path().join("erp")).unwrap();
    std::fs::write(
        temporary.path().join("erp"),
        "host=127.0.0.1 port=1 database=erp user=invalid password=never-expose-this",
    )
    .unwrap();
    let engine = FedEngine::start(config.clone()).await.unwrap();
    let address = engine.management_address().unwrap();
    assert_eq!(get(address, "sources/crm").await["status"], "Active");
    assert_eq!(get(address, "sources/erp").await["status"], "Degraded");
    std::fs::write(temporary.path().join("erp"), mysql_dsn).unwrap();
    let source = get(address, "sources/erp").await;
    submit(
        address,
        "POST",
        "sources/erp/apply",
        "recover-offline-mysql",
        json!({"target":target(&source)}),
    )
    .await;
    assert_eq!(get(address, "sources/erp").await["status"], "Active");
    engine.shutdown_until_drained().await.unwrap();
    admin.execute_batch("DROP TABLE admin_crm.public.online_items; DROP TABLE admin_crm.sales.online_sales; DROP TABLE admin_erp.erp.online_items").unwrap();
}

/// Reproduce durable crash boundaries after closing the process-owned instance.
/// Runtime attachments are intentionally absent at the next startup.
fn inject_interrupted_operation(path: &str, template: &Value, phase: &str) -> String {
    let connection = duckdb::Connection::open(path).unwrap();
    let incarnation = template["incarnation"].as_str().unwrap();
    let payload: String = connection
        .query_row(
            "SELECT payload FROM _fedengine.data_sources WHERE key=?",
            [incarnation],
            |r| r.get(0),
        )
        .unwrap();
    let mut source: Value = serde_json::from_str(&payload).unwrap();
    let mut operation = template.clone();
    let id = format!("interrupted-{phase}");
    operation["id"] = json!(id);
    operation["idempotency_key"] = json!(id);
    operation["status"] = json!("Running");
    operation["phase"] = json!(phase);
    source["current_operation"] = json!(id);
    source["status"] = json!("Activating");
    connection.execute_batch("BEGIN TRANSACTION").unwrap();
    connection
        .execute(
            "UPDATE _fedengine.data_sources SET payload=? WHERE key=?",
            duckdb::params![source.to_string(), incarnation],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO _fedengine.management_operations VALUES (?,?)",
            duckdb::params![id, operation.to_string()],
        )
        .unwrap();
    connection.execute_batch("COMMIT").unwrap();
    id
}

#[tokio::test]
async fn management_quota_queue_backpressure_and_shutdown_preserve_unfinished_intent() {
    let temporary = TestDirectory::new();
    let mut config = configuration(&temporary);
    config.duckdb.max_workers = 1;
    config.management.queue_capacity = 1;
    config.management.max_concurrent_operations = 1;
    let engine = protocol_engine::ProtocolEngine::start(config.clone())
        .await
        .unwrap();
    let address = engine.management_address().unwrap();
    submit(
        address,
        "POST",
        "sources",
        "register-before-shutdown",
        definition(),
    )
    .await;
    let mut portal = wire::raw_start(engine.address().unwrap(), "protocol_db").await;
    wire::packet(
        &mut portal,
        b'P',
        b"stmt\0SELECT i::INTEGER FROM range(1000000) t(i)\0\0\0",
    )
    .await;
    wire::packet(&mut portal, b'B', b"p\0stmt\0\0\0\0\0\0\0").await;
    let mut execute = b"p\0".to_vec();
    execute.extend_from_slice(&1i32.to_be_bytes());
    wire::packet(&mut portal, b'E', &execute).await;
    wire::packet(&mut portal, b'H', &[]).await;
    assert_eq!(wire::until_boundary(&mut portal).await.1, b's');
    let source = get(address, "sources/online").await;
    let (status, operation) = http(
        address,
        "POST",
        "sources/online/disable",
        Some("disable-before-shutdown"),
        json!({"target":target(&source)}),
    )
    .await;
    assert_eq!(status, 202);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = get(
                address,
                &format!("operations/{}", operation["id"].as_str().unwrap()),
            )
            .await;
            if status["status"] == "WaitingForDrain" {
                break;
            }
            assert!(
                !["Succeeded", "Failed", "Interrupted"]
                    .contains(&status["status"].as_str().unwrap())
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("operation must reach the drain barrier while the portal is held");
    let mut extra = definition();
    extra["id"] = json!("extra");
    extra["catalog_alias"] = json!("pg_extra");
    assert_eq!(
        http(address, "POST", "sources", Some("queue-full"), extra)
            .await
            .0,
        503
    );
    tokio::time::timeout(Duration::from_secs(10), engine.shutdown_until_drained())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(engine.handle.task_metrics().tasks, 0);
    assert_eq!(engine.metrics().active_workers, 0);
    drop(portal);
    drop(engine);
    let engine = protocol_engine::ProtocolEngine::start(config)
        .await
        .unwrap();
    let address = engine.management_address().unwrap();
    assert_eq!(get(address, "sources/online").await["status"], "Disabled");
    let operation = get(
        address,
        &format!("operations/{}", operation["id"].as_str().unwrap()),
    )
    .await;
    assert!(["Succeeded", "Interrupted"].contains(&operation["status"].as_str().unwrap()));
    engine.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the disposable PostgreSQL/MySQL fixtures in scripts/test-federation.sh"]
async fn mysql_strict_identity_rotates_on_resume_and_startup() {
    let temporary = TestDirectory::new();
    let mut config = configuration(&temporary);
    config.management.mysql_identity_policy = MysqlIdentityPolicy::Strict;
    config.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into(),
    );
    config.grants.push(Grant {
        principal: "fedengine".into(),
        source: DataSourceId("strict_mysql".into()),
        schema: "erp".into(),
        object: "*".into(),
        actions: ["SELECT".into()].into_iter().collect(),
    });
    let dsn = std::env::var("FEDENGINE_MYSQL_DSN").unwrap();
    std::fs::write(temporary.path().join("strict_mysql"), &dsn).unwrap();
    let admin = duckdb::Connection::open_in_memory().unwrap();
    admin.execute_batch(&format!("SET extension_directory={}; LOAD mysql; ATTACH {} AS admin_strict (TYPE mysql); DROP TABLE IF EXISTS admin_strict.erp.strict_identity_probe; CREATE TABLE admin_strict.erp.strict_identity_probe(id INTEGER)", quote_literal(config.duckdb.extension_directory.as_ref().unwrap()), quote_literal(&dsn))).unwrap();
    let engine = FedEngine::start(config.clone()).await.unwrap();
    let address = engine.management_address().unwrap();
    submit(address,"POST","sources","strict-register",json!({"id":"strict_mysql","kind":"mysql","catalog_alias":"mysql_strict","secret_ref":"file:strict_mysql","default_schema":"erp","allowed_schemas":["erp"],"read_only":true,"ddl_enabled":false})).await;
    let source = get(address, "sources/strict_mysql").await;
    submit(
        address,
        "POST",
        "sources/strict_mysql/apply",
        "strict-apply",
        json!({"target":target(&source)}),
    )
    .await;
    let oid = |catalog: Value| {
        catalog["objects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["name"] == "strict_identity_probe")
            .unwrap()["metadata"]["oid"]
            .as_u64()
            .unwrap()
    };
    let before = oid(get(address, "sources/strict_mysql/catalog").await);
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(engine.address().unwrap().port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .dbname("mysql_strict")
        .connect(tokio_postgres::NoTls)
        .await
        .unwrap();
    let task = tokio::spawn(async move { connection.await.unwrap() });
    let prepared = client
        .prepare("SELECT id FROM erp.strict_identity_probe")
        .await
        .unwrap();
    let source = get(address, "sources/strict_mysql").await;
    submit(
        address,
        "POST",
        "sources/strict_mysql/disable",
        "strict-disable",
        json!({"target":target(&source)}),
    )
    .await;
    admin.execute_batch("DROP TABLE admin_strict.erp.strict_identity_probe; CREATE TABLE admin_strict.erp.strict_identity_probe(id INTEGER)").unwrap();
    let source = get(address, "sources/strict_mysql").await;
    submit(
        address,
        "POST",
        "sources/strict_mysql/apply",
        "strict-resume",
        json!({"target":target(&source)}),
    )
    .await;
    let resumed = oid(get(address, "sources/strict_mysql/catalog").await);
    assert_ne!(before, resumed);
    let error = client.query(&prepared, &[]).await.unwrap_err();
    assert_eq!(error.code().unwrap().code(), "0A000");
    drop(client);
    task.await.unwrap();
    engine.shutdown_until_drained().await.unwrap();
    let engine = FedEngine::start(config).await.unwrap();
    let restarted = oid(get(
        engine.management_address().unwrap(),
        "sources/strict_mysql/catalog",
    )
    .await);
    assert_ne!(resumed, restarted);
    engine.shutdown_until_drained().await.unwrap();
    admin
        .execute_batch("DROP TABLE admin_strict.erp.strict_identity_probe")
        .unwrap();
}

/// Full remote collection/publication regression. Run only against the disposable
/// PostgreSQL fixture; object/column counts and repetitions are explicit inputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "remote scale workload; see docs/development.md, run separately from functional regression"]
async fn metadata_scale_refresh_preserves_query_admission_and_memory_budget() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Instant;
    let objects: usize = std::env::var("FEDENGINE_SCALE_OBJECTS")
        .unwrap_or_else(|_| "10000".into())
        .parse()
        .unwrap();
    let columns: usize = std::env::var("FEDENGINE_SCALE_COLUMNS")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let rounds: usize = std::env::var("FEDENGINE_SCALE_ROUNDS")
        .unwrap_or_else(|_| "10".into())
        .parse()
        .unwrap();
    assert!(
        (1..=10000).contains(&objects) && columns > 0 && objects * columns <= 100000 && rounds >= 3
    );
    let rss_limit: u64 = std::env::var("FEDENGINE_SCALE_RSS_MB")
        .unwrap_or_else(|_| "1024".into())
        .parse::<u64>()
        .unwrap()
        * 1024
        * 1024;
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,fedengine_duckdb::catalog=debug,fedengine_duckdb::identity=debug,fedengine_duckdb::projection=debug,fedengine_duckdb::storage=debug,fedengine_duckdb::runtime=debug,fedengine_management::coordinator=debug,fedengine_query=debug")
        .with_test_writer()
        .try_init();
    let (admin, connection) = tokio_postgres::connect(
        &std::env::var("FEDENGINE_POSTGRES_DSN").unwrap(),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    let admin_driver = tokio::spawn(async move {
        connection.await.unwrap();
    });
    let schema = format!("scale_{}", uuid::Uuid::new_v4().simple());
    let fields = (0..columns)
        .map(|i| format!("c{i} integer"))
        .collect::<Vec<_>>()
        .join(",");
    admin.batch_execute(&format!("CREATE SCHEMA {schema}; DO $body$ BEGIN FOR i IN 0..{} LOOP EXECUTE format('CREATE TABLE {schema}.t_%s ({fields})',i); END LOOP; END $body$; INSERT INTO {schema}.t_0(c0) VALUES (7)",objects-1)).await.unwrap();
    let temporary = TestDirectory::new();
    let mut config = configuration(&temporary);
    config.duckdb.memory_limit = "256MB".into();
    if columns > 1 {
        config.management.metadata_operation_memory_mb = 512;
        config.management.metadata_memory_budget_mb = 512;
    }
    config.duckdb.extension_directory = Some(
        std::fs::canonicalize("data/extensions")
            .unwrap()
            .to_string_lossy()
            .into(),
    );
    config.virtual_databases = vec![VirtualDatabaseConfig {
        name: "bench".into(),
        owner: "fedengine".into(),
        connect: vec![],
        allow_connections: true,
        schemas: vec![VirtualSchemaConfig {
            name: "business".into(),
            source: DataSourceId("scale".into()),
            remote_schema: schema.clone(),
        }],
    }];
    config.grants = vec![Grant {
        principal: "fedengine".into(),
        source: DataSourceId("scale".into()),
        schema: "*".into(),
        object: "*".into(),
        actions: ["SELECT".into()].into(),
    }];
    std::fs::write(
        temporary.path().join("scale.dsn"),
        std::env::var("FEDENGINE_POSTGRES_DSN").unwrap(),
    )
    .unwrap();
    let engine = FedEngine::start(config).await.unwrap();
    let address = engine.management_address().unwrap();
    let mut login = tokio_postgres::Config::new();
    login
        .host("127.0.0.1")
        .port(engine.address().unwrap().port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .dbname("bench");
    let (client, connection) = login.connect(tokio_postgres::NoTls).await.unwrap();
    let driver = tokio::spawn(async move {
        connection.await.unwrap();
    });
    let mut baseline = Vec::new();
    for _ in 0..50 {
        let start = Instant::now();
        client.simple_query("SELECT 1").await.unwrap();
        baseline.push(start.elapsed().as_secs_f64());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    baseline.sort_by(f64::total_cmp);
    let baseline_p99 = baseline[baseline.len() - 1];
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Vec::<(f64, Option<String>)>::new()));
    let task_stop = stop.clone();
    let task_samples = samples.clone();
    let query = tokio::spawn(async move {
        while !task_stop.load(Ordering::Relaxed) {
            let start = Instant::now();
            let result = client.simple_query("SELECT 1").await;
            let elapsed = start.elapsed().as_secs_f64();
            if elapsed > 0.2 {
                eprintln!(
                    "SLOW unrelated at={} elapsed_ms={:.3}",
                    chrono::Utc::now(),
                    elapsed * 1000.
                );
            }
            task_samples
                .lock()
                .unwrap()
                .push((elapsed, result.err().map(|e| e.to_string())));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(client);
    });
    fn rss() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmRSS:")
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse::<u64>().ok())
            })
            .unwrap()
            * 1024
    }
    let peak = Arc::new(std::sync::atomic::AtomicU64::new(rss()));
    let sampler_peak = peak.clone();
    let sampler_stop = stop.clone();
    let sampler = tokio::spawn(async move {
        while !sampler_stop.load(Ordering::Relaxed) {
            sampler_peak.fetch_max(rss(), Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    submit(address,"POST","sources","scale-register",json!({"id":"scale","kind":"postgres","catalog_alias":"scale","secret_ref":"file:scale.dsn","default_schema":schema,"allowed_schemas":[schema],"read_only":true})).await;
    let mut source_task = None;
    let mut source_driver = None;
    let mut end_rss = Vec::new();
    let mut oid = None;
    for round in 0..=rounds {
        let source = get(address, "sources/scale").await;
        let start = Instant::now();
        let path = if round == 0 {
            "sources/scale/apply"
        } else {
            "sources/scale/refresh"
        };
        let (status, operation) = http(
            address,
            "POST",
            path,
            Some(&format!("scale-{round}")),
            json!({"target":target(&source)}),
        )
        .await;
        assert_eq!(status, 202, "{operation}");
        let result = tokio::time::timeout(Duration::from_secs(600), async {
            loop {
                let op = get(
                    address,
                    &format!("operations/{}", operation["id"].as_str().unwrap()),
                )
                .await;
                if ["Succeeded", "Failed", "Interrupted"].contains(&op["status"].as_str().unwrap())
                {
                    break op;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(result["status"], "Succeeded", "{result}");
        if round == 0 {
            let (reader, connection) = login.connect(tokio_postgres::NoTls).await.unwrap();
            source_driver = Some(tokio::spawn(async move {
                connection.await.unwrap();
            }));
            let stop = stop.clone();
            let samples = samples.clone();
            source_task = Some(tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let start = Instant::now();
                    let result = reader.query_one("SELECT c0 FROM business.t_0", &[]).await;
                    let error = match result {
                        Ok(row) => {
                            assert_eq!(row.get::<_, i32>(0), 7);
                            None
                        }
                        Err(error) => Some(error.to_string()),
                    };
                    let elapsed = start.elapsed().as_secs_f64();
                    if elapsed > 0.2 {
                        eprintln!(
                            "SLOW business at={} elapsed_ms={:.3}",
                            chrono::Utc::now(),
                            elapsed * 1000.
                        );
                    }
                    samples.lock().unwrap().push((elapsed, error));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                drop(reader);
            }));
        }
        let page = get(
            address,
            "sources/scale/catalog?object=t_0&include=core,column_properties",
        )
        .await;
        assert_eq!(
            page["objects"][0]["columns"].as_array().unwrap().len(),
            columns
        );
        assert_eq!(
            page["objects"][0]["metadata"]["columns"]
                .as_object()
                .unwrap()
                .len(),
            columns
        );
        let current = page["objects"][0]["metadata"]["oid"].as_u64().unwrap();
        if let Some(oid) = oid {
            assert_eq!(current, oid);
        } else {
            oid = Some(current);
        }
        let all = get(address, "sources/scale/catalog?limit=1").await;
        assert_eq!(all["total"], objects);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let memory = rss();
        end_rss.push(memory);
        eprintln!(
            "SCALE round={round} objects={objects} columns={} elapsed_s={:.3} rss_mb={:.1} peak_rss_mb={:.1}",
            objects * columns,
            start.elapsed().as_secs_f64(),
            memory as f64 / 1048576.,
            peak.load(Ordering::Relaxed) as f64 / 1048576.
        );
    }
    stop.store(true, Ordering::Relaxed);
    query.await.unwrap();
    if let Some(task) = source_task {
        task.await.unwrap();
    }
    if let Some(task) = source_driver {
        task.await.unwrap();
    }
    sampler.await.unwrap();
    driver.await.unwrap();
    let (catalog_reader, connection) = login.connect(tokio_postgres::NoTls).await.unwrap();
    let catalog_driver = tokio::spawn(async move {
        connection.await.unwrap();
    });
    let projected_columns: i64 = catalog_reader.query_one(
        "SELECT count(*)::BIGINT FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON a.attrelid=c.oid WHERE c.relkind='f' AND a.attnum>0 AND NOT a.attisdropped",
        &[],
    ).await.unwrap().get(0);
    assert_eq!(projected_columns, (objects * columns) as i64);
    drop(catalog_reader);
    catalog_driver.await.unwrap();
    let samples = samples.lock().unwrap().clone();
    let mut times = samples.iter().map(|s| s.0).collect::<Vec<_>>();
    times.sort_by(f64::total_cmp);
    let p95 = times[(times.len() - 1) * 95 / 100];
    let p99 = times[(times.len() - 1) * 99 / 100];
    let max = *times.last().unwrap();
    let errors = samples.iter().filter(|s| s.1.is_some()).collect::<Vec<_>>();
    eprintln!(
        "SCALE queries={} baseline_p99_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3} errors={} peak_rss_mb={:.1}",
        times.len(),
        baseline_p99 * 1000.,
        p95 * 1000.,
        p99 * 1000.,
        max * 1000.,
        errors.len(),
        peak.load(Ordering::Relaxed) as f64 / 1048576.
    );
    engine.shutdown_until_drained().await.unwrap();
    assert_eq!(engine.metrics().active_workers, 0);
    assert_eq!(engine.handle.task_metrics().tasks, 0);
    drop(engine);
    admin
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    drop(admin);
    admin_driver.await.unwrap();
    assert!(errors.is_empty(), "query failures: {errors:?}");
    assert!(max <= 1., "query pause exceeded 1 second: {max}");
    assert!(
        p99 <= baseline_p99 + 0.1,
        "p99 regression: baseline={baseline_p99}, actual={p99}"
    );
    assert!(
        peak.load(Ordering::Relaxed) <= rss_limit,
        "RSS exceeded explicit process budget"
    );
    let tail = &end_rss[end_rss.len() - 3..];
    assert!(
        tail.iter().max().unwrap() - tail.iter().min().unwrap() <= 64 * 1024 * 1024,
        "RSS did not stabilize: {end_rss:?}"
    );
}
