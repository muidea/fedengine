use fedengine_core::*;
use sha2::{Digest, Sha256};
use std::io::Read;

pub(super) fn provisioned(directory: &str, logical: &str) -> Result<bool> {
    Ok(read_manifest(std::path::Path::new(directory))?["extensions"][logical].is_object())
}

fn read_manifest(root: &std::path::Path) -> Result<serde_json::Value> {
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
    Ok(manifest)
}

pub(super) fn verify_extension(directory: &str, logical: &str) -> Result<()> {
    let root = std::fs::canonicalize(directory)
        .map_err(|_| EngineError::config("extension directory is unavailable"))?;
    let manifest = read_manifest(&root)?;
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
    let mut file =
        std::fs::File::open(path).map_err(|_| EngineError::config("cannot read extension"))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| EngineError::config("cannot read extension"))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if hash
        .finalize()
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provisioned_files_are_verified_across_buffer_boundaries() {
        let root =
            std::env::temp_dir().join(format!("fedengine-extension-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("postgres_scanner.duckdb_extension");
        let data = vec![42_u8; 2 * 64 * 1024 + 1];
        std::fs::write(&path, &data).unwrap();
        let digest: String = Sha256::digest(&data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "duckdb_version":"v2.0.0", "extensions":{"postgres":{
                    "path":"postgres_scanner.duckdb_extension", "sha256":digest
                }}
            }))
            .unwrap(),
        )
        .unwrap();
        let directory = root.to_str().unwrap();
        assert!(provisioned(directory, "postgres").unwrap());
        assert!(!provisioned(directory, "mysql").unwrap());
        verify_extension(directory, "postgres").unwrap();
        std::fs::write(&path, &data[..data.len() - 1]).unwrap();
        assert!(verify_extension(directory, "postgres").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
