use fedengine_core::EngineError;
use modorch::FrameworkError;
pub(super) fn fw(e: EngineError) -> FrameworkError {
    FrameworkError::InvalidConfig(e.to_string())
}
pub(super) fn engine_error(e: duckdb::Error) -> EngineError {
    // DuckDB errors can embed SQL, ATTACH credentials or remote diagnostics. Never expose them verbatim.
    let text = e.to_string().to_lowercase();
    let (code, message) = if text.contains("interrupt") {
        ("57014", "query canceled")
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
