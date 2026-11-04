mod common;
#[path = "common/wire.rs"]
mod wire;
use common::TestDirectory;
#[path = "common/protocol_engine.rs"]
mod protocol_engine;
use fedengine_core::Config;
use protocol_engine::ProtocolEngine as FedEngine;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_postgres::NoTls;
use wire::{authenticate, message, packet, raw_start, until_boundary};
fn config() -> (Config, TestDirectory) {
    let temporary = TestDirectory::new();
    let mut c = Config::load("config/application.toml").unwrap();
    c.management.enabled = false;
    c.duckdb.database_path = ":memory:".into();
    c.pgwire.listen = "127.0.0.1:0".into();
    c.users[0].password_secret_ref = "USER".into();
    c.duckdb.max_workers = 2;
    c.duckdb.threads = 1;
    c.duckdb.batch_rows = 32;
    c.duckdb.result_channel_batches = 2;
    c.duckdb.temp_directory = temporary.path().to_string_lossy().into_owned();
    (c, temporary)
}
async fn connect(engine: &FedEngine) -> (tokio_postgres::Client, tokio::task::JoinHandle<()>) {
    let mut c = tokio_postgres::Config::new();
    c.host("127.0.0.1")
        .port(engine.address().unwrap().port())
        .user("fedengine")
        .dbname("protocol_db")
        .password(std::env::var("USER").unwrap());
    let (client, connection) = c.connect(NoTls).await.unwrap();
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, task)
}
#[tokio::test]
async fn scram_simple_extended_parameters_null_and_error_recovery() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let (c, t) = connect(&e).await;
    let r = c
        .simple_query("SELECT ';' AS marker; SELECT 7 AS n")
        .await
        .unwrap();
    assert_eq!(r.len(), 6);
    let row = c
        .query_one(
            "SELECT $1::INTEGER + 1 AS n, $2::VARCHAR AS s",
            &[&41i32, &Option::<String>::None],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), 42);
    assert_eq!(row.get::<_, Option<String>>(1), None);
    assert_eq!(
        c.simple_query("BEGIN")
            .await
            .unwrap_err()
            .code()
            .unwrap()
            .code(),
        "0A000"
    );
    assert!(c.prepare("SELECT read_text('/etc/passwd')").await.is_err());
    assert_eq!(
        c.query_one("SELECT 9::INTEGER", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        9
    );
    let prepared = c.prepare("SELECT $1::INTEGER").await.unwrap();
    assert!(c.query(&prepared, &[&"bad"]).await.is_err());
    drop(c);
    t.await.unwrap();
    e.shutdown().await.unwrap();
    assert_eq!(e.handle.health().pending.operations, 0);
    assert!(e.metrics().admitted > 0);
    assert!(e.metrics().rows_streamed >= 3);
    assert!(e.metrics().execution_micros > 0);
    assert_eq!(e.handle.task_metrics().tasks, 0);
    assert_eq!(e.metrics().active_workers, 0);
    assert_eq!(e.metrics().active_sessions, 0);
    assert_eq!(e.metrics().worker_waiters, 0);
}
#[tokio::test]
async fn real_cancel_request_interrupts_worker_and_drains() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let (c, t) = connect(&e).await;
    let cancel = c.cancel_token();
    {
        let query = c.simple_query(
            "SELECT sum(a.i*b.j) FROM range(1000000000) a(i), range(1000000000) b(j)",
        );
        tokio::pin!(query);
        tokio::select! {r=&mut query=>panic!("query ended too early: {r:?}"),_=tokio::time::sleep(Duration::from_millis(100))=>{}}
        cancel.cancel_query(NoTls).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), query)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code().unwrap().code(), "57014");
    }
    assert_eq!(
        c.query_one("SELECT 1::INTEGER", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
    drop(c);
    t.await.unwrap();
    e.shutdown().await.unwrap();
    assert_eq!(e.handle.health().pending.operations, 0);
}
#[tokio::test]
async fn tls_required_without_cert_rejects_before_listener() {
    let (mut c, _temporary) = config();
    c.pgwire.tls_required = true;
    assert!(FedEngine::start(c).await.is_err());
}
#[tokio::test]
async fn rejected_credentials_and_ssl_negotiation() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let mut stream = TcpStream::connect(e.address().unwrap()).await.unwrap();
    stream
        .write_all(&[0, 0, 0, 8, 4, 210, 22, 47])
        .await
        .unwrap();
    assert_eq!(stream.read_u8().await.unwrap(), b'N');
    drop(stream);
    let s = format!(
        "host=127.0.0.1 port={} user=fedengine password=wrong",
        e.address().unwrap().port()
    );
    assert!(tokio_postgres::connect(&s, NoTls).await.is_err());
    e.shutdown().await.unwrap();
}
#[tokio::test]
async fn named_portal_resumes_same_execution_without_materializing_all_rows() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let mut s = raw_start(e.address().unwrap(), "protocol_db").await;
    packet(
        &mut s,
        b'P',
        b"stmt\0SELECT i::INTEGER FROM range(5) t(i)\0\0\0",
    )
    .await;
    packet(&mut s, b'B', b"portal\0stmt\0\0\0\0\0\0\0").await;
    let mut execute = b"portal\0".to_vec();
    execute.extend_from_slice(&2i32.to_be_bytes());
    packet(&mut s, b'E', &execute).await;
    packet(&mut s, b'H', &[]).await;
    let (r, tag) = until_boundary(&mut s).await;
    assert_eq!(r, ["0", "1"]);
    assert_eq!(tag, b's');
    packet(&mut s, b'E', &execute).await;
    packet(&mut s, b'H', &[]).await;
    let (r, tag) = until_boundary(&mut s).await;
    assert_eq!(r, ["2", "3"]);
    assert_eq!(tag, b's');
    packet(&mut s, b'E', &execute).await;
    packet(&mut s, b'H', &[]).await;
    let (r, tag) = until_boundary(&mut s).await;
    assert_eq!(r, ["4"]);
    assert_eq!(tag, b'C');
    packet(&mut s, b'S', &[]).await;
    assert_eq!(message(&mut s).await.0, b'Z');
    drop(s);
    e.shutdown().await.unwrap();
    assert_eq!(e.handle.health().pending.operations, 0);
}

