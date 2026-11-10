//! The shown package: its outputs as tabs, the analysis of the selected tab
//! in a pane below them, and the outputs without differences in a menu at the
//! end of the tabs.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use diff_nixpkgs_attr_core::Tool;
use relm4::Sender;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;

use crate::analysis::AnalyzerSettings;
use crate::analysis_view::{self, AnalysisPane};
use crate::app::Msg;
use crate::panes;
use crate::results::{Package, Status, ToolOutput};

/// Where the pane opens, in pixels from the top of the results.
const PANE_POSITION: i32 = 430;

/// What the pane below the tabs shows for one tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Below {
    Analysis,
    Placeholder(&'static str),
}

fn below(output: &ToolOutput, analysable: bool) -> Below {
    match (output.tool, output.status) {
        (Tool::Difflog, Status::Different) if analysable => Below::Analysis,
        (Tool::Difflog, Status::Running) => Below::Placeholder("Waiting for the build log diff"),
        (Tool::Difflog, _) => Below::Placeholder("Nothing to analyse"),
        (Tool::Diffoscope, _) => {
            Below::Placeholder("Only the build log diff can be analysed for now")
        }
    }
}

/// One notebook page, found again by its key when the package is shown
/// again: tabs move as their outputs finish.
struct Tab {
    key: (Tool, String),
    label: String,
    below: Below,
}

/// The pane below the tabs.
#[derive(Clone)]
struct Bottom {
    title: gtk::Label,
    stack: gtk::Stack,
    placeholder: gtk::Label,
}

impl Bottom {
    fn show(&self, tab: Option<&Tab>) {
        self.title.set_text(&match tab {
            Some(tab) => format!("Analysis ({})", tab.label),
            None => "Analysis".to_owned(),
        });
        match tab.map_or(Below::Placeholder("Nothing to analyse"), |t| t.below) {
            Below::Analysis => self.stack.set_visible_child_name("analysis"),
            Below::Placeholder(text) => {
                self.placeholder.set_text(text);
                self.stack.set_visible_child_name("placeholder");
            }
        }
    }
}

pub struct PackageView {
    /// The tabs above, the analysis below.
    pub paned: gtk::Paned,
    /// Shown instead of `paned` when there are no tabs.
    pub no_tabs: adw::StatusPage,
    notebook: gtk::Notebook,
    unchanged: gtk::MenuButton,
    unchanged_list: gtk::ListBox,
    analysis_box: gtk::Box,
    bottom: Bottom,
    tabs: Rc<RefCell<Vec<Tab>>>,
    rendered: Option<usize>,
    /// The Analysis of the shown package and the size of its diff.
    pub analysis: Option<(AnalysisPane, usize)>,
}

