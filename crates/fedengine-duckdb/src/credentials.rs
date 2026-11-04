use fedengine_core::*;
use std::path::Path;

pub(super) fn resolve(reference: &str, directory: &str) -> Result<String> {
    if !valid_secret_ref(reference) {
        return Err(EngineError::config("invalid credential reference"));
    }
    let value = if let Some(relative) = reference.strip_prefix("file:") {
        let base = Path::new(directory)
            .canonicalize()
            .map_err(|_| EngineError::config("credential directory unavailable"))?;
        let path = base
            .join(relative)
            .canonicalize()
            .map_err(|_| EngineError::config("credential file unavailable"))?;
        if !path.starts_with(&base) || !path.is_file() {
            return Err(EngineError::config(
                "credential file must remain within the configured directory",
            ));
        }
        let metadata = std::fs::metadata(&path)
            .map_err(|_| EngineError::config("credential file unavailable"))?;
        if metadata.len() > 65_536 {
            return Err(EngineError::config("credential file exceeds size limit"));
        }
        std::fs::read_to_string(path)
            .map_err(|_| EngineError::config("credential file unavailable"))?
    } else {
        std::env::var(reference)
            .map_err(|_| EngineError::config("data source credential reference is missing"))?
    };
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(EngineError::config(
            "data source credential reference is empty",
        ));
    }
    Ok(value)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn file_references_are_reloaded_and_cannot_escape_through_symlinks() {
        let root =
            std::env::temp_dir().join(format!("fedengine-credentials-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let directory = root.join("secrets");
        std::fs::create_dir(&directory).unwrap();
        let reference = directory.join("connection");
        std::fs::write(&reference, "first\n").unwrap();
        assert_eq!(
            resolve("file:connection", directory.to_str().unwrap()).unwrap(),
            "first"
        );
        std::fs::write(&reference, "second").unwrap();
        assert_eq!(
            resolve("file:connection", directory.to_str().unwrap()).unwrap(),
            "second"
        );
        std::fs::write(root.join("outside"), "must-not-read").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), directory.join("escape")).unwrap();
        assert!(resolve("file:escape", directory.to_str().unwrap()).is_err());
        assert!(resolve("file:../outside", directory.to_str().unwrap()).is_err());
    }
}
