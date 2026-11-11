//! The "Analysis" tab of a package: the model's verdict, where it came from,
//! and what to do next. Built once per shown package and updated in place, so
//! streamed text does not rebuild the whole page.

use std::cell::RefCell;

use relm4::Sender;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;

use crate::analysis::{
    AnalysisResult, AnalyzerSettings, SizeCheck, Skipped, State, Trigger, Verdict, human_bytes,
};
use crate::app::Msg;
use crate::panes::Follow;

pub struct View<'a> {
    pub state: &'a State,
    /// What the model thought, for the session.
    pub thinking: &'a str,
    pub settings: &'a AnalyzerSettings,
    pub diff_bytes: usize,
}

pub struct AnalysisPane {
    pub root: gtk::Widget,
    heading: gtk::Label,
    banner: adw::Banner,
    detail: gtk::Label,
    provenance: gtk::Label,
    spinner: gtk::Spinner,
    thinking_box: gtk::Expander,
    thinking: gtk::TextBuffer,
    /// The text in `thinking`, to add only what streamed in since.
    thinking_shown: RefCell<String>,
    thinking_follow: Follow,
    analyze: gtk::Button,
    cancel: gtk::Button,
    anyway: gtk::Button,
    prefs: gtk::Button,
}

fn label(css: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .css_classes(css.to_vec())
        .build()
}

fn button(text: &str, css: &[&str]) -> gtk::Button {
    gtk::Button::builder()
        .label(text)
        .css_classes(css.to_vec())
        .build()
}

impl AnalysisPane {
    pub fn new(index: usize, tx: Sender<Msg>, window: &adw::ApplicationWindow) -> Self {
        let heading = label(&["title-2"]);
        let detail = label(&[]);
        let provenance = label(&["dim-label", "caption"]);
        let banner = adw::Banner::builder().revealed(false).build();
        let spinner = gtk::Spinner::builder().halign(gtk::Align::Start).build();
        let thinking = gtk::TextBuffer::new(None);
        let thinking_view = gtk::TextView::builder()
            .buffer(&thinking)
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .css_classes(["dim-label"])
            .build();
        let thinking_follow = Follow::new(&thinking_view);
        let thinking_box = gtk::Expander::builder()
            .label("Thinking")
            .child(
                &gtk::ScrolledWindow::builder()
                    .child(&thinking_view)
                    .min_content_height(120)
                    .max_content_height(300)
                    .propagate_natural_height(true)
                    .build(),
            )
            .visible(false)
            .build();
        let analyze = button("Analyze", &["suggested-action"]);
        let cancel = button("Cancel", &[]);
        let anyway = button("Send anyway…", &["destructive-action"]);
        let prefs = button("Open preferences", &[]);

        {
            let tx = tx.clone();
            analyze.connect_clicked(move |_| tx.emit(Msg::Analyze(index)));
        }
        {
            let tx = tx.clone();
            cancel.connect_clicked(move |_| tx.emit(Msg::CancelAnalysis(index)));
        }
        {
            let tx = tx.clone();
            prefs.connect_clicked(move |_| tx.emit(Msg::OpenPrefs));
        }
        {
            let window = window.clone();
            anyway.connect_clicked(move |_| {
                let dialog = adw::AlertDialog::new(
                    Some("Send anyway?"),
                    Some(
                        "The diff is over the size limit. The server may silently cut off \
                         the start of it, and the verdict could then be wrong.",
                    ),
                );
                dialog.add_responses(&[("cancel", "Cancel"), ("send", "Send anyway")]);
                dialog.set_response_appearance("send", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                let tx = tx.clone();
                dialog.connect_response(None, move |_, response| {
                    if response == "send" {
                        tx.emit(Msg::AnalyzeAnyway(index));
                    }
                });
                dialog.present(Some(&window));
            });
        }

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for b in [&analyze, &cancel, &anyway, &prefs] {
            buttons.append(b);
        }
        let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(16);
        body.set_margin_end(16);
        for child in [
            heading.upcast_ref::<gtk::Widget>(),
            spinner.upcast_ref(),
            detail.upcast_ref(),
            thinking_box.upcast_ref(),
            provenance.upcast_ref(),
            buttons.upcast_ref(),
        ] {
            body.append(child);
        }
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&banner);
        page.append(
            &gtk::ScrolledWindow::builder()
                .child(&body)
                .vexpand(true)
                .hexpand(true)
                .build(),
        );

        Self {
            root: page.upcast(),
            heading,
            banner,
            detail,
            provenance,
            spinner,
            thinking_box,
            thinking,
            thinking_shown: RefCell::default(),
            thinking_follow,
            analyze,
            cancel,
            anyway,
            prefs,
        }
    }

