//! Rewriting a copied tree in place so store hash differences disappear.

use std::borrow::Cow;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::prelude::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result};
use filetime::FileTime;
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use walkdir::WalkDir;

use crate::elf;
use crate::rewrite::rewrite;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Zero the GNU build ID of every ELF file
    pub strip_build_id: bool,
    /// Only print what would be rewritten
    pub list_only: bool,
}

/// Rewrite every symlink target and file below `root` (or `root` itself).
/// A symlink given as `root` is followed.
pub fn normalize(root: &Path, opts: Options) -> Result<()> {
    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry?;
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            normalize_symlink(entry.path(), opts)?;
        } else if file_type.is_file() {
            normalize_file(entry.path(), opts)?;
        }
    }
    Ok(())
}

fn normalize_symlink(path: &Path, opts: Options) -> Result<()> {
    let target = fs::read_link(path).with_context(|| format!("reading link {}", path.display()))?;
    let Cow::Owned(new) = rewrite(target.as_os_str().as_bytes()) else {
        return Ok(());
    };
    if opts.list_only {
        println!("  symlink {}", path.display());
        return Ok(());
    }

    let new = std::ffi::OsStr::from_bytes(&new);
    let link_meta = fs::symlink_metadata(path)?;
    let dir = path
        .parent()
        .context("symlink without a parent directory")?;
    let dir_meta = fs::metadata(dir)?;
    // store dirs are 0555; make the parent writable while we swap the link
    set_mode(dir, dir_meta.permissions().mode() | 0o200)?;
    fs::remove_file(path).with_context(|| format!("unlink {}", path.display()))?;
    symlink(new, path).with_context(|| format!("symlink {}", path.display()))?;
    filetime::set_symlink_file_times(
        path,
        FileTime::from_last_access_time(&link_meta),
        FileTime::from_last_modification_time(&link_meta),
    )?;
    fs::set_permissions(dir, dir_meta.permissions())?;
    filetime::set_file_times(
        dir,
        FileTime::from_last_access_time(&dir_meta),
        FileTime::from_last_modification_time(&dir_meta),
    )?;
    Ok(())
}

fn normalize_file(path: &Path, opts: Options) -> Result<()> {
    let meta = fs::metadata(path)?;
    let content = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let Some(new) = rewrite_contents(&content, opts.strip_build_id)
        .with_context(|| format!("rewriting {}", path.display()))?
    else {
        return Ok(());
    };
    if opts.list_only {
        println!("  file    {} ({})", path.display(), new.reasons.join(", "));
        return Ok(());
    }

    set_mode(path, meta.permissions().mode() | 0o200)?;
    fs::write(path, &new.data).with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, meta.permissions())?;
    filetime::set_file_times(
        path,
        FileTime::from_last_access_time(&meta),
        FileTime::from_last_modification_time(&meta),
    )?;
    Ok(())
}

struct Rewritten {
    data: Vec<u8>,
    reasons: Vec<&'static str>,
}

/// `None` when the file needs no changes.
fn rewrite_contents(content: &[u8], strip_build_id: bool) -> Result<Option<Rewritten>> {
    // gzip (e.g. man pages): the hashes are only visible after decompressing,
    // and a different hash changes the compressed bytes and size, so rewrite
    // the decompressed data and recompress it the same way on both sides.
    // Untouched unless it has a store hash.
    if content.starts_with(&[0x1f, 0x8b]) {
        let mut plain = Vec::new();
        if MultiGzDecoder::new(content).read_to_end(&mut plain).is_ok() {
            let Cow::Owned(fixed) = rewrite(&plain) else {
                return Ok(None);
            };
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&fixed)?;
            return Ok(Some(Rewritten {
                data: encoder.finish()?,
                reasons: vec!["store hashes in gzip contents"],
            }));
        }
    }

    let mut data = rewrite(content);
    let mut reasons = Vec::new();
    if matches!(data, Cow::Owned(_)) {
        reasons.push("store hashes");
    }
    if strip_build_id {
        let ids = elf::nonzero_build_ids(&data);
        if !ids.is_empty() {
            reasons.insert(0, "build-id");
            let bytes = data.to_mut();
            for range in ids {
                bytes[range].fill(0);
            }
        }
    }
    Ok(match data {
        Cow::Owned(data) => Some(Rewritten { data, reasons }),
        Cow::Borrowed(_) => None,
    })
}

pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {}", path.display()))
}
