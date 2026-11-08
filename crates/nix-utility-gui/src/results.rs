//! What a run found, built up from the core's events, and loading a saved
//! `differences-…` directory. Free of any widgets.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use diff_nixpkgs_attr_core::{
    Event, Tool, ToolStatus, analysis_path, difflog_path, diffoscope_path,
};

use crate::analysis::{self, State};

/// How much of one output is kept in memory for display. The whole output is
/// always saved to disk.
pub const DISPLAY_CAP: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Capped {
    bytes: Vec<u8>,
    total: usize,
}

impl Capped {
    pub fn push(&mut self, data: &[u8]) {
        self.total += data.len();
        let room = DISPLAY_CAP.saturating_sub(self.bytes.len());
        self.bytes.extend_from_slice(&data[..data.len().min(room)]);
    }

    pub fn truncated(&self) -> bool {
        self.total > self.bytes.len()
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Same,
    Different,
    Failed(Option<i32>),
    MemoryLimit,
    /// A build output was missing.
    Skipped,
}

impl Status {
    pub fn label(self) -> String {
        match self {
            Status::Running => "running".to_owned(),
            Status::Same => "no differences".to_owned(),
            Status::Different => "differences".to_owned(),
            Status::Failed(Some(code)) => format!("failed (status {code})"),
            Status::Failed(None) => "failed".to_owned(),
            Status::MemoryLimit => "over the memory limit".to_owned(),
            Status::Skipped => "skipped: build output missing".to_owned(),
        }
    }

    pub fn is_problem(self) -> bool {
        matches!(
            self,
            Status::Failed(_) | Status::MemoryLimit | Status::Skipped
        )
    }
}

impl From<ToolStatus> for Status {
    fn from(status: ToolStatus) -> Self {
        match status {
            ToolStatus::Same => Status::Same,
            ToolStatus::Different => Status::Different,
            ToolStatus::Failed(code) => Status::Failed(code),
            ToolStatus::MemoryLimit => Status::MemoryLimit,
        }
    }
}

/// One tab of a package: the build log diff, or one output's diffoscope.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub tool: Tool,
    /// `difflog`, or the output name.
    pub label: String,
    pub status: Status,
    pub text: Capped,
    /// Where it is saved, when it has been.
    pub file: Option<PathBuf>,
    loaded: bool,
}

impl ToolOutput {
    fn new(tool: Tool, label: String, status: Status) -> Self {
        Self {
            tool,
            label,
            status,
            text: Capped::default(),
            file: None,
            loaded: true,
        }
    }

    /// Read the beginning of the saved file, for outputs found on disk.
    pub fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(file) = &self.file else { return };
        let mut buf = Vec::new();
        let total = fs::metadata(file).map_or(0, |m| m.len() as usize);
        if let Ok(f) = File::open(file) {
            let _ = f.take(DISPLAY_CAP as u64).read_to_end(&mut buf);
        }
        self.text = Capped::default();
        self.text.push(&buf);
        self.text.total = total.max(buf.len());
    }
}

#[derive(Debug, Clone)]
pub struct Package {
    pub attr: String,
    pub outputs: Vec<ToolOutput>,
    pub analysis: State,
    /// What the model thought, kept for the session only.
    pub thinking: String,
}

/// The build log diff of a package, as it is sent to the model.
pub struct DiffText {
    pub text: String,
    /// The whole diff, even when only the start is kept for display.
    pub bytes: usize,
}

impl Package {
    pub fn has_differences(&self) -> bool {
        self.outputs.iter().any(|o| o.status == Status::Different)
    }

    /// Compared, and nothing differs and nothing went wrong.
    pub fn unchanged(&self) -> bool {
        !self.outputs.is_empty() && self.outputs.iter().all(|o| o.status == Status::Same)
    }

    pub fn running(&self) -> bool {
        self.outputs.iter().any(|o| o.status == Status::Running)
    }
}

#[derive(Debug, Default)]
pub struct Results {
    pub packages: Vec<Package>,
    pub saved_dir: Option<PathBuf>,
    /// Package indices in the order the sidebar lists them: arrival order
    /// until sorted by verdict, so rows do not jump while analyses arrive.
    pub order: Vec<usize>,
    /// The output that `Event::Output` chunks belong to.
    current: Option<(usize, usize)>,
}