impl PackageView {
    pub fn new() -> Self {
        let notebook = gtk::Notebook::builder()
            .scrollable(true)
            .show_border(false)
            .build();
        let unchanged_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let popover = gtk::Popover::builder()
            .child(
                &gtk::ScrolledWindow::builder()
                    .child(&unchanged_list)
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .propagate_natural_height(true)
                    .propagate_natural_width(true)
                    .min_content_width(220)
                    .max_content_height(400)
                    .build(),
            )
            .build();
        let unchanged = gtk::MenuButton::builder()
            .popover(&popover)
            .tooltip_text("Outputs without differences")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .margin_end(6)
            .visible(false)
            .build();
        notebook.set_action_widget(&unchanged, gtk::PackType::End);

        let title = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["heading"])
            .margin_top(8)
            .margin_start(16)
            .margin_end(16)
            .build();
        let placeholder = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .margin_start(16)
            .margin_end(16)
            .build();
        let analysis_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let stack = gtk::Stack::builder().vexpand(true).build();
        stack.add_named(&analysis_box, Some("analysis"));
        stack.add_named(&placeholder, Some("placeholder"));
        let bottom_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bottom_box.set_size_request(-1, 120);
        bottom_box.append(&title);
        bottom_box.append(&stack);
        let bottom = Bottom {
            title,
            stack,
            placeholder,
        };
        bottom.show(None);

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&notebook)
            .end_child(&bottom_box)
            .resize_start_child(true)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .wide_handle(true)
            .position(PANE_POSITION)
            .build();

        let tabs = Rc::new(RefCell::new(Vec::<Tab>::new()));
        {
            let (tabs, bottom) = (Rc::clone(&tabs), bottom.clone());
            notebook.connect_switch_page(move |_, _, page| {
                bottom.show(tabs.borrow().get(page as usize));
            });
        }

        Self {
            paned,
            no_tabs: adw::StatusPage::new(),
            notebook,
            unchanged,
            unchanged_list,
            analysis_box,
            bottom,
            tabs,
            rendered: None,
            analysis: None,
        }
    }

    fn current_key(&self) -> Option<(Tool, String)> {
        let page = self.notebook.current_page()? as usize;
        self.tabs.borrow().get(page).map(|tab| tab.key.clone())
    }

    fn clear(&mut self) {
        // Emptied first: removing pages switches between them.
        self.tabs.borrow_mut().clear();
        while self.notebook.n_pages() > 0 {
            self.notebook.remove_page(Some(0));
        }
        while let Some(child) = self.analysis_box.first_child() {
            self.analysis_box.remove(&child);
        }
        while let Some(child) = self.unchanged_list.first_child() {
            self.unchanged_list.remove(&child);
        }
        self.analysis = None;
    }

    /// Show package `index`, keeping the selected tab when it is the package
    /// shown already. Returns whether there are tabs; when not, `no_tabs`
    /// says why.
    pub fn render(
        &mut self,
        index: usize,
        shown: (&Package, Vec<Option<PathBuf>>),
        diff_bytes: Option<usize>,
        settings: &AnalyzerSettings,
        tx: &Sender<Msg>,
        window: &adw::ApplicationWindow,
    ) -> bool {
        let keep = if self.rendered == Some(index) {
            self.current_key()
        } else {
            None
        };
        self.clear();
        self.rendered = Some(index);
        let (package, files) = shown;

        if let Some(bytes) = diff_bytes {
            let pane = AnalysisPane::new(index, tx.clone(), window);
            pane.update(&analysis_view::View {
                state: &package.analysis,
                thinking: &package.thinking,
                settings,
                diff_bytes: bytes,
            });
            self.analysis_box.append(&pane.root);
            self.analysis = Some((pane, bytes));
        }

        let (order, unchanged) = package.tab_order();
        let mut tabs = Vec::with_capacity(order.len());
        for i in order {
            let output = &package.outputs[i];
            let page = panes::output_page(output, files[i].clone(), window);
            let label = gtk::Label::new(Some(&panes::tab_label(output)));
            self.notebook.append_page(&page, Some(&label));
            tabs.push(Tab {
                key: (output.tool, output.label.clone()),
                label: output.label.clone(),
                below: below(output, self.analysis.is_some()),
            });
        }

        for &i in &unchanged {
            let output = &package.outputs[i];
            self.unchanged_list.append(
                &adw::ActionRow::builder()
                    .title(&output.label)
                    .subtitle(output.status.label())
                    .use_markup(false)
                    .activatable(false)
                    .build(),
            );
        }
        self.unchanged
            .set_label(&format!("{} unchanged", unchanged.len()));
        self.unchanged.set_visible(!unchanged.is_empty());

        let page = keep
            .and_then(|key| tabs.iter().position(|tab| tab.key == key))
            .unwrap_or(0);
        let has_tabs = !tabs.is_empty();
        *self.tabs.borrow_mut() = tabs;
        if has_tabs {
            self.notebook.set_current_page(Some(page as u32));
        }
        // Not left to `switch-page`, which is quiet when the page stays.
        self.bottom.show(self.tabs.borrow().get(page));

        if !has_tabs {
            self.describe_no_tabs(package);
        }
        has_tabs
    }

    fn describe_no_tabs(&self, package: &Package) {
        let page = &self.no_tabs;
        if package.outputs.is_empty() {
            page.set_title("Comparing…");
            page.set_icon_name(Some("content-loading-symbolic"));
            page.set_description(None);
        } else {
            let names: Vec<&str> = package.outputs.iter().map(|o| o.label.as_str()).collect();
            page.set_title("No differences");
            page.set_icon_name(Some("object-select-symbolic"));
            page.set_description(Some(&gtk::glib::markup_escape_text(&format!(
                "Compared: {}",
                names.join(", ")
            ))));
        }
    }

    /// Nothing is shown: the next package starts on its first tab.
    pub fn forget(&mut self) {
        self.clear();
        self.rendered = None;
        self.unchanged.set_visible(false);
        self.bottom.show(None);
    }
}
