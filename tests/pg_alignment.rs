//! Regression of deployed F01–F06 against the real coordinator, engine and listener.
mod common;
#[allow(dead_code)]
#[path = "common/wire.rs"]
mod wire;
use fedengine_app::FedEngine;
use fedengine_core::*;
use tokio_postgres::{NoTls, SimpleQueryMessage, types::Type};

fn configuration(temp: &common::TestDirectory) -> Config {
    let mut config = Config::load("config/application.toml").unwrap();
    config.pgwire.listen = "127.0.0.1:0".into();
    config.management.listen = "127.0.0.1:0".into();
    config.duckdb.database_path = temp.path().join("state.duckdb").to_string_lossy().into();
    config.duckdb.temp_directory = temp.path().join("tmp").to_string_lossy().into();
    config.duckdb.threads = 1;
    config.users[0].password_secret_ref = "USER".into();
    config.virtual_databases = vec![VirtualDatabaseConfig {
        name: "fedengine".into(),
        owner: "fedengine".into(),
        connect: vec![],
        allow_connections: true,
        schemas: vec![],
    }];
    config
}
async fn connect(engine: &FedEngine) -> (tokio_postgres::Client, tokio::task::JoinHandle<()>) {
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(engine.address().unwrap().port())
        .user("fedengine")
        .password(std::env::var("USER").unwrap())
        .dbname("fedengine")
        .connect(NoTls)
        .await
        .unwrap();
    (client, tokio::spawn(async { connection.await.unwrap() }))
}
#[tokio::test]
async fn division_errors_and_privileges_match_postgres() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let (client, task) = connect(&engine).await;
    for (sql, expected) in [
        ("SELECT 1/2", 0),
        ("SELECT 1::integer/2::integer", 0),
        ("SELECT -5/2", -2),
        ("SELECT 5/-2", -2),
        ("SELECT (9/2)/2", 2),
        ("WITH t(a,b) AS (VALUES(5,2)) SELECT a/b FROM t", 2),
    ] {
        let row = client.query_one(sql, &[]).await.unwrap();
        assert_eq!(*row.columns()[0].type_(), Type::INT4, "{sql}");
        assert_eq!(row.columns()[0].name(), "?column?", "{sql}");
        assert_eq!(row.get::<_, i32>(0), expected, "{sql}");
    }
    assert_eq!(
        client
            .query_one(
                &client
                    .prepare_typed("SELECT $1/$2", &[Type::INT8, Type::INT8])
                    .await
                    .unwrap(),
                &[&5i64, &2i64]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    assert_eq!(
        client
            .query_one("SELECT $1::real/$2::real", &[&5f32, &2f32])
            .await
            .unwrap()
            .get::<_, f32>(0),
        2.5
    );
    assert_eq!(
        client
            .query_one("SELECT NULL::integer/0", &[])
            .await
            .unwrap()
            .get::<_, Option<i32>>(0),
        None
    );
    for (sql, state) in [
        ("SELECT 1/0", "22012"),
        ("SELECT 1::double precision/0::double precision", "22012"),
        ("SELECT 'a'::integer", "22P02"),
        ("SELECT '__fed_pg_division_by_zero'::integer", "22P02"),
        (
            "SELECT 'Invalid Input Error: __fed_pg_division_by_zero'::integer",
            "22P02",
        ),
        (
            "SELECT 'Invalid Input Error: __fed_pg_numeric_division'::integer",
            "22P02",
        ),
        ("SELECT (-2147483648)::integer/(-1)::integer", "22003"),
        ("SELECT '{\"a\":1}'::jsonb", "0A000"),
        // Decimal division cannot silently use DuckDB's inexact floating-point result.
        ("SELECT 1::decimal(10,2)/3::decimal(10,2)", "0A000"),
    ] {
        let error = client.simple_query(sql).await.unwrap_err();
        assert_eq!(error.code().unwrap().code(), state, "{sql}: {error}");
        assert_eq!(
            client
                .query_one("SELECT 1", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            1
        );
    }
    for privilege in ["CONNECT", "connect", "CoNnEcT"] {
        let row = client
            .query_one(
                "SELECT has_database_privilege(current_database(),$1::text)",
                &[&privilege],
            )
            .await
            .unwrap();
        assert!(row.get::<_, bool>(0));
    }
    // Rewriting must preserve strings and the projected label of ordinary expressions.
    let messages = client
        .simple_query("SELECT '1/0' AS literal")
        .await
        .unwrap();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m,SimpleQueryMessage::Row(r) if r.get(0)==Some("1/0")))
    );
    drop(client);
    task.await.unwrap();
    engine.shutdown_until_drained().await.unwrap();
}
#[tokio::test]
async fn timestamptz_preserves_instant_for_text_and_binary_parameters() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let (client, task) = connect(&engine).await;
    let instant = chrono::DateTime::parse_from_rfc3339("2024-02-29T12:34:56.123456Z")
        .unwrap()
        .to_utc();
    let row = client
        .query_one("SELECT $1::timestamptz", &[&instant])
        .await
        .unwrap();
    assert_eq!(row.get::<_, chrono::DateTime<chrono::Utc>>(0), instant);
    let row = client
        .query_one(
            "SELECT $1::text::timestamptz",
            &[&"2024-02-29 20:34:56.123456+08"],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, chrono::DateTime<chrono::Utc>>(0), instant);
    let rows=client.simple_query("SELECT '2024-02-29 20:34:56.123456+08'::timestamptz, '2024-02-29 12:34:56.123456'::timestamp").await.unwrap();
    assert!(rows.iter().any(|m| matches!(m,SimpleQueryMessage::Row(r) if r.get(0)==Some("2024-02-29 12:34:56.123456+00") && r.get(1)==Some("2024-02-29 12:34:56.123456"))));
    drop(client);
    task.await.unwrap();
    engine.shutdown_until_drained().await.unwrap();
}
#[tokio::test]
async fn missing_portals_use_34000_and_sync_recovers() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let mut stream = wire::raw_start(engine.address().unwrap(), "fedengine").await;
    for (tag, body) in [
        (b'E', b"missing\0\0\0\0\0".as_slice()),
        (b'D', b"Pmissing\0".as_slice()),
    ] {
        wire::packet(&mut stream, tag, body).await;
        wire::packet(&mut stream, b'H', &[]).await;
        let (tag, payload) = wire::message(&mut stream).await;
        assert_eq!(tag, b'E');
        assert!(payload.windows(6).any(|v| v == b"34000\0"));
        wire::packet(&mut stream, b'S', &[]).await;
        assert_eq!(wire::message(&mut stream).await.0, b'Z');
    }
    drop(stream);
    engine.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
