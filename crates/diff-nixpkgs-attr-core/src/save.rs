//! Saving differences to disk, without colour:
//!
//! ```text
//! DIR/ATTR/difflog              the build log diff
//! DIR/ATTR/diffoscope/OUTPUT    the diffoscope-nix output
//! ```

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::attr::path_component;
use crate::text::SgrStripper;

/// Where the build log diff of `attr` is saved under `dir`.
pub fn difflog_path(dir: &Path, attr: &str) -> PathBuf {
    dir.join(path_component(attr)).join("difflog")
}

/// Where the diffoscope output of `output` of `attr` is saved under `dir`.
pub fn diffoscope_path(dir: &Path, attr: &str, output: &str) -> PathBuf {
    dir.join(path_component(attr))
        .join("diffoscope")
        .join(path_component(output))
}

pub(crate) struct Saver {
    dir: PathBuf,
    created: bool,
    announced: bool,
}

impl Saver {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            created: false,
            announced: false,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether anything has been saved.
    pub fn used(&self) -> bool {
        self.created
    }

    /// True once after the first successful save, so it is reported once.
    pub fn first_save(&mut self) -> bool {
        let first = self.created && !self.announced;
        self.announced |= self.created;
        first
    }

    fn attr_dir(&self, attr: &str) -> PathBuf {
        self.dir.join(path_component(attr))
    }

    /// Remove what a previous run left for `attr`, so only this run's remain.
    pub fn reset(&self, attr: &str) -> io::Result<()> {
        match fs::remove_dir_all(self.attr_dir(attr)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    pub fn difflog(&mut self, attr: &str, diff: &[u8]) -> io::Result<()> {
        let path = difflog_path(&self.dir, attr);
        fs::create_dir_all(path.parent().expect("has a parent"))?;
        self.created = true;
        fs::write(path, diff)
    }

    /// Save the raw diffoscope output in `raw`, with colours removed.
    pub fn diffoscope(&mut self, attr: &str, output: &str, raw: &Path) -> io::Result<()> {
        let path = diffoscope_path(&self.dir, attr, output);
        fs::create_dir_all(path.parent().expect("has a parent"))?;
        self.created = true;
        let mut out = BufWriter::new(File::create(path)?);
        let mut input = BufReader::new(File::open(raw)?);
        let mut stripper = SgrStripper::default();
        let (mut buf, mut clean) = ([0u8; 64 * 1024], Vec::new());
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            clean.clear();
            stripper.feed(&buf[..n], &mut clean);
            out.write_all(&clean)?;
        }
        clean.clear();
        stripper.finish(&mut clean);
        out.write_all(&clean)?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_colour_stripping() {
        let tmp = tempfile::tempdir().unwrap();
        let mut saver = Saver::new(tmp.path().join("differences-x"));
        assert!(!saver.used());
        let raw = tmp.path().join("raw");
        fs::write(&raw, "\x1b[31m-old\x1b[0m\n+new\n").unwrap();

        saver.difflog("set.a", b"diff\n").unwrap();
        saver.diffoscope("set.a", "out", &raw).unwrap();
        saver.diffoscope("set.a", "../evil", &raw).unwrap();

        let base = tmp.path().join("differences-x/set.a");
        assert_eq!(fs::read_to_string(base.join("difflog")).unwrap(), "diff\n");
        assert_eq!(
            fs::read_to_string(base.join("diffoscope/out")).unwrap(),
            "-old\n+new\n"
        );
        assert!(base.join("diffoscope/.._evil").exists());
        assert!(saver.used());
        assert_eq!(difflog_path(saver.dir(), "set.a"), base.join("difflog"));
        assert_eq!(
            diffoscope_path(saver.dir(), "set.a", "out"),
            base.join("diffoscope/out")
        );

        saver.reset("set.a").unwrap();
        assert!(!base.exists());
        saver.reset("never-existed").unwrap();
    }
}
