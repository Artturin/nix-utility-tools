//! The temporary directory holding the rewritten copies. Store paths are
//! read-only, so the copies have to be made writable before they can be removed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Result;
use tempfile::TempDir;
use walkdir::WalkDir;

pub struct Scratch(TempDir);

impl Scratch {
    /// Cleanup happens on drop, so an interrupted run has to unwind (the
    /// binary turns Ctrl-C into a cancellation) rather than exit.
    pub fn new() -> Result<Self> {
        Ok(Self(TempDir::new()?))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        make_writable(self.0.path());
    }
}

fn make_writable(root: &Path) {
    for entry in WalkDir::new(root).into_iter().flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        let extra = if meta.is_dir() { 0o700 } else { 0o200 };
        let _ = fs::set_permissions(
            entry.path(),
            fs::Permissions::from_mode(meta.permissions().mode() | extra),
        );
    }
}
