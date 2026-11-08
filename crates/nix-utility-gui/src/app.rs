//! The application window and how it reacts to a run.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use diff_nixpkgs_attr_core::tools::Preflight;
use diff_nixpkgs_attr_core::{CancelToken, Event, Flow, Report, Status as RunStatus, run_pipeline};
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::activity::{Activity, Kind, Line, describe};
use crate::form::{Form, choose_folder};
use crate::panes;
use crate::results::Results;
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
}

pub struct App {
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
    pane_dirty: Cell<bool>,
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
    content: adw::ViewStack,
    notebook: gtk::Notebook,
    rendered: Option<usize>,
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
        self.list_dirty.set(true);
        self.pane_dirty.set(true);
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
        let list_scroller = gtk::ScrolledWindow::builder().child(&list).build();
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
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(
            &adw::ViewSwitcher::builder()
                .stack(&stack)
                .policy(adw::ViewSwitcherPolicy::Wide)
                .build(),
        ));
        header.pack_start(&run_button);
        header.pack_start(&cancel_button);
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
            let (window, sender) = (window.clone(), sender.clone());
            open_button.connect_clicked(move |_| {
                let sender = sender.clone();
                choose_folder(&window, "Open a differences directory", move |path| {
                    sender.input(Msg::Load(path));
                });
            });
        }
        {
            let sender = sender.clone();
            list.connect_row_activated(move |_, row| {
                sender.input(Msg::Select(row.index().max(0) as usize));
            });
        }
        {
            let sender = sender.clone();
            adw::StyleManager::default().connect_dark_notify(move |_| {
                sender.input(Msg::ThemeChanged);
            });
        }
        {
            // A run in progress is cancelled first so that no child outlives
            // the window; the window closes when the run has wound down.
            let (token, quit, form) = (
                Rc::clone(&token),
                Rc::clone(&quit_when_finished),
                form.clone(),
            );
            window.connect_close_request(move |window| {
                if let Err(err) = form.read().save() {
                    log::warn!("could not save the settings: {err}");
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
            missing,
            token,
            quit_when_finished,
            results: RefCell::new(Results::default()),
            selected: None,
            activity: Activity::default(),
            log: RefCell::new(Vec::new()),
            clear_log: Cell::new(false),
            list_dirty: Cell::new(false),
            pane_dirty: Cell::new(false),
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
            content,
            notebook,
            rendered: None,
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
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: ComponentSender<Self>) {
        let running = self.running();
        widgets.run_button.set_visible(!running);
        widgets.run_button.set_sensitive(self.missing.is_empty());
        widgets.cancel_button.set_visible(running);
        widgets.open_button.set_sensitive(!running);

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
        widgets.status.set_text(&self.activity.status);
        match self.activity.fraction {
            Some(fraction) => widgets.progress.set_fraction(fraction),
            None if running => widgets.progress.pulse(),
            None => widgets.progress.set_fraction(0.0),
        }

        if self.list_dirty.take() {
            while let Some(child) = widgets.list.first_child() {
                widgets.list.remove(&child);
            }
            for package in &self.results.borrow().packages {
                widgets.list.append(&panes::package_row(package));
            }
            let row = self
                .selected
                .and_then(|i| widgets.list.row_at_index(i as i32));
            widgets.list.select_row(row.as_ref());
        }

        if self.pane_dirty.take() {
            let keep_tab = (widgets.rendered == self.selected)
                .then(|| widgets.notebook.current_page())
                .flatten();
            while widgets.notebook.n_pages() > 0 {
                widgets.notebook.remove_page(Some(0));
            }
            widgets.rendered = self.selected;

            let mut results = self.results.borrow_mut();
            let shown = self.selected.and_then(|i| results.show(i));
            match shown {
                Some((package, files)) => {
                    for (output, file) in package.outputs.iter().zip(files) {
                        let page = panes::output_page(output, file, &widgets.window);
                        let label = gtk::Label::new(Some(&panes::tab_label(output)));
                        widgets.notebook.append_page(&page, Some(&label));
                    }
                    if let Some(page) = keep_tab {
                        widgets.notebook.set_current_page(Some(page));
                    }
                    widgets.content.set_visible_child_name("tabs");
                }
                None => widgets.content.set_visible_child_name("empty"),
            }
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
