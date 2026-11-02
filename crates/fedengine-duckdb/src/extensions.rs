use fedengine_core::*;
use sha2::{Digest, Sha256};
pub(super) fn verify_extension(directory: &str, logical: &str) -> Result<()> {
    let root = std::fs::canonicalize(directory)
        .map_err(|_| EngineError::config("extension directory is unavailable"))?;
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("manifest.json"))
            .map_err(|_| EngineError::config("extension manifest is missing"))?,
    )
    .map_err(|_| EngineError::config("extension manifest is invalid"))?;
    if manifest["duckdb_version"] != "v2.0.0" {
        return Err(EngineError::config(
            "extension manifest core version mismatch",
        ));
    }
    let relative = manifest["extensions"][logical]["path"]
        .as_str()
        .ok_or_else(|| EngineError::config("extension manifest entry missing"))?;
    let path = std::fs::canonicalize(root.join(relative))
        .map_err(|_| EngineError::config("extension file unavailable"))?;
    if !path.starts_with(&root)
        || path.file_name().and_then(|n| n.to_str())
            != Some(&format!("{logical}_scanner.duckdb_extension"))
    {
        return Err(EngineError::config("extension manifest path is invalid"));
    }
    let data = std::fs::read(path).map_err(|_| EngineError::config("cannot read extension"))?;
    if Sha256::digest(&data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        != manifest["extensions"][logical]["sha256"]
            .as_str()
            .unwrap_or("")
    {
        return Err(EngineError::config("extension SHA256 mismatch"));
    }
    Ok(())
}
