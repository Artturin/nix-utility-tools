//! The application window and how it reacts to a run.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use diff_nixpkgs_attr_core::tools::Preflight;
use diff_nixpkgs_attr_core::{
    CancelToken, Event, Flow, Report, Status as RunStatus, Tool, ToolStatus, analysis_path,
    run_pipeline,
};
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::activity::{Activity, Kind, Line, describe};
use crate::analysis::{
    self, AnalysisResult, AnalyzerSettings, AutoTiming, SizeCheck, Skipped, State, Trigger,
};
use crate::analysis_view::{self, AnalysisPane};
use crate::form::{Form, choose_folder};
use crate::keystore::{FallbackKeyStore, KeyStore};
use crate::panes;
use crate::prefs;
use crate::queue::{Item, Queue};
use crate::results::Results;
use crate::runner::{self, Job, Outcome, Update};
use crate::settings::Settings;

#[derive(Debug)]
pub enum Msg {
    Run(Settings),
    Cancel,
    Event(Event),
    Finished(Result<Report, String>),
    Load(PathBuf),
    Select(usize),
    ThemeChanged,
    Analyze(usize),
    AnalyzeAnyway(usize),
    CancelAnalysis(usize),
    /// An update from the analysis started in generation `.0` for `.1`.
    Analysis(u64, String, Update),
    OpenPrefs,
    SetAnalyzer(AnalyzerSettings),
    SortByVerdict,
}

pub struct App {
    form: Form,
    keys: Arc<dyn KeyStore>,
    queue: Queue,
    missing: Vec<&'static str>,
    token: Rc<RefCell<Option<CancelToken>>>,
    quit_when_finished: Rc<Cell<bool>>,
    results: RefCell<Results>,
    selected: Option<usize>,
    activity: Activity,
    // What the view has yet to show.
    log: RefCell<Vec<Line>>,
    clear_log: Cell<bool>,
    list_dirty: Cell<bool>,
    expand_same: Cell<bool>,
    pane_dirty: Cell<bool>,
    analysis_dirty: Cell<bool>,
    open_prefs: Cell<bool>,
    show_results: Cell<bool>,
    toast: RefCell<Option<String>>,
}

pub struct Widgets {
    window: adw::ApplicationWindow,
    stack: adw::ViewStack,
    banner: adw::Banner,
    toasts: adw::ToastOverlay,
    run_button: gtk::Button,
    cancel_button: gtk::Button,
    open_button: gtk::Button,
    progress: gtk::ProgressBar,
    status: gtk::Label,
    log_buffer: gtk::TextBuffer,
    log_view: gtk::TextView,
    list: gtk::ListBox,
    same_list: gtk::ListBox,
    same_expander: gtk::Expander,
    // Package index of each row, per list.
    list_rows: Rc<RefCell<Vec<usize>>>,
    same_rows: Rc<RefCell<Vec<usize>>>,
    content: adw::ViewStack,
    notebook: gtk::Notebook,
    rendered: Option<usize>,
    /// The Analysis tab of the shown package and the size of its diff.
    analysis: Option<(AnalysisPane, usize)>,
    queue_label: gtk::Label,
    sort_button: gtk::Button,
}

impl App {
    fn running(&self) -> bool {
        self.token.borrow().is_some()
    }

    fn say(&self, kind: Kind, text: impl Into<String>) {
        self.log.borrow_mut().push(Line {
            kind,
            text: text.into(),
        });
    }

    fn toast(&self, text: impl Into<String>) {
        *self.toast.borrow_mut() = Some(text.into());
    }

    fn select_first(&mut self) {
        let results = self.results.borrow();
        self.selected = results
            .packages
            .iter()
            .position(|p| p.has_differences())
            .or(if results.packages.is_empty() {
                None
            } else {
                Some(0)
            });
        if let Some(package) = self.selected.and_then(|i| results.packages.get(i))
            && package.unchanged()
        {
            self.expand_same.set(true);
        }
        self.list_dirty.set(true);
        self.pane_dirty.set(true);
    }

