//! Logging on top of the `log` crate, writing `diffoscope-nix: message` lines
//! to stderr (diffoscope's own output goes to stdout).

use std::io::Write;

pub fn init() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| writeln!(buf, "diffoscope-nix: {}", record.args()))
        .init();
}
