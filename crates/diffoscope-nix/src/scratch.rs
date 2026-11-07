//! The temporary directory holding the rewritten copies. Store paths are
//! read-only, so the copies have to be made writable before they can be removed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tempfile::TempDir;
use walkdir::WalkDir;

pub struct Scratch(TempDir);

impl Scratch {
    pub fn new() -> Result<Self> {
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        // Interrupted while normalizing or diffing: the guard below never
        // runs, so clean up here.
        ctrlc::set_handler(move || {
            remove_tree(&path);
            std::process::exit(130);
        })?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> PathBuf {
        self.0.path().to_owned()
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

fn remove_tree(root: &Path) {
    make_writable(root);
    let _ = fs::remove_dir_all(root);
}
