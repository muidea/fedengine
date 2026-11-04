use fedengine_core::*;
fn configuration() -> Config {
    let config: Config =
        toml::from_str(include_str!("../../../examples/config/federation.toml")).unwrap();
    config.validate().unwrap();
    config
}
#[test]
fn configuration_rejects_zero_bounds_and_duplicate_aliases() {
    let mut c = configuration();
    c.duckdb.max_workers = 0;
    assert!(c.validate().is_err());
    c.duckdb.max_workers = 2;
    c.datasources[1].catalog_alias = c.datasources[0].catalog_alias.clone();
    assert!(c.validate().is_err());
    c.datasources[1].catalog_alias = "mysql_erp".into();
    c.datasources[0].catalog_alias = "MEMORY".into();
    assert!(c.validate().is_err());
    c.datasources[0].catalog_alias = "pg_crm".into();
    c.datasources[0].allowed_schemas.push("PG_CATALOG".into());
    assert!(c.validate().is_err());
}

#[test]
fn management_requires_persistence_and_limits_credential_references() {
    let mut c = configuration();
    c.management.enabled = true;
    c.duckdb.database_path = ":memory:".into();
    assert!(c.validate().is_err());
    c.duckdb.database_path = "./data/test.duckdb".into();
    c.datasources[0].secret_ref = "file:postgres/connection.dsn".into();
    assert!(c.validate().is_ok());
    for reference in [
        "file:../outside",
        "file:/etc/passwd",
        "file:",
        "password=plaintext",
    ] {
        c.datasources[0].secret_ref = reference.into();
        assert!(c.validate().is_err());
    }
    c.datasources[0].secret_ref = "FEDENGINE_POSTGRES_DSN".into();
    c.management.enabled = false;
    c.duckdb.database_path = ":memory:".into();
    assert!(c.validate().is_ok());
}

#[test]
fn virtual_databases_share_strict_names_principals_and_binding_limits() {
    let mut c = configuration();
    c.management.enabled = true;
    c.duckdb.database_path = "./data/config-test.duckdb".into();
    let definition = VirtualDatabaseConfig {
        name: "account".into(),
        owner: c.users[0].name.clone(),
        connect: vec![],
        allow_connections: true,
        schemas: vec![],
    };
    for name in [
        "pg_catalog",
        "information_schema",
        "_fedengine",
        "PG_TEMP_1",
        "memory",
        "main",
        "system",
    ] {
        c.virtual_databases = vec![VirtualDatabaseConfig {
            name: name.into(),
            ..definition.clone()
        }];
        assert!(c.validate().is_err(), "{name}");
    }
    c.virtual_databases = vec![definition.clone()];
    assert!(c.validate().is_ok());
    c.virtual_databases[0].schemas = vec![VirtualSchemaConfig {
        name: "business".into(),
        source: c.datasources[0].id.clone(),
        remote_schema: "public".into(),
    }];
    assert!(c.validate().is_ok());
    let binding = c.virtual_databases[0].schemas[0].clone();
    c.virtual_databases[0].schemas.push(binding);
    assert!(c.validate().is_err());
    c.virtual_databases = vec![VirtualDatabaseConfig {
        connect: vec!["unknown".into()],
        ..definition
    }];
    assert!(c.validate().is_err());
}
