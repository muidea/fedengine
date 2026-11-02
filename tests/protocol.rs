mod common;
use common::TestDirectory;
use fedengine_app::FedEngine;
use fedengine_core::Config;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_postgres::NoTls;
fn config() -> (Config, TestDirectory) {
    let temporary = TestDirectory::new();
    let mut c = Config::load("config/application.toml").unwrap();
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
async fn packet<S: tokio::io::AsyncWrite + Unpin>(stream: &mut S, tag: u8, body: &[u8]) {
    stream.write_u8(tag).await.unwrap();
    stream.write_i32((body.len() + 4) as i32).await.unwrap();
    stream.write_all(body).await.unwrap();
}
async fn message<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> (u8, Vec<u8>) {
    let tag = stream.read_u8().await.unwrap();
    let n = stream.read_i32().await.unwrap();
    assert!((4..1_000_000).contains(&n));
    let mut b = vec![0; n as usize - 4];
    stream.read_exact(&mut b).await.unwrap();
    (tag, b)
}
async fn raw_start(e: &FedEngine) -> TcpStream {
    let mut stream = TcpStream::connect(e.address().unwrap()).await.unwrap();
    authenticate(&mut stream).await;
    stream
}
async fn authenticate<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut stream: &mut S) {
    use postgres_protocol::authentication::sasl::{ChannelBinding, ScramSha256};
    let body = b"\0\x03\0\0user\0fedengine\0database\0fedengine\0\0";
    stream.write_i32((body.len() + 4) as i32).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut scram = ScramSha256::new(
        std::env::var("USER").unwrap().as_bytes(),
        ChannelBinding::unrequested(),
    );
    loop {
        let (tag, b) = message(&mut stream).await;
        match tag {
            b'R' => {
                let code = i32::from_be_bytes(b[..4].try_into().unwrap());
                match code {
                    10 => {
                        let mut p = b"SCRAM-SHA-256\0".to_vec();
                        p.extend_from_slice(&(scram.message().len() as i32).to_be_bytes());
                        p.extend_from_slice(scram.message());
                        packet(&mut stream, b'p', &p).await
                    }
                    11 => {
                        scram.update(&b[4..]).unwrap();
                        packet(&mut stream, b'p', scram.message()).await
                    }
                    12 => scram.finish(&b[4..]).unwrap(),
                    0 => {}
                    _ => panic!("unsupported authentication"),
                }
            }
            b'Z' => break,
            b'E' => panic!("auth error: {:?}", String::from_utf8_lossy(&b)),
            _ => {}
        }
    }
}
async fn until_boundary<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> (Vec<String>, u8) {
    let mut rows = vec![];
    loop {
        let (tag, b) = message(stream).await;
        match tag {
            b'D' => {
                let n = i32::from_be_bytes(b[2..6].try_into().unwrap());
                rows.push(String::from_utf8(b[6..6 + n as usize].to_vec()).unwrap());
            }
            b's' | b'C' => return (rows, tag),
            b'E' => panic!("protocol error: {:?}", String::from_utf8_lossy(&b)),
            _ => {}
        }
    }
}
#[tokio::test]
async fn named_portal_resumes_same_execution_without_materializing_all_rows() {
    let (config, _temporary) = config();
    let e = FedEngine::start(config).await.unwrap();
    let mut s = raw_start(&e).await;
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
    authenticate(&mut stream).await;
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
    let mut s = raw_start(&e).await;
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
    let mut stream = raw_start(&e).await;
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
