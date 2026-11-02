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
