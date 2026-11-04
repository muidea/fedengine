use fedengine_core::EngineError;
use modorch::FrameworkError;
pub(super) fn fw(e: EngineError) -> FrameworkError {
    FrameworkError::InvalidConfig(e.to_string())
}
pub(super) fn engine_error(e: duckdb::Error) -> EngineError {
    // DuckDB errors can embed SQL, ATTACH credentials or remote diagnostics. Never expose them verbatim.
    let text = e.to_string().to_lowercase();
    let (code, message) = if text.starts_with("invalid input error: __fed_pg_division_by_zero") {
        ("22012", "division by zero")
    } else if text.starts_with("invalid input error: __fed_pg_relation_missing") {
        (
            "42P01",
            "referenced relation does not exist in the visible database",
        )
    } else if text.starts_with("invalid input error: __fed_pg_oid_range") {
        ("22003", "value is out of range for type oid")
    } else if text.starts_with("invalid input error: __fed_pg_invalid_fork") {
        ("22023", "invalid fork name")
    } else if text.starts_with("invalid input error: __fed_pg_relation_name") {
        ("42602", "invalid relation identifier")
    } else if text.starts_with("invalid input error: __fed_pg_cross_database") {
        ("0A000", "cross-database references are not implemented")
    } else if text.starts_with(
        "binder error: no function matches the given name and argument types '__fed_pg_decode",
    ) {
        (
            "42883",
            "function decode(text, text) does not match the supplied arguments",
        )
    } else if text.starts_with("invalid input error: __fed_pg_decode_escape") {
        ("22P02", "invalid input syntax for type bytea")
    } else if text.starts_with("invalid input error: __fed_pg_decode") {
        ("22023", "invalid data or encoding name for decode")
    } else if text.starts_with("invalid input error: __fed_pg_numeric_division") {
        (
            "0A000",
            "exact PostgreSQL numeric division is not implemented",
        )
    } else if text.contains("conversion error") && text.contains("out of range")
        || text.contains("out of range error")
    {
        ("22003", "numeric value out of range")
    } else if text.contains("conversion error") && text.contains("could not convert string") {
        ("22P02", "invalid input syntax for the requested type")
    } else if text.contains("interrupt") {
        ("57014", "query canceled")
    } else if text.contains("permission denied")
        || text.contains("access denied")
        || text.contains("command denied")
    {
        ("42501", "source denied the requested metadata or operation")
    } else if text.contains("parser error") {
        ("42601", "DuckDB rejected SQL syntax")
    } else if text.contains("does not exist") {
        ("42P01", "referenced object does not exist")
    } else if text.contains("constraint") {
        ("23000", "remote or DuckDB constraint violation")
    } else if text.contains("memory") {
        ("53200", "DuckDB memory limit exceeded")
    } else if text.contains("not implemented") || text.contains("not supported") {
        (
            "0A000",
            "operation not supported by the locked engine/extension",
        )
    } else {
        (
            "XX000",
            "DuckDB execution failed; inspect the source state before retrying writes",
        )
    };
    EngineError::new(code, message)
}
pub(super) fn framework_error(e: FrameworkError) -> EngineError {
    EngineError::new("53300", format!("execution admission failed: {e}"))
}

/// MySQL 8 information_schema strings can arrive with a binary collation.
/// Decode UTF-8 bytes explicitly instead of DuckDB's escaped BLOB-to-VARCHAR cast.
pub(super) fn metadata_text(row: &duckdb::Row<'_>, index: usize) -> fedengine_core::Result<String> {
    use duckdb::types::ValueRef;
    match row.get_ref(index).map_err(engine_error)? {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => String::from_utf8(bytes.to_vec())
            .map_err(|_| EngineError::new("22021", "metadata text is not valid UTF-8")),
        _ => Err(EngineError::new(
            "55000",
            "metadata text has an unexpected source type",
        )),
    }
}
