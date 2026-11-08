//! Which external programs are available.

/// What is on `PATH`. A frontend can show this before offering to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preflight {
    pub git: bool,
    pub nix: bool,
    pub diffoscope: bool,
    /// Without it, evaluation falls back to `nix run nixpkgs#nix-eval-jobs`.
    pub nix_eval_jobs: bool,
}

impl Preflight {
    pub fn check() -> Self {
        let have = |program| which::which(program).is_ok();
        Self {
            git: have("git"),
            nix: have("nix"),
            diffoscope: have("diffoscope"),
            nix_eval_jobs: have("nix-eval-jobs"),
        }
    }

    /// Programs a run cannot do without (a dry run never runs diffoscope).
    pub fn missing(&self, dry_run: bool) -> Vec<&'static str> {
        [
            ("git", self.git),
            ("nix", self.nix),
            ("diffoscope", self.diffoscope || dry_run),
        ]
        .into_iter()
        .filter_map(|(name, present)| (!present).then_some(name))
        .collect()
    }
}
