use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Own only a directory created by this test, and remove it on scope exit.
pub struct TestDirectory(PathBuf);

impl TestDirectory {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        loop {
            let path = std::env::temp_dir().join(format!(
                "fedengine-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test directory: {error}"),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("cannot remove test directory {}: {error}", self.0.display());
        }
    }
}
