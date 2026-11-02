use fedengine_core::*;
use fedengine_ddl::*;
fn configuration() -> Config {
    let config: Config =
        toml::from_str(include_str!("../../../examples/config/federation.toml")).unwrap();
    config.validate().unwrap();
    config
}
fn analyzer() -> Analyzer {
    Analyzer {
        sources: configuration().datasources,
        indexes: vec![],
    }
}
#[test]
fn real_parser_splits_strings_comments_and_dollar_quotes() {
    assert_eq!(
        Analyzer::split("SELECT ';'; -- ;\n SELECT $$x;y$$;")
            .unwrap()
            .len(),
        2
    );
    assert!(Analyzer::split("SELECT 1; INSERT INTO pg_crm.public.t VALUES (1)").is_err());
    assert!(Analyzer::split("CREATE TABLE pg_crm.public.t(i INT); SELECT 1").is_err());
}

#[test]
fn cross_source_writes_and_dependencies_are_rejected() {
    let a = analyzer();
    for sql in [
        "INSERT INTO pg_crm.public.t SELECT * FROM mysql_erp.erp.t",
        "UPDATE pg_crm.public.t SET i=(SELECT max(i) FROM mysql_erp.erp.t)",
        "DELETE FROM pg_crm.public.t WHERE i IN (SELECT i FROM mysql_erp.erp.t)",
        "CREATE TABLE pg_crm.public.t AS SELECT * FROM mysql_erp.erp.t",
        "CREATE TABLE pg_crm.public.t (i INT REFERENCES mysql_erp.erp.t(i))",
        "CREATE TABLE pg_crm.public.t (i INT, FOREIGN KEY(i) REFERENCES mysql_erp.erp.t(i))",
        "CREATE INDEX mysql_erp.erp.x ON pg_crm.public.t(i)",
    ] {
        assert!(a.analyze(sql).is_err(), "must reject {sql}");
    }
}

#[test]
fn same_source_crud_ddl_and_federated_reads_bind_registered_identities() {
    let a = analyzer();
    for sql in [
        "SELECT * FROM pg_crm.public.t JOIN mysql_erp.erp.t USING(i)",
        "INSERT INTO pg_crm.public.t VALUES(1)",
        "UPDATE pg_crm.public.t SET i=2 WHERE i=1",
        "DELETE FROM mysql_erp.t WHERE i=1",
        "CREATE TABLE pg_crm.public.t(i INT)",
        "CREATE SCHEMA pg_crm.sales",
        "CREATE TABLE pg_crm.public.t AS SELECT * FROM pg_crm.public.s",
        "CREATE TABLE pg_crm.public.t(i INT REFERENCES pg_crm.public.s(i))",
        "CREATE INDEX x ON pg_crm.public.t(i)",
        "ALTER TABLE pg_crm.public.t ADD COLUMN s VARCHAR",
        "DROP TABLE pg_crm.public.t",
    ] {
        a.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        a.analyze("SELECT * FROM mysql_erp.t")
            .unwrap()
            .objects
            .iter()
            .next()
            .unwrap()
            .source,
        DataSourceId("erp".into())
    );
}

#[test]
fn reject_engine_administration_and_side_effect_functions_at_any_depth() {
    let a = analyzer();
    for sql in [
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "ATTACH ':memory:' AS evil",
        "COPY pg_crm.public.t TO '/tmp/leak'",
        "SELECT * FROM read_csv('/etc/passwd')",
        "SELECT postgres_execute('pg_crm','DROP TABLE t')",
        "WITH x AS (SELECT mysql_execute('mysql_erp', 'DROP TABLE t')) SELECT * FROM x",
        "SELECT * FROM pg_catalog.pg_authid",
        "SELECT * FROM unknown.public.t",
        "CREATE DATABASE x",
        "CREATE TEMP TABLE pg_crm.public.t(i INT)",
        "DROP TABLE pg_crm.public.t CASCADE",
        "CREATE TABLE pg_crm.public.t(i INT) LOCATION '/tmp/x'",
    ] {
        assert!(a.analyze(sql).is_err(), "must reject {sql}");
    }
}

#[test]
fn ambiguous_defaults_and_mysql_namespaces_fail_closed() {
    let a = analyzer();
    assert!(a.analyze("SELECT * FROM t").is_err());
    assert!(a.analyze("CREATE SCHEMA mysql_erp.unauthorized").is_err());
    let sql =
        "WITH x AS (SELECT * FROM pg_crm.public.t) SELECT * FROM x JOIN mysql_erp.erp.t USING(i)";
    assert_eq!(a.analyze(sql).unwrap().sources.len(), 2);
}

#[test]
fn index_drop_requires_unique_real_table_ownership() {
    let mut a = analyzer();
    assert!(a.analyze("DROP INDEX pg_crm.public.x").is_err());
    let table = ObjectRef {
        source: DataSourceId("crm".into()),
        catalog: "pg_crm".into(),
        schema: "public".into(),
        name: "t".into(),
    };
    let index = ObjectRef {
        name: "x".into(),
        ..table.clone()
    };
    let owner = IndexOwner {
        index,
        table,
        protected: false,
    };
    a.indexes.push(owner.clone());
    assert!(a.analyze("DROP INDEX pg_crm.public.x").is_ok());
    a.indexes.push(owner);
    assert!(a.analyze("DROP INDEX pg_crm.public.x").is_err());
    a.indexes.pop();
    a.indexes[0].protected = true;
    assert!(a.analyze("DROP INDEX pg_crm.public.x").is_err());
}

#[test]
fn equal_physical_hosts_do_not_merge_datasource_ids() {
    let mut a = analyzer();
    let mut second = a.sources[0].clone();
    second.id = DataSourceId("crm2".into());
    second.catalog_alias = "pg_crm2".into();
    a.sources.push(second);
    assert!(
        a.analyze("CREATE TABLE pg_crm.public.t AS SELECT * FROM pg_crm2.public.t")
            .is_err()
    );
}

#[test]
fn custom_types_nested_writes_and_fake_builtin_tables_are_rejected() {
    let a = analyzer();
    for sql in [
        "CREATE TABLE pg_crm.public.t(i unknown.public.secret)",
        "ALTER TABLE pg_crm.public.t ALTER COLUMN i ADD GENERATED ALWAYS AS IDENTITY",
        "WITH x AS (DELETE FROM pg_crm.public.t) SELECT * FROM x",
        "SELECT * FROM range",
    ] {
        assert!(a.analyze(sql).is_err(), "must reject {sql}");
    }
    assert!(
        a.analyze("CREATE TABLE pg_crm.public.t(i INTEGER PRIMARY KEY)")
            .is_ok()
    );
}
