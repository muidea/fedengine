//! Online virtual database publication through real HTTP and PostgreSQL listeners.
mod common;
#[allow(dead_code)]
#[path = "common/wire.rs"]
mod wire;
use fedengine_app::FedEngine;
use fedengine_core::*;
use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_postgres::{Client, NoTls};
fn config(temp: &common::TestDirectory) -> Config {
    let mut c = Config::load("config/application.toml").unwrap();
    c.pgwire.listen = "127.0.0.1:0".into();
    c.management.listen = "127.0.0.1:0".into();
    c.duckdb.database_path = temp.path().join("state.duckdb").to_string_lossy().into();
    c.duckdb.temp_directory = temp.path().join("tmp").to_string_lossy().into();
    c.duckdb.threads = 1;
    c.duckdb.max_workers = 2;
    c.duckdb.batch_rows = 1;
    c.duckdb.result_channel_batches = 1;
    c.users[0].password_secret_ref = "USER".into();
    c.users.push(UserConfig {
        name: "reader".into(),
        password_secret_ref: "USER".into(),
    });
    c.virtual_databases = vec![VirtualDatabaseConfig {
        name: "fedengine".into(),
        owner: "fedengine".into(),
        connect: vec![],
        allow_connections: true,
        schemas: vec![],
    }];
    c
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
async fn mutate(address: SocketAddr, method: &str, path: &str, key: &str, body: Value) -> Value {
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
    .expect("publication drain")
}
async fn connect(
    address: SocketAddr,
    user: &str,
    database: &str,
) -> (Client, tokio::task::JoinHandle<()>) {
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(address.port())
        .user(user)
        .password(std::env::var("USER").unwrap())
        .dbname(database)
        .connect(NoTls)
        .await
        .unwrap();
    (
        client,
        tokio::spawn(async move {
            connection.await.unwrap();
        }),
    )
}
async fn login_error(address: SocketAddr, user: &str, database: &str, expected: &str) {
    let error = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(address.port())
        .user(user)
        .password(std::env::var("USER").unwrap())
        .dbname(database)
        .connect(NoTls)
        .await
        .err()
        .expect("login must fail");
    assert_eq!(error.code().unwrap().code(), expected);
}
fn target(receipt: &Value) -> Value {
    json!({"oid":receipt["oid"],"expected_revision":receipt["revision"]})
}
async fn boundary(stream: &mut TcpStream) -> Option<String> {
    let mut error = None;
    loop {
        let (tag, body) = wire::message(stream).await;
        match tag {
            b'E' => {
                error = body
                    .split(|b| *b == 0)
                    .find(|s| s.first() == Some(&b'C'))
                    .map(|s| String::from_utf8(s[1..].to_vec()).unwrap())
            }
            b'Z' => return error,
            _ => {}
        }
    }
}
async fn parse_bind(stream: &mut TcpStream, sql: &str) {
    let mut body = format!("s\0{sql}\0").into_bytes();
    body.extend_from_slice(&0_i16.to_be_bytes());
    wire::packet(stream, b'P', &body).await;
    // Named portal p, statement s, no format codes/parameters/result formats.
    wire::packet(stream, b'B', b"p\0s\0\0\0\0\0\0\0").await;
    wire::packet(stream, b'H', &[]).await;
    assert_eq!(wire::message(stream).await.0, b'1');
    assert_eq!(wire::message(stream).await.0, b'2');
}

#[tokio::test]
async fn online_crud_cas_prepared_identity_connect_admission_and_restart() {
    let temp = common::TestDirectory::new();
    let config = config(&temp);
    let engine = FedEngine::start(config.clone()).await.unwrap();
    let pg = engine.address().unwrap();
    let api = engine.management_address().unwrap();
    let definition = json!({"name":"online","owner":"fedengine","connect":["reader"],"allow_connections":true,"schemas":[]});
    let (status, _) = http(api, "POST", "virtual-databases", None, definition.clone()).await;
    assert_eq!(status, 400);
    let created = mutate(
        api,
        "POST",
        "virtual-databases",
        "create",
        definition.clone(),
    )
    .await;
    assert_eq!(
        created,
        mutate(
            api,
            "POST",
            "virtual-databases",
            "create",
            definition.clone()
        )
        .await
    );
    let (status, _) = http(
        api,
        "POST",
        "virtual-databases",
        Some("create"),
        json!({"name":"different","owner":"fedengine"}),
    )
    .await;
    assert_eq!(status, 409);
    let (status, db) = http(api, "GET", "virtual-databases/online", None, Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(db["oid"], created["oid"]);
    let (reader, reader_task) = connect(pg, "reader", "online").await;
    let stmt = reader.prepare("SELECT current_database()").await.unwrap();
    let mut raw_sessions = Vec::new();
    for _ in 0..4 {
        let mut raw = wire::raw_start(pg, "online").await;
        parse_bind(&mut raw, "SELECT 1 AS value").await;
        raw_sessions.push(raw);
    }
    let changed = json!({"name":"online","owner":"fedengine","connect":[],"allow_connections":false,"schemas":[]});
    let updated = mutate(
        api,
        "PUT",
        "virtual-databases/online",
        "replace",
        json!({"target":target(&created),"definition":changed}),
    )
    .await;
    assert_eq!(updated["oid"], created["oid"]);
    assert_eq!(updated["revision"], 2);
    // All three extended protocol inspection/admission stages reject old descriptors.
    for (mut raw, (tag, body)) in raw_sessions.into_iter().zip([
        (b'D', b"Ss\0".to_vec()),
        (b'B', b"q\0s\0\0\0\0\0\0\0".to_vec()),
        (b'E', b"p\0\0\0\0\0".to_vec()),
        (b'D', b"Pp\0".to_vec()),
    ]) {
        wire::packet(&mut raw, tag, &body).await;
        wire::packet(&mut raw, b'S', &[]).await;
        assert_eq!(boundary(&mut raw).await.as_deref(), Some("0A000"));
    }
    assert_eq!(
        reader
            .query(&stmt, &[])
            .await
            .unwrap_err()
            .code()
            .unwrap()
            .code(),
        "0A000"
    );
    // REVOKE CONNECT / datallowconn changes gate future startup, not admitted sessions.
    reader
        .simple_query("SET search_path TO public; SELECT current_database()")
        .await
        .unwrap();
    assert_eq!(
        reader
            .query_one("SELECT 1", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
    login_error(pg, "reader", "online", "42501").await;
    login_error(pg, "fedengine", "online", "55000").await;
    let (status, _) = http(
        api,
        "DELETE",
        "virtual-databases/online",
        Some("stale"),
        json!({"target":target(&created)}),
    )
    .await;
    assert_eq!(status, 409);
    let deleted = mutate(
        api,
        "DELETE",
        "virtual-databases/online",
        "delete",
        json!({"target":target(&updated)}),
    )
    .await;
    assert_eq!(
        reader
            .simple_query("SELECT 1")
            .await
            .unwrap_err()
            .code()
            .unwrap()
            .code(),
        "3D000"
    );
    let recreated = mutate(
        api,
        "POST",
        "virtual-databases",
        "recreate",
        definition.clone(),
    )
    .await;
    assert_ne!(created["oid"], recreated["oid"]);
    for sql in ["SELECT 1", "SET search_path TO public"] {
        assert_eq!(
            reader
                .simple_query(sql)
                .await
                .unwrap_err()
                .code()
                .unwrap()
                .code(),
            "3D000"
        );
    }
    assert_eq!(
        deleted,
        mutate(
            api,
            "DELETE",
            "virtual-databases/online",
            "delete",
            json!({"target":target(&updated)})
        )
        .await
    );
    let (status, _) = http(
        api,
        "DELETE",
        "virtual-databases/online",
        Some("aba"),
        json!({"target":target(&created)}),
    )
    .await;
    assert_eq!(status, 409);
    let (fresh, fresh_task) = connect(pg, "reader", "online").await;
    assert_eq!(
        fresh
            .query_one("SELECT current_database()", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "online"
    );
    drop(fresh);
    fresh_task.await.unwrap();
    drop(reader);
    reader_task.await.unwrap();
    // The TOML-defined account database is deleted online and must stay deleted on restart.
    let (_, account) = http(api, "GET", "virtual-databases/fedengine", None, Value::Null).await;
    mutate(
        api,
        "DELETE",
        "virtual-databases/fedengine",
        "delete-account",
        json!({"target":target(&account)}),
    )
    .await;
    engine.shutdown().await.unwrap();
    let engine = FedEngine::start(config).await.unwrap();
    let pg = engine.address().unwrap();
    let api = engine.management_address().unwrap();
    let (status, db) = http(api, "GET", "virtual-databases/online", None, Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(db["oid"], recreated["oid"]);
    let (status, _) = http(api, "GET", "virtual-databases/fedengine", None, Value::Null).await;
    assert_eq!(status, 404);
    login_error(pg, "fedengine", "fedengine", "3D000").await;
    assert_eq!(
        created,
        mutate(api, "POST", "virtual-databases", "create", definition).await,
        "replay must return the historical receipt without restoring the old incarnation"
    );
    let (fresh, task) = connect(pg, "reader", "online").await;
    assert_eq!(
        fresh
            .query_one("SELECT oid FROM pg_database WHERE datname='online'", &[])
            .await
            .unwrap()
            .get::<_, u32>(0),
        recreated["oid"].as_u64().unwrap() as u32
    );
    drop(fresh);
    task.await.unwrap();
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn suspended_portal_blocks_publication_without_blocking_status_and_releases_on_close() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(config(&temp)).await.unwrap();
    let pg = engine.address().unwrap();
    let api = engine.management_address().unwrap();
    let (_, db) = http(api, "GET", "virtual-databases/fedengine", None, Value::Null).await;
    let body = json!({"target":target(&db),"definition":{"name":"fedengine","owner":"fedengine","allow_connections":false}});
    let mut raw = wire::raw_start(pg, "fedengine").await;
    let sql = (0..32)
        .map(|i| format!("SELECT {i} AS value"))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    parse_bind(&mut raw, &sql).await;
    wire::packet(&mut raw, b'E', b"p\0\0\0\0\x01").await;
    wire::packet(&mut raw, b'H', &[]).await;
    assert_eq!(wire::until_boundary(&mut raw).await.1, b's');
    let started = std::time::Instant::now();
    let (status, error) = http(
        api,
        "PUT",
        "virtual-databases/fedengine",
        Some("drain"),
        body.clone(),
    )
    .await;
    assert_eq!(status, 409, "{error}");
    assert_eq!(error["code"], "55P03");
    assert!(started.elapsed() < Duration::from_secs(2));
    let (status, after) = http(api, "GET", "virtual-databases/fedengine", None, Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(after["revision"], db["revision"]);
    wire::packet(&mut raw, b'C', b"Pp\0").await;
    wire::packet(&mut raw, b'S', &[]).await;
    assert_eq!(boundary(&mut raw).await, None);
    let receipt = mutate(api, "PUT", "virtual-databases/fedengine", "drain", body).await;
    assert_eq!(receipt["revision"], 2);
    wire::packet(&mut raw, b'X', &[]).await;
    drop(raw);
    engine.shutdown().await.unwrap();
}
