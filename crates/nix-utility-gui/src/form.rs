//! The settings form on the run page.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::gtk::gio;

use crate::analysis::{AnalysisPrefs, AnalyzerSettings};
use crate::settings::Settings;

#[derive(Clone)]
pub struct Form {
    repo: Rc<RefCell<String>>,
    repo_row: adw::ActionRow,
    output_dir: Rc<RefCell<String>>,
    output_row: adw::ActionRow,
    attrs: adw::EntryRow,
    base: adw::EntryRow,
    system: adw::EntryRow,
    workers: adw::SpinRow,
    eval_memory: adw::SpinRow,
    diffoscope_memory: adw::SpinRow,
    timeout: adw::SpinRow,
    dry_run: adw::SwitchRow,
    /// Edited in the preferences window, so the form only carries it along.
    analysis: Rc<RefCell<AnalysisPrefs>>,
}

const REPO_PLACEHOLDER: &str = "No directory chosen";
const OUTPUT_PLACEHOLDER: &str = "Inside the nixpkgs checkout";

pub fn choose_folder(
    window: &adw::ApplicationWindow,
    title: &str,
    done: impl FnOnce(PathBuf) + 'static,
) {
    let dialog = gtk::FileDialog::builder().title(title).modal(true).build();
    dialog.select_folder(Some(window), gio::Cancellable::NONE, move |result| {
        if let Ok(file) = result
            && let Some(path) = file.path()
        {
            done(path);
        }
    });
}

fn spin(title: &str, subtitle: &str, max: f64, step: f64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(0.0, max, step);
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

fn folder_row(
    window: &adw::ApplicationWindow,
    title: &str,
    placeholder: &'static str,
    value: &Rc<RefCell<String>>,
    reset: bool,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(placeholder)
        .build();
    let button = gtk::Button::builder()
        .label("Choose…")
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&button);
    {
        let (window, row, value, title) = (
            window.clone(),
            row.clone(),
            Rc::clone(value),
            title.to_owned(),
        );
        button.connect_clicked(move |_| {
            let (row, value) = (row.clone(), Rc::clone(&value));
            choose_folder(&window, &title, move |path| {
                let path = path.to_string_lossy().into_owned();
                row.set_subtitle(&path);
                *value.borrow_mut() = path;
            });
        });
    }
    if reset {
        let clear = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text("Use the default")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        row.add_suffix(&clear);
        let (row, value) = (row.clone(), Rc::clone(value));
        clear.connect_clicked(move |_| {
            row.set_subtitle(placeholder);
            value.borrow_mut().clear();
        });
    }
    row
}

impl Form {
    /// The form and the widget to show it in.
    pub fn new(window: &adw::ApplicationWindow, settings: &Settings) -> (Self, gtk::Widget) {
        let repo = Rc::new(RefCell::new(String::new()));
        let output_dir = Rc::new(RefCell::new(String::new()));
        let repo_row = folder_row(window, "Nixpkgs checkout", REPO_PLACEHOLDER, &repo, false);
        let output_row = folder_row(
            window,
            "Save results in",
            OUTPUT_PLACEHOLDER,
            &output_dir,
            true,
        );

        let entry = |title: &str| adw::EntryRow::builder().title(title).build();
        let attrs = entry("Attributes (separated by spaces or commas)");
        let base = entry("Base revision (default: merge base of master and HEAD)");
        let system = entry("System");
        let workers = spin("Evaluation workers", "Each needs GBs of memory", 64.0, 1.0);
        let eval_memory = spin(
            "Evaluator memory limit (MiB)",
            "An evaluator over this restarts itself; 0 for no limit",
            1_048_576.0,
            256.0,
        );
        let diffoscope_memory = spin(
            "diffoscope memory limit (MiB)",
            "Needs a systemd user session; 0 for no limit",
            1_048_576.0,
            256.0,
        );
        let timeout = spin(
            "Time limit per command (minutes)",
            "0 for no limit",
            10_080.0,
            5.0,
        );
        let dry_run = adw::SwitchRow::builder()
            .title("Dry run")
            .subtitle("Evaluate and list what would be compared; build nothing")
            .build();

        let comparison = adw::PreferencesGroup::builder().title("Comparison").build();
        comparison.add(&repo_row);
        for row in [&attrs, &base, &system] {
            comparison.add(row);
        }
        comparison.add(&output_row);
        let resources = adw::PreferencesGroup::builder().title("Resources").build();
        for row in [&workers, &eval_memory, &diffoscope_memory, &timeout] {
            resources.add(row);
        }
        resources.add(&dry_run);

        let page = gtk::Box::new(gtk::Orientation::Vertical, 18);
        page.set_margin_top(12);
        page.set_margin_bottom(12);
        page.append(&comparison);
        page.append(&resources);
        let clamp = adw::Clamp::builder()
            .maximum_size(720)
            .child(&page)
            .margin_start(12)
            .margin_end(12)
            .build();

        let form = Self {
            repo,
            repo_row,
            output_dir,
            output_row,
            attrs,
            base,
            system,
            workers,
            eval_memory,
            diffoscope_memory,
            timeout,
            dry_run,
            analysis: Rc::default(),
        };
        form.write(settings);
        (form, clamp.upcast())
    }

    pub fn write(&self, settings: &Settings) {
        let set_folder = |row: &adw::ActionRow, cell: &Rc<RefCell<String>>, value: &str, empty| {
            *cell.borrow_mut() = value.to_owned();
            row.set_subtitle(if value.is_empty() { empty } else { value });
        };
        set_folder(&self.repo_row, &self.repo, &settings.repo, REPO_PLACEHOLDER);
        set_folder(
            &self.output_row,
            &self.output_dir,
            &settings.output_dir,
            OUTPUT_PLACEHOLDER,
        );
        self.attrs.set_text(&settings.attrs);
        self.base.set_text(&settings.base);
        self.system.set_text(&settings.system);
        self.workers.set_value(f64::from(settings.workers));
        self.eval_memory
            .set_value(settings.max_eval_memory_mib as f64);
        self.diffoscope_memory
            .set_value(settings.diffoscope_memory_mib as f64);
        self.timeout.set_value(settings.timeout_minutes as f64);
        self.dry_run.set_active(settings.dry_run);
        *self.analysis.borrow_mut() = settings.analysis.clone();
    }

    /// The profile in use.
    pub fn analyzer(&self) -> AnalyzerSettings {
        self.analysis.borrow().active().clone()
    }

    pub fn analysis(&self) -> AnalysisPrefs {
        self.analysis.borrow().clone()
    }

    pub fn set_analysis(&self, prefs: AnalysisPrefs) {
        *self.analysis.borrow_mut() = prefs;
    }

    /// Use the profile called `name`; false when there is none.
    pub fn select_profile(&self, name: &str) -> bool {
        self.analysis.borrow_mut().select(name)
    }

    pub fn read(&self) -> Settings {
        Settings {
            repo: self.repo.borrow().clone(),
            attrs: self.attrs.text().into(),
            base: self.base.text().into(),
            system: self.system.text().into(),
            workers: self.workers.value() as u32,
            max_eval_memory_mib: self.eval_memory.value() as u64,
            diffoscope_memory_mib: self.diffoscope_memory.value() as u64,
            timeout_minutes: self.timeout.value() as u64,
            output_dir: self.output_dir.borrow().clone(),
            dry_run: self.dry_run.is_active(),
            analysis: self.analysis.borrow().clone(),
        }
    }
}
