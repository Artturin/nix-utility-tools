//! What a run found, built up from the core's events, and loading a saved
//! `differences-…` directory. Free of any widgets.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use diff_nixpkgs_attr_core::{Event, Tool, ToolStatus, difflog_path, diffoscope_path};

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
}

impl Package {
    pub fn has_differences(&self) -> bool {
        self.outputs.iter().any(|o| o.status == Status::Different)
    }

    pub fn running(&self) -> bool {
        self.outputs.iter().any(|o| o.status == Status::Running)
    }
}

#[derive(Debug, Default)]
pub struct Results {
    pub packages: Vec<Package>,
    pub saved_dir: Option<PathBuf>,
    /// The output that `Event::Output` chunks belong to.
    current: Option<(usize, usize)>,
}

impl Results {
    /// Take in an event. Returns whether the list of packages or one of their
    /// outputs changed in a way a view should show.
    pub fn apply(&mut self, event: &Event) -> bool {
        match event {
            Event::PackageStarting { attr, .. } => {
                self.packages.push(Package {
                    attr: attr.clone(),
                    outputs: Vec::new(),
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
                packages.push(Package { attr, outputs });
            }
        }
        Ok(Self {
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
}
