//! Command-line interface (clap derive only; no logic lives here).

use clap::Parser;
use clap_verbosity_flag::{InfoLevel, Verbosity};

const LONG_ABOUT: &str = "\
Compare two Nix build logs, normalizing out boring differences.

This can be useful to test the effect of a change to build scripts,
especially a refactoring that's intended to have no effect.

Each argument can be either:
 * the store path of a build output, or
 * the store path of a derivation for the build.

Each argument will be passed to `nix log` to get the log.

An argument that is a symlink (./result, /run/current-system, a chain of
links, ...) is followed to its final target first, and the target is what
is passed to `nix log`; a note about this goes to stderr. Use --no-follow
to pass arguments to `nix log` exactly as given. Arguments that are not
symlinks (store paths, installables like nixpkgs#hello) are never altered.

Normalization includes:
 * replacing all store path hashes with a constant
 * stripping ECMA-48(-style) terminal controls, like colors
 * replacing dates, timestamps, and timings with constants
 * removing stdenv phase footers (\"buildPhase completed in 1 minutes
   5 seconds\"); they are only printed for phases of 30s or more, so
   the whole line is dropped, not just the duration
   (disable with --keep-timings)
 * replacing process ids (\"(process:842):\", \"Started with PID: 1766\")
   with N (disable with --keep-pids)
 * replacing random temp dir names (\"/build/qsb-RoAGjy/...\": six random
   letters after \"NAME-\" directly under /build or /tmp) with XXXXXX
   (disable with --keep-paths)
 * replacing the kernel version CMake prints (\"CMAKE_HOST_SYSTEM:
   \"Linux-6.18.57\"\", \"CMAKE_HOST_SYSTEM_VERSION: \"6.18.57\"\") with
   KERNEL_VERSION (disable with --keep-kernel-version)
 * replacing GLib log timestamps \"**: 07:21:29.789:\" with a constant
   (disable with --keep-timings)
 * replacing the Qt Test \"Totals: ..., 4393ms\" duration with Nms
   (disable with --keep-timings)
 * replacing random D-Bus session address parts
   (\"/tmp/dbus-ggOBj2AOpQ,guid=12ad...\") with constants
   (disable with --keep-paths)
 * replacing CMake step timings (\"-- Configuring done (14.6s)\") with
   \"(Ns)\" (disable with --keep-timings)
 * replacing GN completion timings (\"-- GN Done. Made ... in 20890ms\")
   with \"Nms\" (disable with --keep-timings)
 * replacing build IDs, both \"build id 36ad3d...\" / \"(build ID 36ad3d...)\"
   in the fixup phase output and in \".build-id/36/ad3d....\" file names,
   with constants
   (disable with --keep-paths)
 * replacing job and thread counts (-j2, -j23, --jobs=4, \"using 97
   parallel threads\") with N
   (disable with --keep-jobs)
 * replacing progress counters (\"[13/17]\" -> \"[N/N]\", \"[ 45%]\" ->
   \"[N%]\", and CTest's \" 9/86 Test  #7:\" -> \"N/N Test  #7:\") at the
   start of lines; both the counter and the total are replaced
   (disable with --keep-progress)
 * replacing CTest durations (\"Passed    0.02 sec\", \"Total Test time
   (real) =   3.39 sec\") with \"N sec\" (disable with --keep-timings)
 * sorting runs of certain common messages, to reduce
   nondeterminism from concurrency

Off by default:
  --sort-lines  Sort the lines of each normalized log (byte order, keeping
                duplicates) before diffing. Useful when a parallel build
                interleaves its output differently from run to run, at the
                cost of losing all information about ordering; a note is
                printed to stderr when it is on.

Any further arguments are passed to `git diff --no-index`, e.g.
`--word-diff` or `-U20`. It runs inside the temp dir holding the
normalized logs, so relative paths in those options refer to that dir.

Exit status: 0 = no differences, 1 = differences, 2 = error.";

const AFTER_HELP: &str =
    "original program by gnprice https://gist.github.com/gnprice/1864b871656773f78f419201e8b5cda3";

#[derive(Debug, Parser)]
#[command(
    name = "difflog",
    version,
    about = "Compare two Nix build logs, normalizing out boring differences",
    long_about = LONG_ABOUT,
    after_long_help = AFTER_HELP,
)]
pub struct Cli {
    /// First build: a store path of an output or derivation (anything `nix log` accepts)
    #[arg(value_name = "OUT1")]
    pub first: String,

    /// Second build, same forms as OUT1
    #[arg(value_name = "OUT2")]
    pub second: String,

    /// Options passed to `git diff --no-index` (everything after OUT2)
    #[arg(
        value_name = "GIT_DIFF_OPTION",
        num_args = 0..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub git_args: Vec<String>,

    /// Do not follow symlinks; pass the arguments to `nix log` exactly as given
    #[arg(long)]
    pub no_follow: bool,

    /// Keep dates, timestamps, durations and stdenv phase footers
    #[arg(long)]
    pub keep_timings: bool,

    /// Keep random temp dir names, D-Bus addresses and build IDs
    #[arg(long)]
    pub keep_paths: bool,

    /// Keep job and thread counts (-jN, --jobs=N, "N parallel threads")
    #[arg(long)]
    pub keep_jobs: bool,

    /// Keep progress counters ("[13/17]", "[ 45%]", CTest counters)
    #[arg(long)]
    pub keep_progress: bool,

    /// Keep process ids
    #[arg(long)]
    pub keep_pids: bool,

    /// Keep the kernel version CMake prints
    #[arg(long)]
    pub keep_kernel_version: bool,

    /// Sort the lines of each normalized log before diffing (hides ordering differences)
    #[arg(long)]
    pub sort_lines: bool,

    /// Show what would be done (resolved arguments, `nix log` and `git diff`
    /// commands) without running them or creating any files
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub verbose: Verbosity<InfoLevel>,
}