async fn decode_returns_bytea_in_simple_and_extended_protocols() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let (client, task) = connect(&engine).await;
    for (value, format, expected) in [
        ("00ff 20\t4142", "HEX", vec![0, 255, 32, 65, 66]),
        ("AA==\n", "base64", vec![0]),
        ("YQ==YQ==", "base64", vec![97, 97]),
        ("AA=A", "base64", vec![0]),
        ("YW Jj\tZA==", "BASE64", b"abcd".to_vec()),
        ("é\\000\\377\\\\", "escape", vec![0xc3, 0xa9, 0, 255, 92]),
        ("", "hex", vec![]),
        ("", "base64", vec![]),
        ("", "escape", vec![]),
    ] {
        let row = client
            .query_one("SELECT decode($1::text,$2::text)", &[&value, &format])
            .await
            .unwrap();
        assert_eq!(*row.columns()[0].type_(), Type::BYTEA);
        assert_eq!(row.columns()[0].name(), "decode");
        assert_eq!(row.get::<_, Vec<u8>>(0), expected);
    }
    let simple = client
        .simple_query("SELECT decode('00ff','hex')")
        .await
        .unwrap();
    let SimpleQueryMessage::Row(row) = &simple[1] else {
        panic!("expected bytea row")
    };
    assert_eq!(row.get(0), Some("\\x00ff"));
    for (value, format) in [
        ("0 0", "hex"),
        ("0", "hex"),
        ("zz", "hex"),
        ("YQ", "base64"),
        ("00", "unknown"),
    ] {
        let e = client
            .query_one("SELECT decode($1::text,$2::text)", &[&value, &format])
            .await
            .unwrap_err();
        assert_eq!(e.code().unwrap().code(), "22023", "{e}");
    }
    for value in ["\\400", "\\x"] {
        assert_eq!(
            client
                .query_one("SELECT decode($1::text,'escape')", &[&value])
                .await
                .unwrap_err()
                .code()
                .unwrap()
                .code(),
            "22P02"
        );
    }
    assert!(
        client
            .query_one("SELECT decode(NULL::text,'unknown')", &[])
            .await
            .unwrap()
            .get::<_, Option<Vec<u8>>>(0)
            .is_none()
    );
    for sql in [
        "SELECT decode(123,'hex')",
        "SELECT decode('00')",
        "SELECT decode('00','hex','x')",
    ] {
        let e = client.simple_query(sql).await.unwrap_err();
        assert_eq!(e.code().unwrap().code(), "42883", "{sql}: {e}");
    }
    // Private names remain inaccessible to untrusted SQL after public rewrite support.
    assert!(
        client
            .simple_query("SELECT __fed_pg_decode('00','hex')")
            .await
            .is_err()
    );
    drop(client);
    task.await.unwrap();
    engine.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the disposable PostgreSQL fixture in scripts/test-federation.sh"]