#[tokio::test]
async fn metadata_portal_resumes_and_catalog_cancel_releases_worker() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let mut s = raw_start(e.address().unwrap(), "protocol_db").await;
    packet(&mut s, b'P', b"stmt\0SELECT typname FROM pg_catalog.pg_type WHERE oid IN (16,20,21,23,25) ORDER BY oid\0\0\0").await;
    packet(&mut s, b'B', b"portal\0stmt\0\0\0\0\0\0\0").await;
    let mut execute = b"portal\0".to_vec();
    execute.extend_from_slice(&2i32.to_be_bytes());
    for (expected, boundary) in [
        (vec!["bool", "int8"], b's'),
        (vec!["int2", "int4"], b's'),
        (vec!["text"], b'C'),
    ] {
        packet(&mut s, b'E', &execute).await;
        packet(&mut s, b'H', &[]).await;
        let (rows, tag) = until_boundary(&mut s).await;
        assert_eq!(rows, expected);
        assert_eq!(tag, boundary);
    }
    packet(&mut s, b'S', &[]).await;
    assert_eq!(message(&mut s).await.0, b'Z');
    drop(s);
    let (client, connection) = connect(&e).await;
    let cancel = client.cancel_token();
    let running = tokio::spawn(async move {
        let error = client.simple_query("SELECT sum(a.oid::BIGINT*b.oid::BIGINT+c.oid::BIGINT+d.oid::BIGINT+f.oid::BIGINT+g.oid::BIGINT) FROM pg_catalog.pg_type a CROSS JOIN pg_catalog.pg_type b CROSS JOIN pg_catalog.pg_type c CROSS JOIN pg_catalog.pg_type d CROSS JOIN pg_catalog.pg_type f CROSS JOIN pg_catalog.pg_type g").await.unwrap_err();
        assert_eq!(error.code().unwrap().code(), "57014");
        assert_eq!(
            client
                .query_one(
                    "SELECT oid FROM pg_catalog.pg_type WHERE typname='int4'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, u32>(0),
            23
        );
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel_query(NoTls).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    connection.await.unwrap();
    e.shutdown().await.unwrap();
    assert_eq!(e.metrics().active_workers, 0);
    assert_eq!(e.handle.task_metrics().tasks, 0);
}

#[tokio::test]
async fn supplied_oid_infers_uncast_parameter_and_preserves_binary_scalars() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let (c, t) = connect(&e).await;
    let s = c
        .prepare_typed("SELECT $1", &[tokio_postgres::types::Type::INT4])
        .await
        .unwrap();
    assert_eq!(
        c.query_one(&s, &[&123i32]).await.unwrap().get::<_, i32>(0),
        123
    );
    let row = c
        .query_one(
            "SELECT $1::BOOLEAN, $2::BLOB, $3::DOUBLE",
            &[&true, &vec![0u8, 255, 1], &1.25f64],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>(0));
    assert_eq!(row.get::<_, Vec<u8>>(1), vec![0, 255, 1]);
    assert_eq!(row.get::<_, f64>(2), 1.25);
    assert!(c.prepare("SELECT $1").await.is_err());
    drop(c);
    t.await.unwrap();
    e.shutdown().await.unwrap();
}
#[tokio::test]
async fn tls_handshake_scram_query_and_plaintext_rejection() {
    use tokio_rustls::{
        TlsConnector,
        rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
    };
    let (mut cfg, _temporary) = config();
    cfg.pgwire.tls_required = true;
    cfg.pgwire.tls_cert = Some("tests/fixtures/server.crt".into());
    cfg.pgwire.tls_key = Some("tests/fixtures/server.key".into());
    let e = FedEngine::start(cfg).await.unwrap();
    let plaintext = format!(
        "host=127.0.0.1 port={} user=fedengine password=wrong",
        e.address().unwrap().port()
    );
    assert!(tokio_postgres::connect(&plaintext, NoTls).await.is_err());
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut std::io::BufReader::new(
        include_bytes!("fixtures/server.crt").as_slice(),
    )) {
        roots.add(cert.unwrap()).unwrap();
    }
    let connector = TlsConnector::from(std::sync::Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    let mut socket = TcpStream::connect(e.address().unwrap()).await.unwrap();
    socket
        .write_all(&[0, 0, 0, 8, 4, 210, 22, 47])
        .await
        .unwrap();
    assert_eq!(socket.read_u8().await.unwrap(), b'S');
    let mut stream = connector
        .connect(ServerName::try_from("localhost").unwrap(), socket)
        .await
        .unwrap();
    authenticate(&mut stream, "protocol_db").await;
    packet(&mut stream, b'Q', b"SELECT 42::INTEGER\0").await;
    assert_eq!(until_boundary(&mut stream).await.0, ["42"]);
    drop(stream);
    e.shutdown().await.unwrap();
}
#[tokio::test]
async fn shutdown_cancels_running_query_and_full_suspended_portal() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("fedengine=info,modorch=info")
        .with_test_writer()
        .try_init();
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let mut s = raw_start(e.address().unwrap(), "protocol_db").await;
    packet(
        &mut s,
        b'P',
        b"stmt\0SELECT i FROM range(1000000000) t(i)\0\0\0",
    )
    .await;
    packet(&mut s, b'B', b"p\0stmt\0\0\0\0\0\0\0").await;
    let mut execute = b"p\0".to_vec();
    execute.extend_from_slice(&1i32.to_be_bytes());
    packet(&mut s, b'E', &execute).await;
    packet(&mut s, b'H', &[]).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), until_boundary(&mut s))
            .await
            .unwrap()
            .1,
        b's'
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let address = e.address().unwrap();
    tokio::time::timeout(Duration::from_secs(5), e.shutdown_until_drained())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(e.handle.task_metrics().tasks, 0);
    assert_eq!(e.metrics().active_workers, 0);
    assert_eq!(e.metrics().active_sessions, 0);
    assert_eq!(e.metrics().worker_waiters, 0);
    assert_eq!(e.handle.health().pending.operations, 0);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(address))
            .await
            .unwrap()
            .is_err()
    );
}
#[tokio::test]
async fn startup_bind_failure_compensates_and_port_can_be_reused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (mut c, _temporary) = config();
    c.pgwire.listen = address.to_string();
    assert!(FedEngine::start(c.clone()).await.is_err());
    drop(listener);
    let e = FedEngine::start(c).await.unwrap();
    e.shutdown().await.unwrap();
    assert!(tokio::net::TcpListener::bind(address).await.is_ok());
}