    fn analyzer(&self) -> AnalyzerSettings {
        self.form.analyzer()
    }

    fn set_analysis(&self, index: usize, state: State) {
        if let Some(package) = self.results.borrow_mut().packages.get_mut(index) {
            package.analysis = state;
        }
        self.list_dirty.set(true);
        self.analysis_dirty.set(true);
    }

    /// The diff of package `index` to send, or the state that says why not.
    fn prepare(
        &self,
        index: usize,
        bypass: bool,
        settings: &AnalyzerSettings,
    ) -> Result<String, State> {
        let diff = self
            .results
            .borrow_mut()
            .diff_text(index)
            .ok_or(State::Idle)?;
        if !bypass {
            match settings.check_size(diff.bytes) {
                SizeCheck::Fits => {}
                SizeCheck::TooLarge { bytes, limit } => {
                    return Err(State::TooLarge { bytes, limit });
                }
                SizeCheck::ContextTooSmall => return Err(State::ContextTooSmall),
            }
        }
        Ok(diff.text)
    }

    /// Queue an analysis of package `index`. A manual request without a
    /// configured model opens the preferences; an automatic one is skipped.
    fn request(
        &mut self,
        index: usize,
        trigger: Trigger,
        bypass: bool,
        sender: &ComponentSender<Self>,
    ) {
        let settings = self.analyzer();
        let Some(attr) = self
            .results
            .borrow()
            .packages
            .get(index)
            .map(|p| p.attr.clone())
        else {
            return;
        };
        if self.queue.has(&attr) {
            return;
        }
        if !settings.configured() {
            if trigger == Trigger::Manual {
                self.open_prefs.set(true);
                self.toast("Choose a model to analyze with first");
            }
            return;
        }
        if let Err(state) = self.prepare(index, bypass, &settings) {
            self.set_analysis(index, state);
            return;
        }
        if trigger == Trigger::Auto {
            if !self.queue.take_auto_slot(settings.auto_cap) {
                self.set_analysis(index, State::Skipped(Skipped::CapReached));
                return;
            }
            self.queue.sort_when_idle = true;
        }
        self.set_analysis(index, State::Queued);
        self.queue.push(Item {
            attr,
            trigger,
            bypass,
        });
        self.start_next(sender);
    }

    fn start_next(&mut self, sender: &ComponentSender<Self>) {
        if self.queue.running() {
            return;
        }
        let settings = self.analyzer();
        while let Some(item) = self.queue.pop() {
            let Some(index) = self.results.borrow().index_of(&item.attr) else {
                self.queue.dropped();
                continue;
            };
            let diff = match self.prepare(index, item.bypass, &settings) {
                Ok(diff) => diff,
                Err(state) => {
                    self.set_analysis(index, state);
                    self.queue.dropped();
                    continue;
                }
            };
            let token = CancelToken::new();
            self.queue.begin(&item.attr, token.clone());
            self.set_analysis(
                index,
                State::Running {
                    thinking: String::new(),
                    answer: String::new(),
                },
            );
            let tx = sender.input_sender().clone();
            let (generation, attr) = (self.queue.generation(), item.attr.clone());
            let job = Job {
                diff,
                trigger: item.trigger,
            };
            runner::spawn(
                settings,
                Arc::clone(&self.keys),
                job,
                token,
                move |update| {
                    tx.emit(Msg::Analysis(generation, attr.clone(), update));
                },
            );
            return;
        }
        self.queue_idle();
    }

    /// Put consequential packages first once an automatic queue is done, but
    /// not while the run is still adding rows.
    fn queue_idle(&mut self) {
        if self.queue.active() || self.running() || !self.queue.sort_when_idle {
            return;
        }
        self.queue.sort_when_idle = false;
        self.results.borrow_mut().sort_by_verdict();
        self.list_dirty.set(true);
    }

