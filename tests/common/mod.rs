//! Shared helpers for the integration test suites.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch database directory that is unique per call and cleaned up on drop.
pub struct TempDb {
    pub path: PathBuf,
}

impl TempDb {
    /// Creates a unique temporary directory path (not yet created on disk).
    pub fn new(label: &str) -> TempDb {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("cairn-it-{}-{}-{}", std::process::id(), label, n));
        let _ = std::fs::remove_dir_all(&path);
        TempDb { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
