use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

use crate::storage::Storage;

pub(crate) struct Fixture {
    _directory: TempDir,
    path: std::path::PathBuf,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worklight.db");
        Self {
            _directory: directory,
            path,
        }
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn storage(&self) -> Storage {
        Storage::open(&self.path).unwrap()
    }
    pub(crate) fn readonly(&self) -> Storage {
        Storage::open_read_only(&self.path).unwrap()
    }
}
pub(crate) fn at(millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(millis)
}
