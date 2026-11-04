//! The virtual server accepts only authorized, published source catalogs.
mod common;
use fedengine_app::FedEngine;
use fedengine_contract::CatalogHeader;
use fedengine_core::*;
use fedengine_policy::metadata_scope;
use tokio_postgres::NoTls;

fn fixture() -> (Config, CatalogHeader, DataSourceDefinition) {
    let config = Config::load("examples/config/federation.toml").unwrap();
    let definition = config.datasources[0].clone();
    let incarnation = "fixture".to_string();
    let header = CatalogHeader {
        source: ManagedSource {
            id: definition.id.clone(),
            incarnation: incarnation.clone(),
            kind: definition.kind,
            catalog_alias: definition.catalog_alias.clone(),
            desired_revision: 1,
            applied_revision: Some(1),
            desired_enabled: true,
            status: SourceStatus::Active,
            generation: 1,
            deleted: false,
            current_operation: None,
            last_error: None,
            catalog_snapshot: Some("snapshot".into()),
        },
        applied: None,
        snapshot: Some(CatalogSnapshot {
            id: "snapshot".into(),
            incarnation,
            revision: 1,
            generation: 1,
            refreshed_at: "fixture".into(),
            complete: true,
            stale: false,
            structure_digest: "fixture".into(),
            database_oid: 200000000,
            schema_oids: [("public".into(), 200000001)].into_iter().collect(),
            schemas: vec!["public".into()],
            objects: vec![],
        }),
    };
    (config, header, definition)
}
#[test]
fn only_authorized_published_catalogs_are_connectable() {
    let (config, header, definition) = fixture();
    let principal = &config.users[0].name;
    let database = &definition.catalog_alias;
    let scope = metadata_scope(
        &config,
        principal,
        database,
        vec![header.clone()],
        std::slice::from_ref(&definition),
    )
    .unwrap();
    assert_eq!(scope.catalogs.len(), 1);
    for name in ["fedengine", "postgres", "template0", "template1", "unknown"] {
        assert_eq!(
            metadata_scope(
                &config,
                principal,
                name,
                vec![header.clone()],
                std::slice::from_ref(&definition)
            )
            .unwrap_err()
            .sqlstate,
            "3D000"
        );
    }
    for status in [
        SourceStatus::Registered,
        SourceStatus::Disabled,
        SourceStatus::Degraded,
    ] {
        let mut inactive = header.clone();
        inactive.source.status = status;
        assert_eq!(
            metadata_scope(
                &config,
                principal,
                database,
                vec![inactive],
                std::slice::from_ref(&definition)
            )
            .unwrap_err()
            .sqlstate,
            "55000"
        );
    }
    let mut stale = header.clone();
    stale.source.status = SourceStatus::MetadataStale;
    assert!(
        metadata_scope(
            &config,
            principal,
            database,
            vec![stale],
            std::slice::from_ref(&definition)
        )
        .unwrap()
        .catalogs[0]
            .stale
    );
    let mut denied = config.clone();
    denied.grants.clear();
    assert_eq!(
        metadata_scope(
            &denied,
            principal,
            database,
            vec![header],
            std::slice::from_ref(&definition)
        )
        .unwrap_err()
        .sqlstate,
        "42501"
    );
}
#[test]
fn fedengine_name_is_ordinary_when_it_is_an_actual_source_alias() {
    let (mut config, mut header, mut definition) = fixture();
    definition.catalog_alias = "fedengine".into();
    header.source.catalog_alias = "fedengine".into();
    config.datasources[0] = definition.clone();
    config.validate().unwrap();
    assert_eq!(
        metadata_scope(
            &config,
            &config.users[0].name,
            "fedengine",
            vec![header],
            &[definition]
        )
        .unwrap()
        .catalogs
        .len(),
        1
    );
}
#[tokio::test]
async fn empty_server_has_no_synthetic_database_or_username_fallback_catalog() {
    let temporary = common::TestDirectory::new();
    let mut config = Config::load("config/application.toml").unwrap();
    config.management.listen = "127.0.0.1:0".into();
    config.pgwire.listen = "127.0.0.1:0".into();
    config.duckdb.database_path = temporary
        .path()
        .join("management.duckdb")
        .to_string_lossy()
        .into_owned();
    config.duckdb.temp_directory = temporary.path().join("tmp").to_string_lossy().into_owned();
    config.duckdb.threads = 1;
    config.users[0].password_secret_ref = "USER".into();
    let engine = FedEngine::start(config).await.unwrap();
    assert!(engine.management_address().is_some());
    for database in [Some("fedengine"), Some("postgres"), Some("template1"), None] {
        let mut c = tokio_postgres::Config::new();
        c.host("127.0.0.1")
            .port(engine.address().unwrap().port())
            .user("fedengine")
            .password(std::env::var("USER").unwrap());
        if let Some(database) = database {
            c.dbname(database);
        }
        let error = match c.connect(NoTls).await {
            Ok(_) => panic!("invented database accepted"),
            Err(e) => e,
        };
        assert_eq!(error.code().unwrap().code(), "3D000");
    }
    engine.shutdown().await.unwrap();
}

#[test]
fn user_database_connect_and_mapped_objects_have_independent_authorization() {
    use fedengine_contract::CatalogHeads;
    let (mut config, header, definition) = fixture();
    let db = VirtualDatabase {
        revision: 1,
        config: VirtualDatabaseConfig {
            name: "fedengine".into(),
            owner: "fedengine".into(),
            connect: vec![],
            allow_connections: true,
            schemas: vec![],
        },
        oid: 210000000,
        public_oid: 210000001,
        schemas: vec![VirtualSchema {
            config: VirtualSchemaConfig {
                name: "business".into(),
                source: definition.id.clone(),
                remote_schema: "public".into(),
            },
            oid: 210000002,
            snapshot_id: Some("mapped".into()),
        }],
    };
    let heads = CatalogHeads {
        sources: vec![header.clone()],
        databases: vec![db.clone()],
    };
    let scope = metadata_scope(
        &config,
        "fedengine",
        "fedengine",
        heads.clone(),
        std::slice::from_ref(&definition),
    )
    .unwrap();
    let mapped = scope
        .catalogs
        .iter()
        .find(|c| c.catalog == "fedengine")
        .unwrap();
    assert_eq!(mapped.schemas, ["business"]);
    assert!(mapped.grants.iter().all(|g| g.schema == "business"));
    config.grants.clear();
    let empty = metadata_scope(
        &config,
        "fedengine",
        "fedengine",
        heads.clone(),
        std::slice::from_ref(&definition),
    )
    .unwrap();
    assert!(empty.catalogs.is_empty()); // Owner/CONNECT is not SELECT.
    let mut disabled = heads;
    disabled.sources[0].source.status = SourceStatus::Disabled;
    assert!(
        metadata_scope(
            &config,
            "fedengine",
            "fedengine",
            disabled,
            std::slice::from_ref(&definition)
        )
        .unwrap()
        .catalogs
        .is_empty()
    );
    let heads = CatalogHeads {
        sources: vec![],
        databases: vec![db],
    };
    assert!(metadata_scope(&config, "fedengine", "fedengine", heads, &[]).is_ok()); // Empty business database is connectable.
}
