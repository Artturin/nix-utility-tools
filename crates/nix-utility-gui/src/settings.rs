//! What the user configures. Saved as TOML under the XDG config directory;
//! nothing secret lives here.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diff_nixpkgs_attr_core::{Config, Control, ProcessPolicy, SystemdRun};
use serde::{Deserialize, Serialize};

use crate::analysis::AnalysisPrefs;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The nixpkgs checkout.
    pub repo: String,
    /// Attributes, separated by whitespace or commas.
    pub attrs: String,
    /// Empty: the merge base of master and HEAD.
    pub base: String,
    pub system: String,
    pub workers: u32,
    pub max_eval_memory_mib: u64,
    /// Memory cap for each diffoscope run in MiB; 0 for none.
    pub diffoscope_memory_mib: u64,
    /// Time limit per external command in minutes; 0 for none.
    pub timeout_minutes: u64,
    /// Where `differences-…` goes; empty: inside the nixpkgs checkout.
    pub output_dir: String,
    pub dry_run: bool,
    pub analysis: AnalysisPrefs,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            repo: String::new(),
            attrs: String::new(),
            base: String::new(),
            system: format!("{}-linux", std::env::consts::ARCH),
            workers: 2,
            max_eval_memory_mib: 4096,
            diffoscope_memory_mib: 0,
            timeout_minutes: 0,
            output_dir: String::new(),
            dry_run: false,
            analysis: AnalysisPrefs::default(),
        }
    }
}

fn config_file() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "nix-utility-gui")?;
    Some(dirs.config_dir().join("settings.toml"))
}

impl Settings {
    pub fn load() -> Self {
        config_file().map_or_else(Self::default, |path| Self::load_from(&path))
    }

    pub fn load_from(path: &Path) -> Self {
        let mut settings = Self::read_from(path);
        settings.analysis.normalize();
        settings
    }

    fn read_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                log::warn!("ignoring unreadable settings {}: {err}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// When started from a terminal, the shell's directory is the checkout.
    pub fn with_launch_dir(mut self) -> Self {
        if std::io::stdin().is_terminal()
            && let Ok(dir) = std::env::current_dir()
        {
            self.repo = dir.to_string_lossy().into_owned();
        }
        self
    }

    pub fn save(&self) -> std::io::Result<()> {
        match config_file() {
            Some(path) => self.save_to(&path),
            None => Ok(()),
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    pub fn attr_list(&self) -> Vec<String> {
        self.attrs
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Where results go: the chosen directory, else the nixpkgs checkout.
    pub fn effective_output_dir(&self) -> PathBuf {
        if self.output_dir.trim().is_empty() {
            PathBuf::from(self.repo.trim())
        } else {
            PathBuf::from(self.output_dir.trim())
        }
    }

    /// The comparison to run, or what is wrong with the input.
    pub fn config(&self) -> Result<Config, String> {
        let repo = self.repo.trim();
        if repo.is_empty() {
            return Err("Choose the nixpkgs checkout".to_owned());
        }
        if !Path::new(repo).is_dir() {
            return Err(format!("{repo} is not a directory"));
        }
        let attrs = self.attr_list();
        if attrs.is_empty() {
            return Err("Enter at least one attribute".to_owned());
        }
        let output_dir = self.effective_output_dir();
        let mut cfg = Config::new(repo, attrs, output_dir);
        let base = self.base.trim();
        cfg.base = (!base.is_empty()).then(|| base.to_owned());
        cfg.system = self.system.trim().to_owned();
        cfg.eval_workers = self.workers.max(1);
        cfg.eval_max_memory_mib =
            (self.max_eval_memory_mib > 0).then_some(self.max_eval_memory_mib);
        cfg.dry_run = self.dry_run;
        Ok(cfg)
    }

    pub fn control(&self, token: diff_nixpkgs_attr_core::CancelToken) -> Control {
        Control::new(
            token,
            ProcessPolicy {
                memory_max: (self.diffoscope_memory_mib > 0)
                    .then(|| self.diffoscope_memory_mib * 1024 * 1024),
                timeout: (self.timeout_minutes > 0)
                    .then(|| Duration::from_secs(self.timeout_minutes * 60)),
                systemd_run: SystemdRun::Auto,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_split_on_whitespace_and_commas() {
        let settings = Settings {
            attrs: " qt6Packages.qtbase,kdePackages.dolphin\n hello ".into(),
            ..Settings::default()
        };
        assert_eq!(
            settings.attr_list(),
            ["qt6Packages.qtbase", "kdePackages.dolphin", "hello"]
        );
    }

    #[test]
    fn output_dir_defaults_to_the_checkout() {
        let mut settings = Settings {
            repo: "/src/nixpkgs".into(),
            ..Settings::default()
        };
        assert_eq!(settings.effective_output_dir(), Path::new("/src/nixpkgs"));
        settings.output_dir = "/tmp/out".into();
        assert_eq!(settings.effective_output_dir(), Path::new("/tmp/out"));
    }

    #[test]
    fn config_validates_input() {
        let tmp = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        assert!(settings.config().unwrap_err().contains("checkout"));
        settings.repo = tmp.path().join("nope").display().to_string();
        assert!(settings.config().unwrap_err().contains("not a directory"));
        settings.repo = tmp.path().display().to_string();
        assert!(settings.config().unwrap_err().contains("attribute"));
        settings.attrs = "hello".into();
        settings.base = " HEAD~3 ".into();
        settings.max_eval_memory_mib = 0;
        let cfg = settings.config().unwrap();
        assert_eq!(cfg.base.as_deref(), Some("HEAD~3"));
        assert_eq!(cfg.eval_max_memory_mib, None);
        assert_eq!(cfg.output_dir, tmp.path());
    }

    #[test]
    fn policy_maps_units() {
        let settings = Settings {
            diffoscope_memory_mib: 2048,
            timeout_minutes: 3,
            ..Settings::default()
        };
        let ctl = settings.control(Default::default());
        assert_eq!(ctl.policy.memory_max, Some(2048 << 20));
        assert_eq!(ctl.policy.timeout, Some(Duration::from_secs(180)));
        assert_eq!(
            Settings::default()
                .control(Default::default())
                .policy
                .memory_max,
            None
        );
    }

    #[test]
    fn round_trips_and_tolerates_missing_or_bad_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conf/settings.toml");
        assert_eq!(Settings::load_from(&path), Settings::default());
        let settings = Settings {
            repo: "/x".into(),
            workers: 3,
            ..Settings::default()
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
        std::fs::write(&path, "workers = \"many\"").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        std::fs::write(&path, "repo = \"/y\"").unwrap();
        assert_eq!(Settings::load_from(&path).repo, "/y");
    }

    #[test]
    fn profiles_round_trip_and_an_empty_list_gets_a_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        let mut settings = Settings::default();
        settings.analysis.save_host = true;
        settings.analysis.active_mut().model = "qwen3".into();
        settings.analysis.add_copy();
        settings.analysis.rename_active("Claude").unwrap();
        settings.analysis.active_mut().endpoint = crate::analysis::Endpoint::Openai;
        settings.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[analysis.profiles]]"), "{text}");
        assert_eq!(Settings::load_from(&path), settings);

        std::fs::write(&path, "[analysis]\nactive = \"x\"\nprofiles = []\n").unwrap();
        let loaded = Settings::load_from(&path).analysis;
        assert_eq!(loaded.names(), ["Default"]);
        assert_eq!(loaded.active, "Default");
    }
}