    pub fn update(&self, view: &View<'_>) {
        let settings = view.settings;
        let size = settings.check_size(view.diff_bytes);

        let mut heading: String;
        let (mut detail, mut banner) = (String::new(), None);
        let mut provenance = String::new();
        let (mut busy, mut waiting) = (false, false);
        let mut css = "";
        match view.state {
            State::Idle => {
                heading = "Not analysed".into();
                detail = if settings.configured() {
                    settings.limit_text()
                } else {
                    format!(
                        "Profile “{}” has no model yet. Choose a server and model in the \
                         preferences.",
                        settings.name
                    )
                };
            }
            State::Queued => {
                heading = "Waiting for its turn".into();
                waiting = true;
            }
            State::Running { answer, .. } => {
                heading = format!("Analysing with {} ({})…", settings.model, settings.name);
                detail = answer.clone();
                busy = true;
            }
            State::Done(result) => {
                let (title, class) = match result.verdict {
                    Verdict::Consequential => ("Consequential differences", "warning"),
                    Verdict::Inconsequential => ("Inconsequential differences only", "success"),
                };
                heading = title.to_owned();
                css = class;
                detail = if result.explanation.is_empty() {
                    "The model reported no explanation.".into()
                } else {
                    result.explanation.clone()
                };
                banner = result.warning.clone();
                provenance = describe(result);
            }
            State::Failed(message) => {
                heading = "The analysis failed".into();
                detail = message.clone();
                css = "error";
            }
            State::TooLarge { bytes, limit } => {
                heading = "Too large to analyse".into();
                detail = format!(
                    "The diff is {} and the limit for this model is {}.",
                    human_bytes(*bytes),
                    human_bytes(*limit)
                );
            }
            State::ContextTooSmall => {
                heading = "No room for the diff".into();
                detail = settings.limit_text();
            }
            State::Skipped(Skipped::CapReached) => {
                heading = "Not analysed".into();
                detail = format!(
                    "The limit of {} automatic analyses per run was reached.",
                    settings.auto_cap
                );
            }
        }
        if matches!(view.state, State::Idle | State::TooLarge { .. }) {
            match size {
                SizeCheck::TooLarge { bytes, limit } if view.state == &State::Idle => {
                    heading = "Too large to analyse".into();
                    detail = format!(
                        "The diff is {} and the limit for this model is {}.",
                        human_bytes(bytes),
                        human_bytes(limit)
                    );
                }
                SizeCheck::ContextTooSmall => detail = settings.limit_text(),
                _ => {}
            }
        }
        if matches!(
            view.state,
            State::Idle | State::TooLarge { .. } | State::ContextTooSmall
        ) && !matches!(size, SizeCheck::Fits)
        {
            detail.push_str("\n\n");
            detail.push_str(&settings.context_advice());
        }

        if !matches!(view.state, State::Done(_)) {
            provenance = profile_line(settings);
        }

        self.heading.set_text(&heading);
        self.heading.set_css_classes(&["title-2", css]);
        self.detail.set_text(&detail);
        self.detail.set_visible(!detail.is_empty());
        self.provenance.set_text(&provenance);
        self.provenance.set_visible(!provenance.is_empty());
        self.banner.set_title(banner.as_deref().unwrap_or(""));
        self.banner.set_revealed(banner.is_some());
        self.spinner.set_spinning(busy);
        self.spinner.set_visible(busy);

        let thinking = match view.state {
            State::Running { thinking, .. } => thinking.as_str(),
            _ => view.thinking,
        };
        let mut shown = self.thinking_shown.borrow_mut();
        if thinking != shown.as_str() {
            // Replacing the text would lose the scroll position: add only
            // what is new when the text has grown.
            let at_end = self.thinking_follow.at_end();
            match thinking.strip_prefix(shown.as_str()) {
                Some(rest) => self.thinking.insert(&mut self.thinking.end_iter(), rest),
                None => self.thinking.set_text(thinking),
            }
            thinking.clone_into(&mut shown);
            if at_end {
                self.thinking_follow.scroll_to_end();
            }
        }
        self.thinking_box.set_visible(!thinking.is_empty());

        let working = busy || waiting;
        let too_large = matches!(size, SizeCheck::TooLarge { .. });
        self.analyze.set_visible(!working);
        self.analyze.set_label(match view.state {
            State::Done(_) => "Analyze again",
            State::Failed(_) => "Retry",
            _ => "Analyze",
        });
        self.analyze
            .set_sensitive(settings.max_diff_bytes().is_some() && !too_large);
        self.cancel.set_visible(working);
        self.anyway.set_visible(!working && too_large);
        self.prefs.set_visible(
            !working
                && (!settings.configured()
                    || matches!(view.state, State::Failed(_) | State::ContextTooSmall)
                    || settings.max_diff_bytes().is_none()),
        );
    }
}

/// Which profile an analysis would use, for every state but a finished one.
fn profile_line(settings: &AnalyzerSettings) -> String {
    let model = if settings.configured() {
        settings.model.as_str()
    } else {
        "no model"
    };
    format!(
        "Profile “{}”: {}, {model}",
        settings.name,
        settings.endpoint.label()
    )
}

fn describe(result: &AnalysisResult) -> String {
    let provenance = &result.provenance;
    let server = match &provenance.host {
        Some(host) => format!("{} at {host}", provenance.endpoint.label()),
        None => provenance.endpoint.label().to_owned(),
    };
    let how = match provenance.trigger {
        Trigger::Manual => "started manually",
        Trigger::Auto => "started automatically",
    };
    format!(
        "{} with {}, {how}, {}",
        server, provenance.model, provenance.timestamp
    )
}