    /// Stop what is running and forget what is waiting.
    fn cancel_analyses(&mut self) {
        for attr in self.queue.cancel_all() {
            let index = self.results.borrow().index_of(&attr);
            if let Some(index) = index {
                self.set_analysis(index, State::Idle);
            }
        }
    }

    fn save_analysis(&self, attr: &str, result: &AnalysisResult) {
        let Some(dir) = self.results.borrow().saved_dir.clone() else {
            return;
        };
        let path = analysis_path(&dir, attr);
        if let Err(err) = analysis::save(&path, result, self.analyzer().save_host) {
            self.toast(format!("Cannot save the analysis of {attr}: {err}"));
        }
    }

    /// Write the analyses that finished before the results had a directory.
    fn save_pending(&self) {
        let Some(dir) = self.results.borrow().saved_dir.clone() else {
            return;
        };
        for package in &self.results.borrow().packages {
            if let State::Done(result) = &package.analysis
                && !analysis_path(&dir, &package.attr).exists()
            {
                self.save_analysis(&package.attr, result);
            }
        }
    }

    /// Queue every package with differences, for automatic analysis.
    fn request_all(&mut self, sender: &ComponentSender<Self>) {
        let indices: Vec<usize> = {
            let results = self.results.borrow();
            results
                .order
                .iter()
                .copied()
                .filter(|&i| results.packages[i].has_differences())
                .collect()
        };
        for index in indices {
            self.request(index, Trigger::Auto, false, sender);
        }
    }
}

fn event_attr(event: &Event) -> Option<&str> {
    match event {
        Event::ToolStarted { attr, .. }
        | Event::ToolFinished { attr, .. }
        | Event::Skipped { attr, .. } => Some(attr),
        _ => None,
    }
}

fn button(label: &str, class: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .css_classes([class])
        .build()
}

