//! Real listener regressions for explicit logical databases and terminal startup errors.
mod common;
#[allow(dead_code)]
#[path = "common/wire.rs"]
mod wire;
use fedengine_app::FedEngine;
use fedengine_core::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_postgres::NoTls;

fn config(temp: &common::TestDirectory) -> Config {
    let mut c = Config::load("config/application.toml").unwrap();
    c.pgwire.listen = "127.0.0.1:0".into();
    c.management.listen = "127.0.0.1:0".into();
    c.duckdb.database_path = temp
        .path()
        .join("state.duckdb")
        .to_string_lossy()
        .into_owned();
    c.duckdb.temp_directory = temp.path().join("tmp").to_string_lossy().into_owned();
    c.duckdb.threads = 1;
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
async fn failure(
    address: std::net::SocketAddr,
    user: &str,
    password: &str,
    database: Option<&str>,
    expected: &str,
) {
    use postgres_protocol::authentication::sasl::{ChannelBinding, ScramSha256};
    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut body = format!("\0\x03\0\0user\0{user}\0").into_bytes();
    if let Some(db) = database {
        body.extend_from_slice(format!("database\0{db}\0").as_bytes());
    }
    body.push(0);
    stream.write_i32((body.len() + 4) as i32).await.unwrap();
    stream.write_all(&body).await.unwrap();
    let mut scram = ScramSha256::new(password.as_bytes(), ChannelBinding::unrequested());
    loop {
        let (tag, b) = wire::message(&mut stream).await;
        match tag {
            b'R' => match i32::from_be_bytes(b[..4].try_into().unwrap()) {
                10 => {
                    let mut p = b"SCRAM-SHA-256\0".to_vec();
                    p.extend_from_slice(&(scram.message().len() as i32).to_be_bytes());
                    p.extend_from_slice(scram.message());
                    wire::packet(&mut stream, b'p', &p).await;
                }
                11 => {
                    scram.update(&b[4..]).unwrap();
                    wire::packet(&mut stream, b'p', scram.message()).await;
                }
                12 => scram.finish(&b[4..]).unwrap(),
                other => panic!("AuthenticationOk or unexpected code before rejection: {other}"),
            },
            b'E' => {
                let fields = b
                    .split(|b| *b == 0)
                    .filter(|f| !f.is_empty())
                    .map(|f| (f[0], String::from_utf8(f[1..].to_vec()).unwrap()))
                    .collect::<std::collections::BTreeMap<_, _>>();
                assert_eq!(fields[&b'S'], "FATAL");
                assert_eq!(fields[&b'C'], expected);
                let end = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read_u8())
                    .await
                    .unwrap();
                assert!(
                    matches!(end,Err(e) if e.kind()==std::io::ErrorKind::UnexpectedEof),
                    "startup failure must end with EOF, not ReadyForQuery"
                );
                break;
            }
            other => panic!("unexpected startup response: {other}"),
        }
    }
}
#[tokio::test]
async fn explicit_user_database_defaults_public_scope_and_stable_identity() {
    let temp = common::TestDirectory::new();
    let c = config(&temp);
    let engine = FedEngine::start(c.clone()).await.unwrap();
    let address = engine.address().unwrap();
    for database in [None, Some(""), Some("fedengine")] {
        let mut stream = TcpStream::connect(address).await.unwrap();
        wire::authenticate_as(&mut stream, "fedengine", database).await;
        wire::packet(&mut stream, b'X', &[]).await;
    }
    let conn = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(address.port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .connect(NoTls)
        .await
        .unwrap();
    let (client, connection) = conn;
    let task = tokio::spawn(async move {
        connection.await.unwrap();
    });
    let row=client.query_one("SELECT current_database(),current_user,current_schema(),current_setting('search_path')",&[]).await.unwrap();
    assert_eq!(row.get::<_, String>(0), "fedengine");
    assert_eq!(row.get::<_, String>(1), "fedengine");
    assert_eq!(row.get::<_, String>(2), "public");
    assert_eq!(row.get::<_, String>(3), "\"$user\", public");
    let oid: u32 = client
        .query_one("SELECT oid FROM pg_database WHERE datname='fedengine'", &[])
        .await
        .unwrap()
        .get(0);
    let schemas = client
        .query("SELECT nspname FROM pg_namespace ORDER BY nspname", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>();
    assert_eq!(schemas, ["information_schema", "pg_catalog", "public"]);
    client
        .batch_execute("SET search_path TO missing, public")
        .await
        .unwrap();
    assert_eq!(
        client
            .query_one("SELECT current_schema()", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "public"
    );
    client.batch_execute("RESET search_path").await.unwrap();
    assert_eq!(
        client
            .query_one("SHOW search_path", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "\"$user\", public"
    );
    let error = client
        .query("SELECT * FROM another.public.t", &[])
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "0A000");
    drop(client);
    task.await.unwrap();
    engine.shutdown().await.unwrap();
    let engine = FedEngine::start(c).await.unwrap();
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(engine.address().unwrap().port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .connect(NoTls)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        connection.await.unwrap();
    });
    assert_eq!(
        client
            .query_one("SELECT oid FROM pg_database WHERE datname='fedengine'", &[])
            .await
            .unwrap()
            .get::<_, u32>(0),
        oid
    );
    drop(client);
    task.await.unwrap();
    engine.shutdown().await.unwrap();
}
#[tokio::test]
async fn startup_authentication_precedes_database_checks_and_failures_are_terminal() {
    let temp = common::TestDirectory::new();
    let mut c = config(&temp);
    let password = std::env::var("USER").unwrap();
    let engine = FedEngine::start(c.clone()).await.unwrap();
    let addr = engine.address().unwrap();
    for database in [Some("missing"), Some("fedengine")] {
        failure(addr, "fedengine", "wrong-password", database, "28P01").await;
    }
    failure(addr, "unknown", &password, Some("missing"), "28P01").await;
    failure(addr, "fedengine", &password, Some("missing"), "3D000").await;
    failure(addr, "fedengine", &password, Some(" fedengine"), "3D000").await;
    failure(addr, "reader", &password, Some("fedengine"), "42501").await;
    failure(addr, "reader", &password, None, "3D000").await;
    engine.shutdown().await.unwrap();
    c.virtual_databases[0].allow_connections = false;
    let engine = FedEngine::start(c).await.unwrap();
    failure(
        engine.address().unwrap(),
        "fedengine",
        &password,
        None,
        "55000",
    )
    .await;
    engine.shutdown().await.unwrap();
}

#[test]
fn postgres_qualified_wildcards_columns_and_correlated_queries_execute_after_binding() {
    use fedengine_ddl::{Analyzer, BindingNamespace};
    let c = Config::load("examples/config/federation.toml").unwrap();
    let a = Analyzer {
        sources: c.datasources,
        indexes: vec![],
        namespace: Some(BindingNamespace {
            catalog: "fedengine".into(),
            allow_cross_database: false,
            search_path: vec!["a".into()],
            bindings: [
                ("a".into(), (DataSourceId("crm".into()), "public".into())),
                ("b".into(), (DataSourceId("erp".into()), "erp".into())),
            ]
            .into(),
            relations: [("a".into(), "t".into()), ("b".into(), "t".into())].into(),
        }),
    };
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch("ATTACH ':memory:' AS pg_crm; ATTACH ':memory:' AS mysql_erp; CREATE SCHEMA pg_crm.public; CREATE SCHEMA mysql_erp.erp; CREATE TABLE pg_crm.public.t(i INTEGER); CREATE TABLE mysql_erp.erp.t(i INTEGER); INSERT INTO pg_crm.public.t VALUES(1); INSERT INTO mysql_erp.erp.t VALUES(1)").unwrap();
    for sql in [
        "SELECT a.t.*, b.t.* FROM a.t JOIN fedengine.b.t USING(i)",
        "SELECT a.t.i, fedengine.b.t.i FROM a.t JOIN fedengine.b.t ON a.t.i=fedengine.b.t.i",
        "SELECT t.i FROM a.t",
        "SELECT fedengine.a.t.i FROM a.t",
        "SELECT a.t.i FROM a.t WHERE EXISTS(SELECT 1 FROM (SELECT 1 AS i) t WHERE t.i=1)",
        "SELECT \"a\".\"t\".* FROM \"fedengine\".\"a\".\"t\"",
        "WITH x AS (SELECT a.t.* FROM a.t) SELECT x.* FROM x",
        "SELECT q.* FROM (SELECT a.t.* FROM a.t) q",
        "SELECT a.t.i FROM a.t ORDER BY a.t.i",
        "SELECT a.t.i FROM a.t WHERE EXISTS(SELECT 1 FROM b.t WHERE b.t.i=a.t.i)",
    ] {
        let bound = a.analyze(sql).unwrap();
        let value = conn
            .query_row(&bound.sql, [], |r| r.get::<_, i32>(0))
            .unwrap_or_else(|e| panic!("{sql}: {}: {e}", bound.sql));
        assert_eq!(value, 1);
    }
    assert_eq!(
        a.analyze("SELECT t.* FROM a.t JOIN b.t USING(i)")
            .unwrap_err()
            .sqlstate,
        "42702"
    );
    assert_eq!(
        a.analyze("SELECT a.t.* FROM a.t q").unwrap_err().sqlstate,
        "42P01"
    );
}