#[tokio::test]
async fn date_time_uuid_and_exact_decimal_wire_values() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let (c, t) = connect(&e).await;
    let date = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    let time = chrono::NaiveTime::from_hms_micro_opt(12, 34, 56, 123456).unwrap();
    let stamp = date.and_time(time);
    let uuid = uuid::Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap();
    let row = c
        .query_one(
            "SELECT $1::DATE,$2::TIME,$3::TIMESTAMP,$4::UUID",
            &[&date, &time, &stamp, &uuid],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, chrono::NaiveDate>(0), date);
    assert_eq!(row.get::<_, chrono::NaiveTime>(1), time);
    assert_eq!(row.get::<_, chrono::NaiveDateTime>(2), stamp);
    assert_eq!(row.get::<_, uuid::Uuid>(3), uuid);
    let rows = c
        .simple_query("SELECT 12345678901234567890.123400::DECIMAL(28,6),NULL::INTEGER")
        .await
        .unwrap();
    let tokio_postgres::SimpleQueryMessage::Row(row) = &rows[1] else {
        panic!("expected a row")
    };
    assert_eq!(row.get(0), Some("12345678901234567890.123400"));
    assert_eq!(row.get(1), None);
    drop(c);
    t.await.unwrap();
    e.shutdown().await.unwrap();
}