impl SimpleComponent for App {
    type Init = ();
    type Input = Msg;
    type Output = ();
    type Root = adw::ApplicationWindow;
    type Widgets = Widgets;

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title("Nix utility tools")
            .default_width(1150)
            .default_height(780)
            .build()
    }

    fn init(
        _init: Self::Init,
        window: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let settings = Settings::load();
        let missing = Preflight::check().missing(settings.dry_run);
        let (form, form_widget) = Form::new(&window, &settings);

        // Run page: the form above, progress and the log below.
        let progress = gtk::ProgressBar::builder().margin_top(6).build();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let log_buffer = panes::log_buffer();
        let (log_scroller, log_view) = panes::log_view(&log_buffer);
        log_scroller.set_size_request(-1, 150);
        let activity_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        for margin in [
            activity_box.clone().upcast::<gtk::Widget>(),
            progress.clone().upcast(),
        ] {
            margin.set_margin_start(12);
            margin.set_margin_end(12);
        }
        activity_box.append(&progress);
        activity_box.append(&status);
        activity_box.append(&log_scroller);
        activity_box.set_margin_bottom(12);
        let form_scroller = gtk::ScrolledWindow::builder()
            .child(&form_widget)
            .vexpand(true)
            .build();
        let run_page = gtk::Paned::new(gtk::Orientation::Vertical);
        run_page.set_start_child(Some(&form_scroller));
        run_page.set_end_child(Some(&activity_box));
        run_page.set_resize_start_child(true);
        run_page.set_resize_end_child(true);
        run_page.set_shrink_start_child(false);
        run_page.set_shrink_end_child(false);
        run_page.set_position(430);

        // Results page.
        let list = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .build();
        let same_list = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .build();
        let same_expander = gtk::Expander::builder()
            .child(&same_list)
            .visible(false)
            .margin_start(6)
            .margin_end(6)
            .build();
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let queue_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let sort_button = gtk::Button::builder()
            .label("Sort by verdict")
            .tooltip_text("Show packages with consequential differences first")
            .css_classes(["flat", "caption"])
            .build();
        let sidebar_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sidebar_bar.set_margin_start(12);
        sidebar_bar.set_margin_end(6);
        sidebar_bar.set_margin_top(6);
        sidebar_bar.append(&queue_label);
        sidebar_bar.append(&sort_button);
        sidebar.append(&sidebar_bar);
        sidebar.append(&list);
        sidebar.append(&same_expander);
        let list_scroller = gtk::ScrolledWindow::builder().child(&sidebar).build();
        let notebook = gtk::Notebook::builder()
            .scrollable(true)
            .show_border(false)
            .build();
        let content = adw::ViewStack::new();
        content.add_named(
            &adw::StatusPage::builder()
                .title("No result selected")
                .description("Run a comparison, or open a saved differences directory.")
                .icon_name("edit-find-symbolic")
                .build(),
            Some("empty"),
        );
        content.add_named(&notebook, Some("tabs"));
        let split = adw::NavigationSplitView::new();
        split.set_sidebar(Some(&adw::NavigationPage::new(&list_scroller, "Packages")));
        split.set_content(Some(&adw::NavigationPage::new(&content, "Differences")));
        split.set_max_sidebar_width(380.0);
        split.set_sidebar_width_fraction(0.28);

        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(
            &run_page,
            Some("run"),
            "Run",
            "media-playback-start-symbolic",
        );
        stack.add_titled_with_icon(&split, Some("results"), "Results", "view-list-symbolic");

        // Window chrome.
        let run_button = button("Run", "suggested-action");
        let cancel_button = button("Cancel", "destructive-action");
        cancel_button.set_visible(false);
        let open_button = gtk::Button::builder()
            .icon_name("document-open-symbolic")
            .tooltip_text("Open a saved differences directory")
            .build();
        let prefs_button = gtk::Button::builder()
            .icon_name("preferences-system-symbolic")
            .tooltip_text("Analysis preferences")
            .build();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(
            &adw::ViewSwitcher::builder()
                .stack(&stack)
                .policy(adw::ViewSwitcherPolicy::Wide)
                .build(),
        ));
        header.pack_start(&run_button);
        header.pack_start(&cancel_button);
        header.pack_end(&prefs_button);
        header.pack_end(&open_button);

        let banner = adw::Banner::new("");
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&banner);
        body.append(&stack);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&body));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toasts));
        window.set_content(Some(&toolbar));

        let token = Rc::new(RefCell::new(None::<CancelToken>));
        let quit_when_finished = Rc::new(Cell::new(false));

        {
            let (form, sender) = (form.clone(), sender.clone());
            run_button.connect_clicked(move |_| sender.input(Msg::Run(form.read())));
        }
        {
            let sender = sender.clone();
            cancel_button.connect_clicked(move |_| sender.input(Msg::Cancel));
        }
        {
            let sender = sender.clone();
            prefs_button.connect_clicked(move |_| sender.input(Msg::OpenPrefs));
        }
        {
            let sender = sender.clone();
            sort_button.connect_clicked(move |_| sender.input(Msg::SortByVerdict));
        }
        {
            let (window, sender) = (window.clone(), sender.clone());
            open_button.connect_clicked(move |_| {
                let sender = sender.clone();
                choose_folder(&window, "Open a differences directory", move |path| {
                    sender.input(Msg::Load(path));
                });
            });
        }
        let list_rows = Rc::new(RefCell::new(Vec::<usize>::new()));
        let same_rows = Rc::new(RefCell::new(Vec::<usize>::new()));
        for (this, other, rows) in [
            (&list, &same_list, &list_rows),
            (&same_list, &list, &same_rows),
        ] {
            let (sender, other, rows) = (sender.clone(), other.clone(), Rc::clone(rows));
            this.connect_row_activated(move |_, row| {
                other.unselect_all();
                let index = usize::try_from(row.index()).ok();
                if let Some(package) = index.and_then(|i| rows.borrow().get(i).copied()) {
                    sender.input(Msg::Select(package));
                }
            });
        }
        {
            let sender = sender.clone();
            adw::StyleManager::default().connect_dark_notify(move |_| {
                sender.input(Msg::ThemeChanged);
            });
        }
        let queue = Queue::default();
        {
            // A run in progress is cancelled first so that no child outlives
            // the window; the window closes when the run has wound down.
            let (token, analysis_token, quit, form) = (
                Rc::clone(&token),
                queue.token(),
                Rc::clone(&quit_when_finished),
                form.clone(),
            );
            window.connect_close_request(move |window| {
                if let Err(err) = form.read().save() {
                    log::warn!("could not save the settings: {err}");
                }
                if let Some(token) = analysis_token.borrow().as_ref() {
                    token.cancel();
                }
                match token.borrow().as_ref() {
                    Some(token) => {
                        token.cancel();
                        quit.set(true);
                        window.set_visible(false);
                        gtk::glib::Propagation::Stop
                    }
                    None => gtk::glib::Propagation::Proceed,
                }
            });
        }

        let model = App {
            form,
            keys: Arc::new(FallbackKeyStore::system()),
            queue,
            missing,
            token,
            quit_when_finished,
            results: RefCell::new(Results::default()),
            selected: None,
            activity: Activity::default(),
            log: RefCell::new(Vec::new()),
            clear_log: Cell::new(false),
            list_dirty: Cell::new(false),
            expand_same: Cell::new(false),
            pane_dirty: Cell::new(false),
            analysis_dirty: Cell::new(false),
            open_prefs: Cell::new(false),
            show_results: Cell::new(false),
            toast: RefCell::new(None),
        };
        let widgets = Widgets {
            window,
            stack,
            banner,
            toasts,
            run_button,
            cancel_button,
            open_button,
            progress,
            status,
            log_buffer,
            log_view,
            list,
            same_list,
            same_expander,
            list_rows,
            same_rows,
            content,
            notebook,
            rendered: None,
            analysis: None,
            queue_label,
            sort_button,
        };
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            Msg::Run(settings) => {
                if self.running() {
                    return;
                }
                let mut cfg = match settings.config() {
                    Ok(cfg) => cfg,
                    Err(problem) => return self.toast(problem),
                };
                cfg.color = false;
                if let Err(err) = settings.save() {
                    log::warn!("could not save the settings: {err}");
                }
                let token = CancelToken::new();
                let ctl = settings.control(token.clone());
                *self.token.borrow_mut() = Some(token);
                self.queue.reset();
                *self.results.borrow_mut() = Results::default();
                self.activity = Activity::default();
                self.selected = None;
                self.clear_log.set(true);
                self.list_dirty.set(true);
                self.pane_dirty.set(true);

                let tx = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let result = run_pipeline(&cfg, &ctl, &mut |event| {
                        tx.emit(Msg::Event(event));
                        Flow::Continue
                    });
                    tx.emit(Msg::Finished(result.map_err(|err| err.to_string())));
                });
            }
            Msg::Cancel => {
                if let Some(token) = self.token.borrow().as_ref() {
                    token.cancel();
                    self.activity.status = "Cancelling…".to_owned();
                }
                self.cancel_analyses();
            }
            Msg::Event(event) => {
                self.activity.apply(&event);
                if let Some(line) = describe(&event) {
                    self.log.borrow_mut().push(line);
                }
                let changed = self.results.borrow_mut().apply(&event);
                if changed {
                    self.list_dirty.set(true);
                    let results = self.results.borrow();
                    if self.selected.is_none() && !results.packages.is_empty() {
                        self.selected = Some(0);
                        self.pane_dirty.set(true);
                    }
                    let shown = self.selected.and_then(|i| results.packages.get(i));
                    if let (Some(shown), Some(attr)) = (shown, event_attr(&event))
                        && shown.attr == attr
                    {
                        self.pane_dirty.set(true);
                    }
                }
                match &event {
                    Event::Saved(_) => self.save_pending(),
                    Event::ToolFinished {
                        attr,
                        tool: Tool::Difflog,
                        status: ToolStatus::Different,
                        ..
                    } => {
                        let analyzer = self.analyzer();
                        let index = self.results.borrow().index_of(attr);
                        if let Some(index) = index
                            && analyzer.auto_analyze
                            && analyzer.auto_timing == AutoTiming::DuringRun
                        {
                            self.request(index, Trigger::Auto, false, &sender);
                        }
                    }
                    _ => {}
                }
            }
            Msg::Finished(result) => {
                *self.token.borrow_mut() = None;
                match result {
                    Ok(report) => {
                        self.activity.status = match report.status {
                            RunStatus::Completed => "Done".to_owned(),
                            RunStatus::NothingChanged => "Nothing changed".to_owned(),
                            RunStatus::DryRun => "Dry run finished".to_owned(),
                            RunStatus::Cancelled => "Cancelled".to_owned(),
                            RunStatus::TimedOut => "Timed out".to_owned(),
                        };
                        if matches!(
                            report.status,
                            RunStatus::Completed | RunStatus::NothingChanged
                        ) {
                            self.activity.fraction = Some(1.0);
                        }
                        if let Some(dir) = &report.saved_to {
                            self.say(Kind::Stage, format!("Results saved in {}", dir.display()));
                        }
                        let have_results = !self.results.borrow().packages.is_empty();
                        if have_results {
                            self.select_first();
                            self.show_results.set(true);
                        }
                        self.save_pending();
                        let analyzer = self.analyzer();
                        if report.status == RunStatus::Completed
                            && analyzer.auto_analyze
                            && analyzer.auto_timing == AutoTiming::AfterRun
                        {
                            self.request_all(&sender);
                        }
                        self.start_next(&sender);
                    }
                    Err(message) => {
                        self.activity.status = "Failed".to_owned();
                        self.say(Kind::Error, message.clone());
                        self.toast(message);
                    }
                }
            }
            Msg::Load(dir) => {
                if self.running() {
                    return self.toast("Wait for the run to finish, or cancel it");
                }
                match Results::load(&dir) {
                    Ok(results) if results.packages.is_empty() => {
                        self.toast(format!("No differences found in {}", dir.display()));
                    }
                    Ok(results) => {
                        self.queue.reset();
                        *self.results.borrow_mut() = results;
                        self.select_first();
                        self.show_results.set(true);
                    }
                    Err(err) => self.toast(format!("Cannot read {}: {err}", dir.display())),
                }
            }
            Msg::Select(index) => {
                self.selected = Some(index);
                self.pane_dirty.set(true);
            }
            Msg::ThemeChanged => self.pane_dirty.set(true),
            Msg::Analyze(index) => self.request(index, Trigger::Manual, false, &sender),
            Msg::AnalyzeAnyway(index) => self.request(index, Trigger::Manual, true, &sender),
            Msg::CancelAnalysis(index) => {
                let attr = self
                    .results
                    .borrow()
                    .packages
                    .get(index)
                    .map(|p| p.attr.clone());
                if let Some(attr) = attr
                    && self.queue.cancel_one(&attr)
                {
                    self.set_analysis(index, State::Idle);
                }
            }
            Msg::Analysis(generation, attr, update) => {
                if !self.queue.is_current(generation, &attr) {
                    return;
                }
                let Some(index) = self.results.borrow().index_of(&attr) else {
                    return;
                };
                match update {
                    Update::Delta { thinking, answer } => {
                        let mut results = self.results.borrow_mut();
                        if let State::Running {
                            thinking: seen,
                            answer: said,
                        } = &mut results.packages[index].analysis
                        {
                            seen.push_str(&thinking);
                            said.push_str(&answer);
                        }
                        self.analysis_dirty.set(true);
                    }
                    Update::Finished(outcome) => {
                        self.queue.finish();
                        let thought = match &self.results.borrow().packages[index].analysis {
                            State::Running { thinking, .. } => thinking.clone(),
                            _ => String::new(),
                        };
                        match outcome {
                            Outcome::Done(result) => {
                                self.save_analysis(&attr, &result);
                                self.results.borrow_mut().packages[index].thinking = thought;
                                self.set_analysis(index, State::Done(result));
                            }
                            Outcome::Cancelled => self.set_analysis(index, State::Idle),
                            Outcome::Failed(message) => {
                                self.set_analysis(index, State::Failed(message));
                            }
                        }
                        self.start_next(&sender);
                    }
                }
            }
            Msg::OpenPrefs => self.open_prefs.set(true),
            Msg::SetAnalyzer(settings) => {
                self.form.set_analyzer(settings);
                if let Err(err) = self.form.read().save() {
                    log::warn!("could not save the settings: {err}");
                }
                self.analysis_dirty.set(true);
            }
            Msg::SortByVerdict => {
                self.results.borrow_mut().sort_by_verdict();
                self.list_dirty.set(true);
            }
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, sender: ComponentSender<Self>) {
        let running = self.running();
        let analysing = self.queue.active();
        widgets.run_button.set_visible(!running);
        widgets.run_button.set_sensitive(self.missing.is_empty());
        widgets.cancel_button.set_visible(running || analysing);
        if running {
            widgets.cancel_button.set_label("Cancel run");
            widgets
                .cancel_button
                .set_tooltip_text(Some("Stop the run and the analyses"));
        } else {
            widgets.cancel_button.set_label("Cancel analysis");
            widgets
                .cancel_button
                .set_tooltip_text(Some("Stop analysing the remaining packages"));
        }
        widgets.open_button.set_sensitive(!running);
        let has_results = !self.results.borrow().packages.is_empty();
        widgets.sort_button.set_sensitive(has_results);
        match self.queue.progress() {
            Some((done, total)) => {
                widgets
                    .queue_label
                    .set_text(&format!("{done} of {total} analysed"));
            }
            None => widgets.queue_label.set_text(""),
        }

        widgets.banner.set_revealed(!self.missing.is_empty());
        if !self.missing.is_empty() {
            widgets.banner.set_title(&format!(
                "Cannot run: {} not found in PATH. Install {} or add {} to PATH, then restart.",
                self.missing.join(", "),
                if self.missing.len() == 1 {
                    "it"
                } else {
                    "them"
                },
                if self.missing.len() == 1 {
                    "it"
                } else {
                    "them"
                },
            ));
        }

        if self.clear_log.take() {
            widgets.log_buffer.set_text("");
        }
        let lines = std::mem::take(&mut *self.log.borrow_mut());
        if !lines.is_empty() {
            panes::append_log(&widgets.log_buffer, &widgets.log_view, &lines);
        }
        let status = match self.queue.progress() {
            Some((done, total)) if !running => format!("Analysing: {done} of {total} done"),
            _ if !running && analysing => "Analysing…".to_owned(),
            _ => self.activity.status.clone(),
        };
        widgets.status.set_text(&status);
        match self.activity.fraction {
            Some(fraction) => widgets.progress.set_fraction(fraction),
            None if running => widgets.progress.pulse(),
            None => widgets.progress.set_fraction(0.0),
        }

        if self.list_dirty.take() {
            for list in [&widgets.list, &widgets.same_list] {
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
            }
            let (mut shown, mut same) = (Vec::new(), Vec::new());
            let results = self.results.borrow();
            for &index in &results.order {
                let package = &results.packages[index];
                let (list, rows) = if package.unchanged() {
                    (&widgets.same_list, &mut same)
                } else {
                    (&widgets.list, &mut shown)
                };
                list.append(&panes::package_row(package));
                rows.push(index);
            }
            let position = |rows: &[usize]| rows.iter().position(|&i| Some(i) == self.selected);
            widgets.same_list.unselect_all();
            widgets.list.unselect_all();
            if let Some(row) = position(&shown).and_then(|i| widgets.list.row_at_index(i as i32)) {
                widgets.list.select_row(Some(&row));
            } else if let Some(row) =
                position(&same).and_then(|i| widgets.same_list.row_at_index(i as i32))
            {
                widgets.same_list.select_row(Some(&row));
            }
            widgets.same_expander.set_visible(!same.is_empty());
            widgets
                .same_expander
                .set_label(Some(&format!("No differences ({})", same.len())));
            if self.expand_same.take() {
                widgets.same_expander.set_expanded(true);
            }
            *widgets.list_rows.borrow_mut() = shown;
            *widgets.same_rows.borrow_mut() = same;
        }

        if self.pane_dirty.take() {
            let keep_tab = (widgets.rendered == self.selected)
                .then(|| widgets.notebook.current_page())
                .flatten();
            while widgets.notebook.n_pages() > 0 {
                widgets.notebook.remove_page(Some(0));
            }
            widgets.rendered = self.selected;
            widgets.analysis = None;

            let mut results = self.results.borrow_mut();
            let diff_bytes = self
                .selected
                .and_then(|i| results.diff_text(i))
                .map(|d| d.bytes);
            let shown = self.selected.and_then(|i| results.show(i));
            match shown {
                Some((package, files)) => {
                    let settings = self.analyzer();
                    for (output, file) in package.outputs.iter().zip(files) {
                        let page = panes::output_page(output, file, &widgets.window);
                        let label = gtk::Label::new(Some(&panes::tab_label(output)));
                        widgets.notebook.append_page(&page, Some(&label));
                        if let (Some(bytes), Some(index), true) = (
                            diff_bytes,
                            self.selected,
                            widgets.analysis.is_none()
                                && output.tool == Tool::Difflog
                                && output.status == crate::results::Status::Different,
                        ) {
                            let pane = AnalysisPane::new(
                                index,
                                sender.input_sender().clone(),
                                &widgets.window,
                            );
                            pane.update(&analysis_view::View {
                                state: &package.analysis,
                                thinking: &package.thinking,
                                settings: &settings,
                                diff_bytes: bytes,
                            });
                            let label = gtk::Label::new(Some("Analysis"));
                            widgets.notebook.append_page(&pane.root, Some(&label));
                            widgets.analysis = Some((pane, bytes));
                        }
                    }
                    if let Some(page) = keep_tab {
                        widgets.notebook.set_current_page(Some(page));
                    }
                    widgets.content.set_visible_child_name("tabs");
                }
                None => widgets.content.set_visible_child_name("empty"),
            }
        }

        if self.analysis_dirty.take()
            && let (Some((pane, bytes)), Some(index)) = (&widgets.analysis, self.selected)
            && let Some(package) = self.results.borrow().packages.get(index)
        {
            pane.update(&analysis_view::View {
                state: &package.analysis,
                thinking: &package.thinking,
                settings: &self.analyzer(),
                diff_bytes: *bytes,
            });
        }

        if self.open_prefs.take() {
            let tx = sender.input_sender().clone();
            prefs::show(
                &widgets.window,
                self.analyzer(),
                Arc::clone(&self.keys),
                move |settings| tx.emit(Msg::SetAnalyzer(settings)),
            );
        }

        if self.show_results.take() {
            widgets.stack.set_visible_child_name("results");
        }
        if let Some(text) = self.toast.borrow_mut().take() {
            widgets.toasts.add_toast(adw::Toast::new(&text));
        }
        if !running && self.quit_when_finished.get() {
            widgets.window.close();
        }
    }
}
