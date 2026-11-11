//! Building the views that show results.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use diff_nixpkgs_attr_core::Tool;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::gtk::glib::translate::IntoGlib;
use relm4::gtk::{gio, glib, pango};
use sourceview5::prelude::*;

use crate::activity::{Kind, Line};
use crate::analysis::{Skipped, State, Verdict};
use crate::ansi::{self, Style};
use crate::results::{DISPLAY_CAP, Package, Status, ToolOutput};

/// Lines kept in the run log.
pub const LOG_LINES: i32 = 5000;
/// Lines kept in the build log; builders can be chatty.
pub const BUILD_LOG_LINES: i32 = 3000;

pub fn is_dark() -> bool {
    adw::StyleManager::default().is_dark()
}

pub fn open_file(window: &adw::ApplicationWindow, path: &Path) {
    let file = gio::File::for_path(path);
    gtk::FileLauncher::new(Some(&file)).launch(Some(window), gio::Cancellable::NONE, |result| {
        if let Err(err) = result {
            log::warn!("could not open the file: {err}");
        }
    });
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(child)
        .hexpand(true)
        .vexpand(true)
        .build()
}

fn text_view(buffer: &impl IsA<gtk::TextBuffer>) -> gtk::TextView {
    gtk::TextView::builder()
        .buffer(buffer)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .left_margin(8)
        .right_margin(8)
        .top_margin(6)
        .bottom_margin(6)
        .build()
}

/// A unified diff, highlighted by GtkSourceView.
fn diff_view(text: &str) -> gtk::Widget {
    let buffer = sourceview5::Buffer::new(None);
    buffer.set_language(
        sourceview5::LanguageManager::default()
            .language("diff")
            .as_ref(),
    );
    buffer.set_highlight_syntax(true);
    let scheme = if is_dark() { "Adwaita-dark" } else { "Adwaita" };
    buffer.set_style_scheme(
        sourceview5::StyleSchemeManager::default()
            .scheme(scheme)
            .as_ref(),
    );
    buffer.set_text(text);
    let view = sourceview5::View::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_left_margin(8);
    scrolled(&view).upcast()
}

fn tag_for(buffer: &gtk::TextBuffer, name: &str, props: &[(&str, &dyn ToValue)]) -> gtk::TextTag {
    let table = buffer.tag_table();
    table
        .lookup(name)
        .or_else(|| buffer.create_tag(Some(name), props))
        .expect("tag creation cannot fail")
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn style_tags(buffer: &gtk::TextBuffer, style: Style) -> Vec<gtk::TextTag> {
    let mut tags = Vec::new();
    if style.dim && style.fg.is_none() {
        tags.push(tag_for(buffer, "dim", &[("foreground", &"#888888")]));
    }
    if let Some(color) = style.fg {
        let value = hex(color.rgb());
        tags.push(tag_for(
            buffer,
            &format!("fg{value}"),
            &[("foreground", &value)],
        ));
    }
    if let Some(color) = style.bg {
        let value = hex(color.rgb());
        tags.push(tag_for(
            buffer,
            &format!("bg{value}"),
            &[("background", &value)],
        ));
    }
    if style.bold {
        tags.push(tag_for(
            buffer,
            "bold",
            &[("weight", &(pango::Weight::Bold.into_glib()))],
        ));
    }
    if style.italic {
        tags.push(tag_for(
            buffer,
            "italic",
            &[("style", &pango::Style::Italic)],
        ));
    }
    if style.underline {
        tags.push(tag_for(
            buffer,
            "underline",
            &[("underline", &pango::Underline::Single)],
        ));
    }
    tags
}

/// Diffoscope's coloured text: escape sequences become text tags.
fn ansi_view(text: &str) -> gtk::Widget {
    let buffer = gtk::TextBuffer::new(None);
    let spans = ansi::parse(text);
    let plain: String = spans.iter().map(|s| s.text.as_str()).collect();
    buffer.set_text(&plain);
    let mut offset = 0;
    for span in &spans {
        let len = span.text.chars().count() as i32;
        if span.style != Style::default() {
            let (start, end) = (
                buffer.iter_at_offset(offset),
                buffer.iter_at_offset(offset + len),
            );
            for tag in style_tags(&buffer, span.style) {
                buffer.apply_tag(&tag, &start, &end);
            }
        }
        offset += len;
    }
    scrolled(&text_view(&buffer)).upcast()
}

pub fn status_page(title: &str, icon: &str, spinner: bool) -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .title(title)
        .icon_name(icon)
        .build();
    if spinner {
        page.set_child(Some(&gtk::Spinner::builder().spinning(true).build()));
    }
    page.upcast()
}

/// The tab for one output of a package.
pub fn output_page(
    output: &ToolOutput,
    file: Option<PathBuf>,
    window: &adw::ApplicationWindow,
) -> gtk::Widget {
    if output.status != Status::Different {
        return match output.status {
            Status::Running => status_page("Running…", "content-loading-symbolic", true),
            other => {
                let icon = if other.is_problem() {
                    "dialog-warning-symbolic"
                } else {
                    "emblem-ok-symbolic"
                };
                status_page(&other.label(), icon, false)
            }
        };
    }

    let text = output.text.text();
    let body = match output.tool {
        Tool::Difflog => diff_view(&text),
        Tool::Diffoscope => ansi_view(&text),
    };
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    if output.text.truncated() {
        let banner = adw::Banner::new(&format!(
            "Showing the first {} MiB of {} MiB",
            DISPLAY_CAP / (1024 * 1024),
            output.text.total().div_ceil(1024 * 1024)
        ));
        if let Some(file) = file {
            banner.set_button_label(Some("Open full output"));
            let window = window.clone();
            banner.connect_button_clicked(move |_| open_file(&window, &file));
        }
        banner.set_revealed(true);
        page.append(&banner);
    }
    page.append(&body);
    page.upcast()
}

