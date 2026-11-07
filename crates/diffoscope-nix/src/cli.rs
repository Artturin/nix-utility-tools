//! Command-line interface (all clap derives live here).

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

/// Run diffoscope on two Nix store paths, ignoring store path hash differences.
#[derive(Debug, Parser)]
#[command(
    version,
    about,
    long_about = include_str!("help.txt"),
    override_usage = "diffoscope-nix [OPTIONS] <PATH1> <PATH2> [DIFFOSCOPE_ARGS]..."
)]
pub struct Cli {
    /// Don't copy or run anything; list which files/symlinks would be
    /// rewritten and the diffoscope command line.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Don't zero the GNU build ID. By default the build ID (NT_GNU_BUILD_ID
    /// note, i.e. .note.gnu.build-id) is zeroed in every ELF file. The ID is a
    /// hash of the build, so it differs whenever anything else does. Its bytes
    /// can also sit right in front of a string such as the interpreter path,
    /// where `strings` glues any printable ones onto it
    /// (yD/nix/store/...-ld-linux-x86-64.so.2), so zeroing it removes that
    /// noise as well. Doesn't touch anything derived from the ID, such as
    /// .build-id/xx/yyyy.debug file names or .gnu_debuglink checksums.
    #[arg(long)]
    pub keep_build_id: bool,

    /// First store path (file or directory)
    pub path1: PathBuf,

    /// Second store path (file or directory)
    pub path2: PathBuf,

    /// Passed to diffoscope unchanged
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub diffoscope_args: Vec<OsString>,
}