impl Results {
    /// Take in an event. Returns whether the list of packages or one of their
    /// outputs changed in a way a view should show.
    pub fn apply(&mut self, event: &Event) -> bool {
        match event {
            Event::PackageStarting { attr, .. } => {
                self.order.push(self.packages.len());
                self.packages.push(Package {
                    attr: attr.clone(),
                    outputs: Vec::new(),
                    analysis: State::Idle,
                    thinking: String::new(),
                });
                self.current = None;
                true
            }
            Event::ToolStarted {
                tool,
                output: label,
                ..
            } => {
                let Some(index) = self.packages.len().checked_sub(1) else {
                    return false;
                };
                let package = &mut self.packages[index];
                let label = label.clone().unwrap_or_else(|| "difflog".to_owned());
                package
                    .outputs
                    .push(ToolOutput::new(*tool, label, Status::Running));
                self.current = Some((index, package.outputs.len() - 1));
                true
            }
            Event::Output { data, .. } => {
                if let Some(output) = self.current_output() {
                    output.text.push(data);
                }
                false
            }
            Event::ToolFinished { status, .. } => {
                let Some(output) = self.current_output() else {
                    return false;
                };
                output.status = (*status).into();
                self.current = None;
                true
            }
            Event::Skipped { attr, output, .. } => {
                let Some(package) = self.packages.iter_mut().rev().find(|p| &p.attr == attr) else {
                    return false;
                };
                package.outputs.push(ToolOutput::new(
                    Tool::Diffoscope,
                    output.clone(),
                    Status::Skipped,
                ));
                true
            }
            Event::Saved(dir) => {
                self.saved_dir = Some(dir.clone());
                false
            }
            _ => false,
        }
    }

    pub fn index_of(&self, attr: &str) -> Option<usize> {
        self.packages.iter().position(|p| p.attr == attr)
    }

    /// The difflog of package `index` when it has differences to analyze.
    pub fn diff_text(&mut self, index: usize) -> Option<DiffText> {
        let output = self
            .packages
            .get_mut(index)?
            .outputs
            .iter_mut()
            .find(|o| o.tool == Tool::Difflog && o.status == Status::Different)?;
        output.ensure_loaded();
        Some(DiffText {
            text: output.text.text(),
            bytes: output.text.total(),
        })
    }

    /// Consequential packages first, inconsequential last, otherwise in
    /// arrival order.
    pub fn sort_by_verdict(&mut self) {
        let packages = &self.packages;
        self.order
            .sort_by_key(|&i| (packages[i].analysis.sort_rank(), i));
    }

    fn current_output(&mut self) -> Option<&mut ToolOutput> {
        let (p, o) = self.current?;
        self.packages.get_mut(p)?.outputs.get_mut(o)
    }

    /// Read what package `index` shows, and where each of its outputs is
    /// saved (when it has been).
    pub fn show(&mut self, index: usize) -> Option<(&Package, Vec<Option<PathBuf>>)> {
        let dir = self.saved_dir.clone();
        let package = self.packages.get_mut(index)?;
        for output in &mut package.outputs {
            output.ensure_loaded();
        }
        let files = package
            .outputs
            .iter()
            .map(|output| {
                output.file.clone().or_else(|| {
                    let dir = dir.as_deref()?;
                    (output.status == Status::Different)
                        .then(|| saved_file(dir, &package.attr, output))
                })
            })
            .collect();
        Some((package, files))
    }

    /// The packages with differences in a saved `differences-…` directory.
    /// Nothing is read until an output is shown.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let mut attr_dirs: Vec<_> = fs::read_dir(dir)?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .collect();
        attr_dirs.sort_by_key(|e| e.file_name());

        let mut packages = Vec::new();
        for entry in attr_dirs {
            let attr = entry.file_name().to_string_lossy().into_owned();
            let mut outputs = Vec::new();
            let difflog = entry.path().join("difflog");
            if difflog.is_file() {
                outputs.push(saved_output(Tool::Difflog, "difflog", difflog));
            }
            if let Ok(read) = fs::read_dir(entry.path().join("diffoscope")) {
                let mut files: Vec<_> = read
                    .filter_map(Result::ok)
                    .filter(|e| e.path().is_file())
                    .collect();
                files.sort_by_key(|e| e.file_name());
                for file in files {
                    let label = file.file_name().to_string_lossy().into_owned();
                    outputs.push(saved_output(Tool::Diffoscope, &label, file.path()));
                }
            }
            if !outputs.is_empty() {
                let analysis = analysis::load(&analysis_path(dir, &attr))
                    .map_or(State::Idle, |result| State::Done(Box::new(result)));
                packages.push(Package {
                    attr,
                    outputs,
                    analysis,
                    thinking: String::new(),
                });
            }
        }
        Ok(Self {
            order: (0..packages.len()).collect(),
            packages,
            saved_dir: Some(dir.to_owned()),
            current: None,
        })
    }
}