async fn decode_matches_real_postgres_values_nulls_and_errors() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let (client, task) = connect(&engine).await;
    let (pg, connection) =
        tokio_postgres::connect(&std::env::var("FEDENGINE_POSTGRES_DSN").unwrap(), NoTls)
            .await
            .unwrap();
    let native = tokio::spawn(async { connection.await.unwrap() });
    let mut cases: Vec<(String, String)> = (0..=255)
        .flat_map(|n| {
            [
                (format!("\\{n:03o}"), "escape".into()),
                (format!("{n:02X}"), "hex".into()),
            ]
        })
        .collect();
    for value in [
        "",
        " ",
        "YQ==",
        "YR==",
        "YWJj",
        "YW Jj\r\nZA==",
        "YQ",
        "YQ=",
        "====",
        "YQ==YQ==",
        "AA=A",
        "A===",
        "AA==\u{000b}",
        "AA==\t",
    ] {
        cases.push((value.into(), "BaSe64".into()));
    }
    for value in [
        "", "0", "0 0", " 00  ff ", "\\", "\\400", "\\378", "\\01", "é", "中", "\\\\",
    ] {
        for format in ["hex", "escape", "unknown"] {
            cases.push((value.into(), format.into()));
        }
    }
    for (value, format) in &cases {
        let actual = client
            .query_one("SELECT decode($1::text,$2::text)", &[value, format])
            .await
            .map(|r| r.get::<_, Option<Vec<u8>>>(0))
            .map_err(|e| e.code().unwrap().code().to_owned());
        let expected = pg
            .query_one("SELECT decode($1::text,$2::text)", &[value, format])
            .await
            .map(|r| r.get::<_, Option<Vec<u8>>>(0))
            .map_err(|e| e.code().unwrap().code().to_owned());
        assert_eq!(actual, expected, "value={value:?}, format={format:?}");
    }
    eprintln!("decode native PostgreSQL comparisons={}", cases.len());
    drop(pg);
    native.await.unwrap();
    drop(client);
    task.await.unwrap();
    engine.shutdown_until_drained().await.unwrap();
}