pub fn tab_label(output: &ToolOutput) -> String {
    if output.status.is_problem() {
        format!("{} ⚠", output.label)
    } else {
        output.label.clone()
    }
}

/// A row of the package list.
pub fn package_row(package: &Package) -> adw::ActionRow {
    let different = package
        .outputs
        .iter()
        .filter(|o| o.status == Status::Different)
        .count();
    let problems = package
        .outputs
        .iter()
        .filter(|o| o.status.is_problem())
        .count();
    let mut parts = Vec::new();
    if package.running() {
        parts.push("comparing…".to_owned());
    }
    if different > 0 {
        parts.push(format!("{different} with differences"));
    }
    if problems > 0 {
        parts.push(format!("{problems} with problems"));
    }
    if parts.is_empty() {
        parts.push("no differences".to_owned());
    }
    let row = adw::ActionRow::builder()
        .title(glib_escape(&package.attr))
        .subtitle(parts.join(", "))
        .activatable(true)
        .build();
    if package.running() {
        row.add_prefix(&gtk::Spinner::builder().spinning(true).build());
    } else if problems > 0 {
        row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    }
    add_verdict(&row, &package.analysis);
    row
}

/// The analysis verdict, as a badge on the row.
fn add_verdict(row: &adw::ActionRow, state: &State) {
    let (text, css): (String, &str) = match state {
        State::Done(result) => {
            let doubt = if result.unreliable { " ?" } else { "" };
            match result.verdict {
                Verdict::Consequential => (format!("consequential{doubt}"), "warning"),
                Verdict::Inconsequential => (format!("inconsequential{doubt}"), "success"),
            }
        }
        State::Queued => ("queued".to_owned(), "dim-label"),
        State::Running { .. } => ("analysing…".to_owned(), "dim-label"),
        State::Failed(_) => ("analysis failed".to_owned(), "error"),
        State::TooLarge { .. } => ("too large".to_owned(), "dim-label"),
        State::Skipped(Skipped::CapReached) => ("not analysed".to_owned(), "dim-label"),
        State::Idle | State::ContextTooSmall => return,
    };
    row.add_suffix(
        &gtk::Label::builder()
            .label(text)
            .css_classes([css, "caption"])
            .valign(gtk::Align::Center)
            .build(),
    );
}

/// Titles are Pango markup; attributes are plain text.
fn glib_escape(text: &str) -> String {
    relm4::gtk::glib::markup_escape_text(text).into()
}

fn log_tag(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Stage => Some("stage"),
        Kind::Note => Some("note"),
        Kind::Warn => Some("warn"),
        Kind::Error => Some("error"),
        Kind::Plain => None,
    }
}

pub fn log_buffer() -> gtk::TextBuffer {
    let buffer = gtk::TextBuffer::new(None);
    tag_for(
        &buffer,
        "stage",
        &[("weight", &pango::Weight::Bold.into_glib())],
    );
    tag_for(&buffer, "note", &[("foreground", &"#888888")]);
    tag_for(&buffer, "warn", &[("foreground", &"#e5a50a")]);
    tag_for(&buffer, "error", &[("foreground", &"#e01b24")]);
    buffer
}

/// Keeps a text view at its end as text is added, unless the user has
/// scrolled up to read something.
#[derive(Clone)]
pub struct Follow {
    view: gtk::TextView,
    /// A scroll to the end is waiting for the new text to be laid out.
    pending: Rc<Cell<bool>>,
}

impl Follow {
    pub fn new(view: &gtk::TextView) -> Self {
        Self {
            view: view.clone(),
            pending: Rc::default(),
        }
    }

    /// Whether the view shows its end. Ask before adding text: afterwards
    /// the end has moved.
    pub fn at_end(&self) -> bool {
        self.pending.get()
            || self
                .view
                .vadjustment()
                .is_none_or(|a| a.value() >= a.upper() - a.page_size() - 1.0)
    }

    /// Scroll to the end once new text is laid out: scrolling right away uses
    /// the old line heights and stops short of the end.
    pub fn scroll_to_end(&self) {
        if self.pending.replace(true) {
            return;
        }
        let this = self.clone();
        glib::idle_add_local_full(glib::Priority::LOW, move || {
            this.pending.set(false);
            if let Some(adjustment) = this.view.vadjustment() {
                adjustment.set_value(adjustment.upper() - adjustment.page_size());
            }
            glib::ControlFlow::Break
        });
    }
}

pub fn log_view(buffer: &gtk::TextBuffer) -> (gtk::Widget, Follow) {
    let view = text_view(buffer);
    (scrolled(&view).upcast(), Follow::new(&view))
}

/// Append to a log, keeping at most `keep` lines. The view follows the end
/// only if it was showing it.
pub fn append_log(buffer: &gtk::TextBuffer, follow: &Follow, lines: &[Line], keep: i32) {
    let at_end = follow.at_end();
    for line in lines {
        let start = buffer.end_iter().offset();
        buffer.insert(&mut buffer.end_iter(), &format!("{}\n", line.text));
        if let Some(tag) = log_tag(line.kind) {
            buffer.apply_tag_by_name(tag, &buffer.iter_at_offset(start), &buffer.end_iter());
        }
    }
    let excess = buffer.line_count() - keep;
    if excess > 0
        && let Some(mut to) = buffer.iter_at_line(excess)
    {
        buffer.delete(&mut buffer.start_iter(), &mut to);
    }
    if at_end {
        follow.scroll_to_end();
    }
}