fn saved_file(dir: &Path, attr: &str, output: &ToolOutput) -> PathBuf {
    match output.tool {
        Tool::Difflog => difflog_path(dir, attr),
        Tool::Diffoscope => diffoscope_path(dir, attr, &output.label),
    }
}

fn saved_output(tool: Tool, label: &str, file: PathBuf) -> ToolOutput {
    ToolOutput {
        tool,
        label: label.to_owned(),
        status: Status::Different,
        text: Capped::default(),
        file: Some(file),
        loaded: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(attr: &str) -> Event {
        Event::PackageStarting {
            attr: attr.into(),
            index: 0,
            total: 1,
        }
    }

    fn tool_started(tool: Tool, output: Option<&str>) -> Event {
        Event::ToolStarted {
            attr: "a".into(),
            tool,
            output: output.map(str::to_owned),
        }
    }

    fn finished(tool: Tool, output: Option<&str>, status: ToolStatus) -> Event {
        Event::ToolFinished {
            attr: "a".into(),
            tool,
            output: output.map(str::to_owned),
            status,
        }
    }

    fn output(tool: Tool, text: &str) -> Event {
        Event::Output {
            tool,
            data: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn events_build_packages_with_their_outputs() {
        let mut results = Results::default();
        results.apply(&Event::Saved("/out/differences-a".into()));
        for event in [
            started("a"),
            tool_started(Tool::Difflog, None),
            output(Tool::Difflog, "+x\n"),
            finished(Tool::Difflog, None, ToolStatus::Different),
            tool_started(Tool::Diffoscope, Some("out")),
            output(Tool::Diffoscope, "one "),
            output(Tool::Diffoscope, "two"),
            finished(Tool::Diffoscope, Some("out"), ToolStatus::Same),
            Event::Skipped {
                attr: "a".into(),
                output: "dev".into(),
                old_path: "o".into(),
                new_path: "n".into(),
            },
        ] {
            results.apply(&event);
        }
        let package = &results.packages[0];
        assert_eq!(package.attr, "a");
        let labels: Vec<_> = package.outputs.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["difflog", "out", "dev"]);
        assert_eq!(package.outputs[0].text.text(), "+x\n");
        assert_eq!(package.outputs[1].text.text(), "one two");
        assert_eq!(package.outputs[1].status, Status::Same);
        assert_eq!(package.outputs[2].status, Status::Skipped);
        assert!(package.has_differences() && !package.running());
        let (_, files) = results.show(0).unwrap();
        assert_eq!(
            files,
            [Some("/out/differences-a/a/difflog".into()), None, None]
        );
    }

    #[test]
    fn unchanged_needs_every_output_to_be_the_same() {
        let mut results = Results::default();
        results.apply(&started("a"));
        assert!(!results.packages[0].unchanged(), "nothing compared yet");
        results.apply(&tool_started(Tool::Difflog, None));
        assert!(!results.packages[0].unchanged(), "still running");
        results.apply(&finished(Tool::Difflog, None, ToolStatus::Same));
        assert!(results.packages[0].unchanged());
        results.apply(&tool_started(Tool::Diffoscope, Some("out")));
        results.apply(&finished(
            Tool::Diffoscope,
            Some("out"),
            ToolStatus::Failed(None),
        ));
        assert!(!results.packages[0].unchanged());
    }

    #[test]
    fn running_until_finished() {
        let mut results = Results::default();
        results.apply(&started("a"));
        results.apply(&tool_started(Tool::Difflog, None));
        assert!(results.packages[0].running());
        results.apply(&finished(Tool::Difflog, None, ToolStatus::MemoryLimit));
        assert!(!results.packages[0].running());
        assert_eq!(results.packages[0].outputs[0].status, Status::MemoryLimit);
    }

    #[test]
    fn display_is_capped_but_the_size_is_known() {
        let mut capped = Capped::default();
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..5 {
            capped.push(&chunk);
        }
        assert!(capped.truncated());
        assert_eq!(capped.total(), 5 * 1024 * 1024);
        assert_eq!(capped.text().len(), DISPLAY_CAP);
    }

    #[test]
    fn loads_a_saved_directory_lazily() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("differences-x");
        fs::create_dir_all(dir.join("set.b/diffoscope")).unwrap();
        fs::create_dir_all(dir.join("set.a/diffoscope")).unwrap();
        fs::write(dir.join("set.a/difflog"), "-old\n+new\n").unwrap();
        fs::write(dir.join("set.a/diffoscope/out"), "out diff").unwrap();
        fs::write(dir.join("set.a/diffoscope/dev"), "dev diff").unwrap();
        fs::write(dir.join("stray-file"), "").unwrap();

        let mut results = Results::load(&dir).unwrap();
        let attrs: Vec<_> = results.packages.iter().map(|p| p.attr.as_str()).collect();
        assert_eq!(attrs, ["set.a"], "empty and non-directories are left out");
        let labels: Vec<_> = results.packages[0]
            .outputs
            .iter()
            .map(|o| o.label.as_str())
            .collect();
        assert_eq!(labels, ["difflog", "dev", "out"]);
        let output = &mut results.packages[0].outputs[0];
        assert_eq!(output.text.total(), 0);
        output.ensure_loaded();
        assert_eq!(output.text.text(), "-old\n+new\n");
        assert_eq!(results.saved_dir.as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn loading_a_missing_directory_fails() {
        assert!(Results::load(Path::new("/nonexistent/differences")).is_err());
    }

    fn done(verdict: analysis::Verdict, unreliable: bool) -> State {
        State::Done(Box::new(analysis::AnalysisResult {
            verdict,
            explanation: String::new(),
            provenance: analysis::Provenance {
                endpoint: analysis::Endpoint::Ollama,
                model: "m".into(),
                host: None,
                trigger: analysis::Trigger::Manual,
                timestamp: "2026-01-01T00:00:00Z".into(),
            },
            prompt_tokens: None,
            warning: None,
            unreliable,
        }))
    }

    #[test]
    fn sorting_by_verdict_is_stable_and_only_when_asked() {
        use analysis::Verdict::{Consequential, Inconsequential};
        let mut results = Results::default();
        for attr in ["a", "b", "c", "d", "e"] {
            results.apply(&started(attr));
        }
        results.packages[0].analysis = done(Inconsequential, false);
        results.packages[1].analysis = State::Failed("x".into());
        results.packages[2].analysis = done(Consequential, false);
        results.packages[3].analysis = done(Inconsequential, true);
        results.packages[4].analysis = done(Consequential, true);
        assert_eq!(results.order, [0, 1, 2, 3, 4], "arrival order until sorted");
        results.sort_by_verdict();
        assert_eq!(results.order, [2, 4, 1, 3, 0]);
    }

    #[test]
    fn diff_text_is_the_difflog_with_differences() {
        let mut results = Results::default();
        results.apply(&started("a"));
        results.apply(&tool_started(Tool::Difflog, None));
        results.apply(&output(Tool::Difflog, "-x\n+y\n"));
        assert!(results.diff_text(0).is_none(), "still running");
        results.apply(&finished(Tool::Difflog, None, ToolStatus::Different));
        let diff = results.diff_text(0).unwrap();
        assert_eq!((diff.text.as_str(), diff.bytes), ("-x\n+y\n", 6));
        assert!(results.diff_text(1).is_none());
    }

    #[test]
    fn a_saved_analysis_is_shown_again_when_a_directory_is_opened() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::write(dir.join("a/difflog"), "+x\n").unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::write(dir.join("b/difflog"), "+x\n").unwrap();
        let State::Done(result) = done(analysis::Verdict::Consequential, false) else {
            unreachable!()
        };
        analysis::save(&analysis_path(dir, "a"), &result, false).unwrap();
        fs::write(dir.join("b/analysis"), "not json").unwrap();

        let results = Results::load(dir).unwrap();
        assert_eq!(results.packages[0].analysis.sort_rank(), 0);
        assert_eq!(results.packages[1].analysis, State::Idle);
    }
}