#[tokio::test]
async fn named_objects_reject_duplicates_and_sync_close_release_quotas() {
    let (mut cfg, _temporary) = config();
    cfg.pgwire.max_statements = 1;
    cfg.pgwire.max_portals = 1;
    let e = FedEngine::start(cfg).await.unwrap();
    let mut stream = raw_start(e.address().unwrap(), "protocol_db").await;
    packet(&mut stream, b'P', b"s\0SELECT 1::INTEGER\0\0\0").await;
    packet(&mut stream, b'H', &[]).await;
    assert_eq!(message(&mut stream).await.0, b'1');
    packet(&mut stream, b'P', b"s\0SELECT 2::INTEGER\0\0\0").await;
    packet(&mut stream, b'H', &[]).await;
    let (tag, body) = message(&mut stream).await;
    assert_eq!(tag, b'E');
    assert!(body.windows(6).any(|w| w == b"42P05\0"));
    packet(&mut stream, b'S', &[]).await;
    assert_eq!(message(&mut stream).await.0, b'Z');
    packet(&mut stream, b'B', b"p\0s\0\0\0\0\0\0\0").await;
    packet(&mut stream, b'H', &[]).await;
    assert_eq!(message(&mut stream).await.0, b'2');
    packet(&mut stream, b'B', b"p\0s\0\0\0\0\0\0\0").await;
    packet(&mut stream, b'H', &[]).await;
    let (tag, body) = message(&mut stream).await;
    assert_eq!(tag, b'E');
    assert!(body.windows(6).any(|w| w == b"42P03\0"));
    packet(&mut stream, b'S', &[]).await;
    assert_eq!(message(&mut stream).await.0, b'Z');
    packet(&mut stream, b'B', b"new\0s\0\0\0\0\0\0\0").await;
    packet(&mut stream, b'H', &[]).await;
    assert_eq!(message(&mut stream).await.0, b'2');
    packet(&mut stream, b'C', b"Ss\0").await;
    packet(&mut stream, b'H', &[]).await;
    assert_eq!(message(&mut stream).await.0, b'3');
    packet(&mut stream, b'P', b"new_stmt\0SELECT 3::INTEGER\0\0\0").await;
    packet(&mut stream, b'H', &[]).await;
    assert_eq!(message(&mut stream).await.0, b'1');
    drop(stream);
    e.shutdown().await.unwrap();
}