#[derive(Debug, Default)]
struct WireResult {
    parameters: Vec<u32>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
    state: Option<String>,
}
async fn extended_probe(
    stream: &mut tokio::net::TcpStream,
    sql: &str,
    declared: &[u32],
    values: &[Option<&[u8]>],
    binary_result: bool,
) -> WireResult {
    let mut parse = vec![0];
    parse.extend_from_slice(sql.as_bytes());
    parse.push(0);
    parse.extend_from_slice(&(declared.len() as i16).to_be_bytes());
    for oid in declared {
        parse.extend_from_slice(&oid.to_be_bytes());
    }
    wire::packet(stream, b'P', &parse).await;
    wire::packet(stream, b'D', b"S\0").await;
    let mut bind = vec![0, 0, 0, 0]; // unnamed portal, statement, default text input
    bind.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for value in values {
        if let Some(value) = value {
            bind.extend_from_slice(&(value.len() as i32).to_be_bytes());
            bind.extend_from_slice(value);
        } else {
            bind.extend_from_slice(&(-1i32).to_be_bytes());
        }
    }
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&i16::from(binary_result).to_be_bytes());
    wire::packet(stream, b'B', &bind).await;
    wire::packet(stream, b'E', &[0, 0, 0, 0, 0]).await;
    wire::packet(stream, b'S', &[]).await;
    let mut result = WireResult::default();
    loop {
        let (tag, body) = wire::message(stream).await;
        match tag {
            b't' => {
                let count = i16::from_be_bytes(body[..2].try_into().unwrap()) as usize;
                result.parameters = body[2..]
                    .chunks_exact(4)
                    .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
                    .collect();
                assert_eq!(result.parameters.len(), count);
            }
            b'D' => {
                let count = i16::from_be_bytes(body[..2].try_into().unwrap()) as usize;
                let mut input = &body[2..];
                let mut row = Vec::new();
                for _ in 0..count {
                    let n = i32::from_be_bytes(input[..4].try_into().unwrap());
                    input = &input[4..];
                    if n < 0 {
                        row.push(None);
                    } else {
                        row.push(Some(input[..n as usize].to_vec()));
                        input = &input[n as usize..];
                    }
                }
                result.rows.push(row);
            }
            b'E' => {
                result.state = body.split(|b| *b == 0).find_map(|f| {
                    (f.first() == Some(&b'C')).then(|| String::from_utf8(f[1..].to_vec()).unwrap())
                });
            }
            b'Z' => return result,
            _ => {}
        }
    }
}
#[tokio::test]
async fn original_parameter_contract_survives_metadata_lowering() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let mut stream = wire::raw_start(engine.address().unwrap(), "fedengine").await;
    for (sql, types, values) in [
        (
            "SELECT pg_database_size($1)",
            vec![19],
            vec![Some(&b"1"[..])],
        ),
        (
            "SELECT pg_get_ruledef($1::oid)",
            vec![26],
            vec![Some(&b"1"[..])],
        ),
        (
            "SELECT pg_get_ruledef($1,$2)",
            vec![26, 16],
            vec![None, Some(&b"true"[..])],
        ),
        ("SELECT pg_get_partkeydef($1)", vec![26], vec![None]),
        (
            "SELECT pg_tablespace_location($1)",
            vec![26],
            vec![Some(&b"1"[..])],
        ),
    ] {
        let result = extended_probe(&mut stream, sql, &[], &values, false).await;
        assert_eq!(result.state, None, "{sql}: {result:?}");
        assert_eq!(result.parameters, types, "{sql}");
        assert_eq!(result.rows, vec![vec![None]], "{sql}");
        let result = extended_probe(&mut stream, sql, &types, &values, true).await;
        assert_eq!(result.state, None, "explicit OIDs {sql}: {result:?}");
        assert_eq!(result.rows, vec![vec![None]], "{sql}");
    }
    let result = extended_probe(
        &mut stream,
        "SELECT pg_database_size($1),$3::integer+$3::integer,'$2'::text",
        &[26, 25, 23],
        &[Some(b"1"), Some(b"unused"), Some(b"21")],
        false,
    )
    .await;
    assert_eq!(result.state, None, "{result:?}");
    assert_eq!(result.parameters, vec![26, 25, 23]);
    assert_eq!(
        result.rows,
        vec![vec![None, Some(b"42".to_vec()), Some(b"$2".to_vec())]]
    );
    for sql in ["SELECT 1", "SELECT pg_tablespace_location(0)"] {
        let result = extended_probe(
            &mut stream,
            sql,
            &[23, 25],
            &[Some(b"9"), Some(b"ignored")],
            false,
        )
        .await;
        assert_eq!(result.state, None, "{sql}: {result:?}");
        assert_eq!(result.parameters, vec![23, 25]);
        let result = extended_probe(
            &mut stream,
            sql,
            &[23, 25],
            &[Some(b"bad"), Some(b"ignored")],
            false,
        )
        .await;
        assert_eq!(result.state.as_deref(), Some("22P02"), "{sql}: {result:?}");
    }
    for (sql, declared, values, state) in [
        (
            "SELECT pg_get_ruledef($1)",
            vec![],
            vec![Some(&b"bad"[..])],
            "22P02",
        ),
        ("SELECT pg_database_size($1)", vec![], vec![], "08P01"),
        (
            "SELECT pg_get_ruledef($1,$2)",
            vec![],
            vec![Some(&b"1"[..]), Some(&b"bad"[..])],
            "22P02",
        ),
        (
            "SELECT pg_database_size($2)",
            vec![],
            vec![None, None],
            "42P18",
        ),
    ] {
        let result = extended_probe(&mut stream, sql, &declared, &values, false).await;
        assert_eq!(result.state.as_deref(), Some(state), "{sql}: {result:?}");
    }
    drop(stream);
    engine.shutdown_until_drained().await.unwrap();
}
#[tokio::test]
async fn metadata_result_names_survive_lowering_and_parameter_typing() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let (client, task) = connect(&engine).await;
    for (sql, declared, expected) in [
        (
            "SELECT 'pg_class'::regclass, 1::integer, 2::bigint, 1+2, 'pg_class'::regclass::text",
            vec![],
            vec!["regclass", "int4", "int8", "?column?", "regclass"],
        ),
        (
            "SELECT $1, pg_database_size($2)",
            vec![Type::REGCLASS, Type::NAME],
            vec!["?column?", "pg_database_size"],
        ),
        (
            "SELECT 'pg_class'::regclass AS \"KeptName\"",
            vec![],
            vec!["KeptName"],
        ),
        (
            "WITH x AS (SELECT 'pg_class'::regclass) SELECT x.* FROM x",
            vec![],
            vec!["regclass"],
        ),
    ] {
        let prepared = client.prepare_typed(sql, &declared).await.unwrap();
        assert_eq!(
            prepared
                .columns()
                .iter()
                .map(|c| c.name())
                .collect::<Vec<_>>(),
            expected,
            "{sql}"
        );
    }
    let rows = client
        .simple_query("SELECT 'pg_class'::regclass,1+2")
        .await
        .unwrap();
    let row = rows
        .iter()
        .find_map(|m| {
            if let SimpleQueryMessage::Row(r) = m {
                Some(r)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        row.columns().iter().map(|c| c.name()).collect::<Vec<_>>(),
        vec!["regclass", "?column?"]
    );
    drop(client);
    task.await.unwrap();
    engine.shutdown_until_drained().await.unwrap();
}
#[tokio::test]
async fn regclass_names_bind_in_scope_and_text_output_preserves_oid_binary() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let mut stream = wire::raw_start(engine.address().unwrap(), "fedengine").await;
    for name in [
        "pg_class",
        "pg_catalog.pg_class",
        "\"pg_catalog\".\"pg_class\"",
        "fedengine.pg_catalog.pg_class",
    ] {
        let result = extended_probe(
            &mut stream,
            "SELECT $1::regclass",
            &[],
            &[Some(name.as_bytes())],
            false,
        )
        .await;
        assert_eq!(result.state, None, "{name}: {result:?}");
        assert_eq!(result.parameters, vec![2205]);
        assert_eq!(result.rows, vec![vec![Some(b"pg_class".to_vec())]]);
        let result = extended_probe(
            &mut stream,
            "SELECT $1::regclass",
            &[],
            &[Some(name.as_bytes())],
            true,
        )
        .await;
        assert_eq!(result.state, None, "{name}: {result:?}");
        assert_eq!(
            result.rows,
            vec![vec![Some(1259u32.to_be_bytes().to_vec())]]
        );
        let result = extended_probe(
            &mut stream,
            "SELECT pg_relation_size($1)",
            &[],
            &[Some(name.as_bytes())],
            false,
        )
        .await;
        assert_eq!(result.state, None, "{name}: {result:?}");
        assert_eq!(result.rows, vec![vec![None]]);
    }
    for (name, state) in [
        ("missing_relation", "42P01"),
        ("pg_class ORDER BY 1", "42602"),
        ("other.pg_catalog.pg_class", "0A000"),
        ("4294967296", "22003"),
        ("+1259", "42P01"),
    ] {
        let result = extended_probe(
            &mut stream,
            "SELECT $1::regclass",
            &[],
            &[Some(name.as_bytes())],
            false,
        )
        .await;
        assert_eq!(result.state.as_deref(), Some(state), "{name}: {result:?}");
    }
    for (value, output) in [
        ("'pg_class'", "pg_class"),
        ("0", "-"),
        ("1259", "pg_class"),
        ("4294967295", "4294967295"),
    ] {
        let result = extended_probe(
            &mut stream,
            &format!("SELECT {value}::regclass"),
            &[],
            &[],
            false,
        )
        .await;
        assert_eq!(result.state, None, "{value}: {result:?}");
        assert_eq!(result.rows, vec![vec![Some(output.as_bytes().to_vec())]]);
    }
    for sql in [
        "SELECT 'pg_class'::regclass::text",
        "SELECT to_regclass($1)::text",
        "SELECT $1::regclass::text",
    ] {
        let values = if sql.contains("$1") {
            vec![Some(&b"pg_catalog.pg_class"[..])]
        } else {
            vec![]
        };
        let result = extended_probe(&mut stream, sql, &[], &values, false).await;
        assert_eq!(result.state, None, "{sql}: {result:?}");
        assert_eq!(result.rows, vec![vec![Some(b"pg_class".to_vec())]]);
    }
    let result = extended_probe(
        &mut stream,
        "SELECT $1",
        &[2205],
        &[Some(b"pg_class")],
        false,
    )
    .await;
    assert_eq!(result.state, None, "{result:?}");
    assert_eq!(result.rows, vec![vec![Some(b"pg_class".to_vec())]]);
    for sql in ["SELECT to_regclass($1)", "SELECT to_regclass($1)::text"] {
        let result = extended_probe(&mut stream, sql, &[], &[Some(b"missing")], false).await;
        assert_eq!(result.state, None, "{sql}: {result:?}");
        assert_eq!(result.rows, vec![vec![None]]);
    }
    drop(stream);
    engine.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
async fn dynamic_relation_fork_uses_text_parameters_and_strict_nulls() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let mut stream = wire::raw_start(engine.address().unwrap(), "fedengine").await;
    for fork in [
        Some(&b"main"[..]),
        Some(&b"fsm"[..]),
        Some(&b"vm"[..]),
        Some(&b"init"[..]),
        None,
    ] {
        let result = extended_probe(
            &mut stream,
            "SELECT pg_relation_size($1,$2)",
            &[],
            &[Some(b"pg_class"), fork],
            false,
        )
        .await;
        assert_eq!(result.state, None, "{fork:?}: {result:?}");
        assert_eq!(result.parameters, vec![2205, 25]);
        assert_eq!(result.rows, vec![vec![None]]);
    }
    let result = extended_probe(
        &mut stream,
        "SELECT pg_relation_size($1,lower($2))",
        &[],
        &[Some(b"pg_class"), Some(b"MAIN")],
        false,
    )
    .await;
    assert_eq!(result.state, None, "{result:?}");
    let result = extended_probe(
        &mut stream,
        "SELECT pg_relation_size($1,$2)",
        &[],
        &[Some(b"pg_class"), Some(b"bad")],
        false,
    )
    .await;
    assert_eq!(result.state.as_deref(), Some("22023"), "{result:?}");
    for oid in [None, Some(&b"0"[..])] {
        let result = extended_probe(
            &mut stream,
            "SELECT pg_relation_size($1,$2)",
            &[],
            &[oid, Some(b"bad")],
            false,
        )
        .await;
        assert_eq!(result.state, None, "{result:?}");
        assert_eq!(result.rows, vec![vec![None]]);
    }
    drop(stream);
    engine.shutdown_until_drained().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the disposable PostgreSQL fixture in scripts/test-federation.sh"]
async fn metadata_parameters_and_regclass_match_native_postgres() {
    let temp = common::TestDirectory::new();
    let engine = FedEngine::start(configuration(&temp)).await.unwrap();
    let (client, task) = connect(&engine).await;
    let (native, connection) =
        tokio_postgres::connect(&std::env::var("FEDENGINE_POSTGRES_DSN").unwrap(), NoTls)
            .await
            .unwrap();
    let native_task = tokio::spawn(async move { connection.await.unwrap() });
    for (sql, declared) in [
        ("SELECT pg_database_size($1)", vec![]),
        ("SELECT pg_database_size($1)", vec![Type::OID]),
        ("SELECT pg_get_ruledef($1,$2)", vec![]),
        ("SELECT pg_get_ruledef($1::oid)", vec![]),
        ("SELECT pg_get_partkeydef($1)", vec![]),
        ("SELECT pg_tablespace_location($1)", vec![]),
        ("SELECT pg_relation_size($1,$2)", vec![]),
        ("SELECT pg_get_viewdef($1)", vec![]),
        ("SELECT pg_get_viewdef($1,$2)", vec![]),
        ("SELECT pg_get_viewdef($1,$2)", vec![Type::OID, Type::BOOL]),
        ("SELECT 1", vec![Type::INT4, Type::TEXT]),
        (
            "SELECT pg_get_ruledef($1),$3::integer",
            vec![Type::OID, Type::TEXT, Type::INT4],
        ),
        ("SELECT pg_get_ruledef($2)", vec![]),
    ] {
        let actual = client
            .prepare_typed(sql, &declared)
            .await
            .map(|s| s.params().iter().map(Type::oid).collect::<Vec<_>>())
            .map_err(|e| e.code().unwrap().code().to_owned());
        let expected = native
            .prepare_typed(sql, &declared)
            .await
            .map(|s| s.params().iter().map(Type::oid).collect::<Vec<_>>())
            .map_err(|e| e.code().unwrap().code().to_owned());
        assert_eq!(actual, expected, "{sql} {declared:?}");
    }
    for sql in [
        "SELECT 'pg_class'::regclass::text",
        "SELECT 'pg_catalog.pg_class'::regclass::text",
        "SELECT 0::regclass::text",
        "SELECT 4294967295::regclass::text",
        "SELECT to_regclass('missing_relation')::text",
        "SELECT to_regclass('pg_class ORDER BY 1')::text",
        "SELECT pg_relation_size(0,'bad')",
        "SELECT pg_relation_size(NULL,'bad')",
        "SELECT pg_relation_size('pg_class','bad')",
        "SELECT to_regclass('4294967296')::text",
        "SELECT '+1259'::regclass::text",
        "SELECT '4294967296'::regclass::text",
    ] {
        let execute = |messages: Vec<SimpleQueryMessage>| {
            messages
                .into_iter()
                .filter_map(|m| match m {
                    SimpleQueryMessage::Row(row) => Some(row.get(0).map(str::to_owned)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let actual = client
            .simple_query(sql)
            .await
            .map(execute)
            .map_err(|e| e.code().unwrap().code().to_owned());
        let expected = native
            .simple_query(sql)
            .await
            .map(execute)
            .map_err(|e| e.code().unwrap().code().to_owned());
        assert_eq!(actual, expected, "{sql}");
    }
    drop(native);
    native_task.await.unwrap();
    drop(client);
    task.await.unwrap();
    engine.shutdown_until_drained().await.unwrap();
}
