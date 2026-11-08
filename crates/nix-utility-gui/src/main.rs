mod activity;
mod analysis;
mod analysis_view;
mod ansi;
mod app;
mod form;
mod keystore;
mod panes;
mod prefs;
mod queue;
mod results;
mod runner;
mod settings;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let app = relm4::RelmApp::new("io.github.artturin.NixUtilityGui");
    sourceview5::init();
    app.run::<app::App>(());
}
