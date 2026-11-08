mod activity;
mod ansi;
mod app;
mod form;
mod panes;
mod results;
mod settings;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let app = relm4::RelmApp::new("io.github.artturin.NixUtilityGui");
    sourceview5::init();
    app.run::<app::App>(());
}
