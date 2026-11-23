mod activity;
mod analysis;
mod analysis_view;
mod ansi;
mod app;
mod form;
mod keystore;
mod package_view;
mod panes;
mod prefs;
mod queue;
mod results;
mod runner;
mod settings;

use relm4::gtk::gio;
use relm4::gtk::prelude::*;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let app = relm4::RelmApp::new("io.github.artturin.NixUtilityGui");
    // `nix-utility-gui DIR` opens a saved differences directory; the window
    // handles the `open` signal.
    let gapp = relm4::main_application();
    gapp.set_flags(gapp.flags() | gio::ApplicationFlags::HANDLES_OPEN);
    sourceview5::init();
    app.run::<app::App>(());
}