#[tokio::test]
async fn catalog_wire_types_session_execute_and_namespace_invalidation() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let (c, t) = connect(&e).await;
    let row = c
        .query_one(
            "SELECT oid, typlen, typname FROM pg_catalog.pg_type WHERE typname='int4'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, u32>(0), 23);
    assert_eq!(row.get::<_, i16>(1), 4);
    assert_eq!(row.get::<_, String>(2), "int4");
    let setting = c
        .prepare("SET application_name TO 'metadata regression'")
        .await
        .unwrap();
    // Parse/Describe must not change settings.
    assert_eq!(
        c.query_one("SELECT current_setting('application_name')", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        ""
    );
    c.execute(&setting, &[]).await.unwrap();
    assert_eq!(
        c.query_one("SELECT current_setting('application_name')", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "metadata regression"
    );
    let old = c
        .prepare("SELECT oid FROM pg_catalog.pg_type WHERE typname='int4'")
        .await
        .unwrap();
    c.simple_query("SET search_path TO pg_catalog")
        .await
        .unwrap();
    assert_eq!(
        c.query(&old, &[]).await.unwrap_err().code().unwrap().code(),
        "0A000"
    );
    c.simple_query("RESET ALL").await.unwrap();
    assert_eq!(
        c.query_one("SELECT current_setting('application_name')", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        ""
    );
    for sql in [
        "SELECT * FROM _fedengine.meta_pg_type",
        "SELECT * FROM pg_catalog.pg_type, read_csv('/etc/passwd')",
        "SELECT * INTO stolen FROM pg_catalog.pg_type",
    ] {
        assert!(c.simple_query(sql).await.is_err(), "{sql}");
    }
    drop(c);
    t.await.unwrap();
    e.shutdown().await.unwrap();
    assert_eq!(e.metrics().active_workers, 0);
    assert_eq!(e.handle.task_metrics().tasks, 0);
}

#[tokio::test]
async fn unknown_startup_database_is_rejected() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let mut c = tokio_postgres::Config::new();
    c.host("127.0.0.1")
        .port(e.address().unwrap().port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .dbname("unregistered_catalog");
    let error = match c.connect(NoTls).await {
        Ok(_) => panic!("unknown database accepted"),
        Err(e) => e,
    };
    assert_eq!(error.code().unwrap().code(), "3D000");
    e.shutdown().await.unwrap();
}

#[tokio::test]
async fn omitted_and_empty_startup_database_use_login_user_database() {
    let (mut config, _temporary) = config();
    config.users[0].name = "protocol_db".into();
    let engine = FedEngine::start(config).await.unwrap();
    // Send Startup packets directly: client libraries may fill in a database
    // themselves, which would leave the server fallback untested.
    for database in [None, Some(""), Some("protocol_db")] {
        let mut stream = TcpStream::connect(engine.address().unwrap()).await.unwrap();
        wire::authenticate_as(&mut stream, "protocol_db", database).await;
        for sql in [
            b"SELECT current_database()\0".as_slice(),
            b"SELECT current_user\0".as_slice(),
        ] {
            packet(&mut stream, b'Q', sql).await;
            let (rows, boundary) = until_boundary(&mut stream).await;
            assert_eq!(rows, ["protocol_db"]);
            assert_eq!(boundary, b'C');
            assert_eq!(message(&mut stream).await.0, b'Z');
        }
    }
    engine.shutdown().await.unwrap();
}
