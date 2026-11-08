use std::time::Duration;

use clap::Parser;

const AFTER_HELP: &str = "\
Differences are also saved (without colour) under
  $PWD/differences-ATTRS-BRANCH-SHORTREV/FULL-ATTRPATH/
    difflog             the build log diff
    diffoscope/OUTPUT   the diffoscope-nix output, one file per output
ATTRS is the attributes given, joined with \"+\"; BRANCH and SHORTREV are those
of the working tree. Nothing is saved when no differences are found.

Environment:
  SYSTEM       system to evaluate for      (default: x86_64-linux)
  WORKERS      nix-eval-jobs worker count  (default: 2)
  DIFF_PAGER   pager for tool output       (default: less -FR)
  NO_COLOR     disable colour (otherwise used on terminals)";

/// Compare one or more nixpkgs attributes (package sets or single packages)
/// between a base revision and the working tree. Every non-debug output
/// whose store path changed is built on both sides, with one nix build per
/// side for all attributes together, and run through difflog and
/// diffoscope-nix.
#[derive(Debug, Parser)]
#[command(version, after_help = AFTER_HELP)]
pub struct Cli {
    /// Attribute path in pkgs: a package set, a single package, or one or
    /// more of either, e.g. `qt6Packages`, `qt6Packages.qtwebengine`.
    /// Flake-style paths (pkgs.X, legacyPackages.SYSTEM.X) work too.
    /// Overlapping attributes are only diffed once.
    #[arg(value_name = "ATTR", required = true)]
    pub attrs: Vec<String>,

    /// Revision to compare against (default: merge-base of master and HEAD).
    #[arg(short, long, value_name = "REV")]
    pub base: Option<String>,

    /// Evaluate both sides, list what would be diffed, and run `nix build
    /// --dry-run`; builds nothing and runs no diff tools.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Print the diff output directly (default: pipe it through a pager when
    /// stdout is a terminal).
    #[arg(long)]
    pub no_pager: bool,

    /// Also wait for Enter between packages (q quits).
    #[arg(long)]
    pub pause: bool,

    /// System to evaluate for.
    #[arg(long, env = "SYSTEM", default_value = "x86_64-linux")]
    pub system: String,

    /// nix-eval-jobs worker count; each worker is a full nixpkgs evaluator
    /// and can use gigabytes of memory.
    #[arg(long, env = "WORKERS", default_value_t = 2, value_name = "N")]
    pub workers: u32,

    /// Memory limit per nix-eval-jobs worker in MiB; a worker over it is
    /// restarted (0: no limit).
    #[arg(long, default_value_t = 4096, value_name = "MIB")]
    pub max_eval_memory: u64,

    /// Memory limit for each diffoscope run, e.g. 8G or 4096M (bytes
    /// without a suffix). Enforced with `systemd-run --user --scope`;
    /// nothing else (nix, nix-eval-jobs) is limited that way.
    #[arg(long, value_name = "SIZE", value_parser = parse_size, conflicts_with = "no_systemd_run")]
    pub memory_max: Option<u64>,

    /// Time limit for each external command in seconds (0: no limit).
    #[arg(long, default_value = "0", value_name = "SECONDS", value_parser = parse_timeout)]
    pub timeout: Timeout,

    /// Never use systemd-run for the diffoscope memory limit.
    #[arg(long)]
    pub no_systemd_run: bool,

    #[arg(long, env = "DIFF_PAGER", hide = true)]
    pub diff_pager: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout(pub Option<Duration>);

fn parse_timeout(s: &str) -> Result<Timeout, String> {
    let seconds: u64 = s.parse().map_err(|e| format!("{e}"))?;
    Ok(Timeout((seconds > 0).then(|| Duration::from_secs(seconds))))
}

/// `4096`, `512K`, `4096M`, `8G`, `1T` (powers of 1024).
fn parse_size(s: &str) -> Result<u64, String> {
    let (digits, multiplier) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (
            &s[..i],
            match c.to_ascii_uppercase() {
                'K' => 1 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                _ => return Err(format!("unknown size suffix `{c}` (use K, M, G or T)")),
            },
        ),
        _ => (s, 1),
    };
    let number: u64 = digits.parse().map_err(|_| format!("invalid size `{s}`"))?;
    let bytes = number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size `{s}` is too large"))?;
    if bytes == 0 {
        return Err("size must be more than zero".to_owned());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("512k"), Ok(512 << 10));
        assert_eq!(parse_size("8G"), Ok(8 << 30));
        assert!(parse_size("8X").is_err());
        assert!(parse_size("G").is_err());
        assert!(parse_size("0").is_err());
        assert!(parse_size("99999999999T").is_err());
    }

    #[test]
    fn timeouts() {
        assert_eq!(parse_timeout("0"), Ok(Timeout(None)));
        assert_eq!(
            parse_timeout("90"),
            Ok(Timeout(Some(Duration::from_secs(90))))
        );
        assert!(parse_timeout("-1").is_err());
    }

    #[test]
    fn clap_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
