//! What the LLM analysis of a build-log diff is configured as, how big a diff
//! may be, what its result looks like, and how that is saved. Free of any
//! widgets.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use nix_log_diff_analyzer::llm::LlmEndpoint;
use nix_log_diff_analyzer::parse::Verdict as ModelVerdict;
use nix_log_diff_analyzer::usage::PromptCheck;
use serde::{Deserialize, Serialize};

/// Tokens the prompt template takes before the diff is added.
const TEMPLATE_TOKENS: u32 = 800;
/// What the answer may take when no token budget is configured.
const DEFAULT_RESERVE_TOKENS: u32 = 2048;
/// Bytes of diff per token. Build-log text measured about 2.6 against ollama;
/// the smaller figure keeps the limit on the safe side.
const BYTES_PER_TOKEN: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Endpoint {
    Ollama,
    LlamaCpp,
    Openai,
}

impl Endpoint {
    pub const ALL: [Endpoint; 3] = [Endpoint::Ollama, Endpoint::LlamaCpp, Endpoint::Openai];

    pub fn label(self) -> &'static str {
        match self {
            Endpoint::Ollama => "ollama",
            Endpoint::LlamaCpp => "llama.cpp",
            Endpoint::Openai => "OpenAI-compatible",
        }
    }

    /// The name used in saved files.
    pub fn id(self) -> &'static str {
        match self {
            Endpoint::Ollama => "ollama",
            Endpoint::LlamaCpp => "llama-cpp",
            Endpoint::Openai => "openai",
        }
    }

    /// The context to assume when the user has not said. Never optimistic for
    /// local servers: too low only makes the limit stricter, too high lets
    /// the server silently cut the prompt. ollama's own default is 4096 on
    /// machines with less than 24 GiB of VRAM.
    pub fn default_context(self) -> u32 {
        match self {
            Endpoint::Ollama | Endpoint::LlamaCpp => 4096,
            Endpoint::Openai => 32768,
        }
    }

    pub fn llm(self) -> LlmEndpoint {
        match self {
            Endpoint::Ollama => LlmEndpoint::Ollama,
            Endpoint::LlamaCpp => LlmEndpoint::LlamaCpp,
            Endpoint::Openai => LlmEndpoint::Openai,
        }
    }

    /// How the user raises the context of the server itself.
    pub fn context_hint(self) -> &'static str {
        match self {
            Endpoint::Ollama => "OLLAMA_CONTEXT_LENGTH",
            Endpoint::LlamaCpp => "llama-server -c",
            Endpoint::Openai => "the provider's context window",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutoTiming {
    /// One package at a time once the whole run has finished.
    AfterRun,
    /// As each build log diff finishes, while the run goes on.
    DuringRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalyzerSettings {
    /// What the profile is called; unique among the profiles.
    pub name: String,
    pub endpoint: Endpoint,
    /// Empty: the endpoint's default address.
    pub host: String,
    /// Empty: not configured yet.
    pub model: String,
    /// What the server's context is set to; 0 for the endpoint's default.
    pub context_tokens: u32,
    /// Stop a model that thinks for longer; 0 for no limit.
    pub max_thinking_tokens: u32,
    /// Cut the answer off after this many tokens; 0 for no limit.
    pub max_output_tokens: u32,
    /// Seconds of silence before giving up; 0 for never.
    pub idle_timeout_secs: u64,
    /// Analyze every changed build log diff without being asked.
    pub auto_analyze: bool,
    pub auto_timing: AutoTiming,
    /// At most this many packages per run are analyzed automatically; 0 for
    /// no cap.
    pub auto_cap: u32,
}

impl Default for AnalyzerSettings {
    fn default() -> Self {
        Self {
            name: DEFAULT_PROFILE.to_owned(),
            endpoint: Endpoint::Ollama,
            host: String::new(),
            model: String::new(),
            context_tokens: 0,
            max_thinking_tokens: 0,
            max_output_tokens: 0,
            idle_timeout_secs: 600,
            auto_analyze: false,
            auto_timing: AutoTiming::AfterRun,
            auto_cap: 25,
        }
    }
}

const DEFAULT_PROFILE: &str = "Default";

/// The analysis profiles, which one is used, and what is shared by all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalysisPrefs {
    /// The name of the profile in use.
    pub active: String,
    /// Write the host into saved analyses. Off: a results directory may be
    /// shared, and an internal host name would leak.
    pub save_host: bool,
    pub profiles: Vec<AnalyzerSettings>,
}

impl Default for AnalysisPrefs {
    fn default() -> Self {
        Self {
            active: DEFAULT_PROFILE.to_owned(),
            save_host: false,
            profiles: vec![AnalyzerSettings::default()],
        }
    }
}

impl AnalysisPrefs {
    fn position(&self, name: &str) -> Option<usize> {
        self.profiles.iter().position(|p| p.name == name)
    }

    fn active_index(&self) -> usize {
        self.position(&self.active).unwrap_or(0)
    }

    /// The profile in use. There is always one after [`Self::normalize`].
    pub fn active(&self) -> &AnalyzerSettings {
        &self.profiles[self.active_index()]
    }

    pub fn active_mut(&mut self) -> &mut AnalyzerSettings {
        let index = self.active_index();
        &mut self.profiles[index]
    }

    pub fn names(&self) -> Vec<String> {
        self.profiles.iter().map(|p| p.name.clone()).collect()
    }

    /// Use the profile called `name`; false when there is none.
    pub fn select(&mut self, name: &str) -> bool {
        let known = self.position(name).is_some();
        if known {
            self.active = name.to_owned();
        }
        known
    }

    /// `base`, or `base 2`, `base 3`… whichever is free.
    fn unique_name(&self, base: &str) -> String {
        let base = base.trim();
        let base = if base.is_empty() {
            DEFAULT_PROFILE
        } else {
            base
        };
        if self.position(base).is_none() {
            return base.to_owned();
        }
        (2..)
            .map(|n| format!("{base} {n}"))
            .find(|name| self.position(name).is_none())
            .expect("an unused name")
    }

    /// Copy the active profile under a new name and use the copy.
    pub fn add_copy(&mut self) -> String {
        let mut copy = self.active().clone();
        copy.name = self.unique_name(&copy.name);
        self.active = copy.name.clone();
        self.profiles.push(copy);
        self.active.clone()
    }

    /// Delete the active profile and use its neighbour; never the last one.
    pub fn remove_active(&mut self) -> bool {
        if self.profiles.len() <= 1 {
            return false;
        }
        let index = self.active_index();
        self.profiles.remove(index);
        self.active = self.profiles[index.min(self.profiles.len() - 1)]
            .name
            .clone();
        true
    }

    pub fn rename_active(&mut self, name: &str) -> Result<(), &'static str> {
        let name = name.trim();
        if name.is_empty() {
            return Err("A profile needs a name");
        }
        if self
            .position(name)
            .is_some_and(|i| i != self.active_index())
        {
            return Err("Another profile has this name");
        }
        self.active_mut().name = name.to_owned();
        self.active = name.to_owned();
        Ok(())
    }

    /// Make a hand-edited or empty list usable: at least one profile, unique
    /// non-empty names, and an active profile that exists.
    pub fn normalize(&mut self) {
        if self.profiles.is_empty() {
            self.profiles.push(AnalyzerSettings::default());
        }
        let profiles = std::mem::take(&mut self.profiles);
        for mut profile in profiles {
            profile.name = self.unique_name(&profile.name);
            self.profiles.push(profile);
        }
        if self.position(&self.active).is_none() {
            self.active = self.profiles[0].name.clone();
        }
    }
}

/// Whether a diff may be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeCheck {
    Fits,
    TooLarge {
        bytes: usize,
        limit: usize,
    },
    /// The context cannot hold the template and the answer budget at all.
    ContextTooSmall,
}

impl AnalyzerSettings {
    pub fn configured(&self) -> bool {
        !self.model.trim().is_empty()
    }

    pub fn context(&self) -> u32 {
        if self.context_tokens == 0 {
            self.endpoint.default_context()
        } else {
            self.context_tokens
        }
    }

    /// Tokens kept free for the answer: the larger of the thinking and output
    /// budgets, since a reasoning model's thinking counts against the context.
    pub fn answer_reserve(&self) -> u32 {
        match self.max_thinking_tokens.max(self.max_output_tokens) {
            0 => DEFAULT_RESERVE_TOKENS,
            reserve => reserve,
        }
    }

    /// The most diff bytes that fit, or `None` when nothing does.
    pub fn max_diff_bytes(&self) -> Option<usize> {
        let room = i64::from(self.context())
            - i64::from(TEMPLATE_TOKENS)
            - i64::from(self.answer_reserve());
        u64::try_from(room)
            .ok()
            .filter(|&room| room > 0)
            .map(|room| (room * BYTES_PER_TOKEN) as usize)
    }

    pub fn check_size(&self, diff_bytes: usize) -> SizeCheck {
        match self.max_diff_bytes() {
            None => SizeCheck::ContextTooSmall,
            Some(limit) if diff_bytes > limit => SizeCheck::TooLarge {
                bytes: diff_bytes,
                limit,
            },
            Some(_) => SizeCheck::Fits,
        }
    }

    /// The limit in words, for the preferences and the Analysis tab.
    pub fn limit_text(&self) -> String {
        match self.max_diff_bytes() {
            Some(limit) => format!("Diffs up to {} can be analyzed.", human_bytes(limit)),
            None => "The context size is too small for the configured output budget.".to_owned(),
        }
    }

    /// Advice for raising the limit, always worth showing when it is small.
    pub fn context_advice(&self) -> String {
        format!(
            "To analyze larger diffs, raise the server's context ({}), then raise the context \
             size here to match. A larger context uses more memory.",
            self.endpoint.context_hint()
        )
    }
}

pub fn human_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Consequential,
    Inconsequential,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Consequential => "consequential",
            Verdict::Inconsequential => "inconsequential",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    Manual,
    Auto,
}

/// Where a result came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub endpoint: Endpoint,
    pub model: String,
    /// Known for the session; only saved when the user opts in.
    pub host: Option<String>,
    pub trigger: Trigger,
    /// RFC 3339, UTC.
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisResult {
    pub verdict: Verdict,
    pub explanation: String,
    pub provenance: Provenance,
    pub prompt_tokens: Option<u32>,
    /// Truncation or an unverified prompt size.
    pub warning: Option<String>,
    /// The server cut the prompt: do not rely on the verdict.
    pub unreliable: bool,
}

impl AnalysisResult {
    /// A result from a finished analysis, or `None` when the model gave no
    /// usable answer.
    pub fn from_model(
        verdict: ModelVerdict,
        prompt_check: PromptCheck,
        prompt_tokens: Option<u32>,
        provenance: Provenance,
    ) -> Option<Self> {
        let (verdict, explanation) = match verdict {
            ModelVerdict::Inconsequential => (Verdict::Inconsequential, String::new()),
            ModelVerdict::Consequential { explanation } => (Verdict::Consequential, explanation),
            ModelVerdict::Unrecognized => return None,
        };
        Some(Self {
            verdict,
            explanation,
            provenance,
            prompt_tokens,
            warning: prompt_check.warning(),
            unreliable: prompt_check.unreliable(),
        })
    }
}

/// Why a package was not analyzed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// The automatic cap per run was reached.
    CapReached,
}

/// Where a package's analysis stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Idle,
    Queued,
    Running {
        thinking: String,
        answer: String,
    },
    Done(Box<AnalysisResult>),
    Failed(String),
    TooLarge {
        bytes: usize,
        limit: usize,
    },
    ContextTooSmall,
    Skipped(Skipped),
}

impl State {
    /// Where a package goes when sorted by verdict: consequential first,
    /// everything that is not known to be harmless next, inconsequential last.
    pub fn sort_rank(&self) -> u8 {
        match self {
            State::Done(result) => match result.verdict {
                Verdict::Consequential => 0,
                Verdict::Inconsequential if result.unreliable => 1,
                Verdict::Inconsequential => 2,
            },
            _ => 1,
        }
    }
}

const FILE_VERSION: u32 = 1;

/// The saved form: verdict and explanation, never the thinking.
#[derive(Debug, Serialize, Deserialize)]
struct SavedAnalysis {
    version: u32,
    verdict: String,
    explanation: String,
    model: String,
    endpoint: Endpoint,
    timestamp: String,
    trigger: Trigger,
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    warning: Option<String>,
    #[serde(default)]
    unreliable: bool,
    /// Only when the user opted in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
}

/// Write `result` to `path`. The host is left out unless `save_host`.
pub fn save(path: &Path, result: &AnalysisResult, save_host: bool) -> io::Result<()> {
    let saved = SavedAnalysis {
        version: FILE_VERSION,
        verdict: result.verdict.label().to_owned(),
        explanation: result.explanation.clone(),
        model: result.provenance.model.clone(),
        endpoint: result.provenance.endpoint,
        timestamp: result.provenance.timestamp.clone(),
        trigger: result.provenance.trigger,
        prompt_tokens: result.prompt_tokens,
        warning: result.warning.clone(),
        unreliable: result.unreliable,
        host: save_host.then(|| result.provenance.host.clone()).flatten(),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&saved).map_err(io::Error::other)?;
    fs::write(path, text + "\n")
}

/// Read a saved analysis. A missing, unreadable or unknown file is `None`.
pub fn load(path: &Path) -> Option<AnalysisResult> {
    let saved: SavedAnalysis = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    if saved.version != FILE_VERSION {
        return None;
    }
    let verdict = match saved.verdict.as_str() {
        "consequential" => Verdict::Consequential,
        "inconsequential" => Verdict::Inconsequential,
        _ => return None,
    };
    Some(AnalysisResult {
        verdict,
        explanation: saved.explanation,
        provenance: Provenance {
            endpoint: saved.endpoint,
            model: saved.model,
            host: saved.host,
            trigger: saved.trigger,
            timestamp: saved.timestamp,
        },
        prompt_tokens: saved.prompt_tokens,
        warning: saved.warning,
        unreliable: saved.unreliable,
    })
}

/// The current time as RFC 3339 in UTC.
pub fn now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    rfc3339(secs)
}

fn rfc3339(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(endpoint: Endpoint) -> AnalyzerSettings {
        AnalyzerSettings {
            endpoint,
            ..AnalyzerSettings::default()
        }
    }

    fn named(name: &str, model: &str) -> AnalyzerSettings {
        AnalyzerSettings {
            name: name.into(),
            model: model.into(),
            ..AnalyzerSettings::default()
        }
    }

    #[test]
    fn profiles_are_copied_renamed_and_removed() {
        let mut prefs = AnalysisPrefs::default();
        prefs.active_mut().model = "qwen3".into();
        assert_eq!(prefs.add_copy(), "Default 2");
        assert_eq!(prefs.active().model, "qwen3");
        assert_eq!(
            prefs.rename_active("Default"),
            Err("Another profile has this name")
        );
        assert_eq!(prefs.rename_active("  "), Err("A profile needs a name"));
        prefs.rename_active(" Claude ").unwrap();
        prefs.active_mut().model = "claude-opus".into();
        assert_eq!(prefs.active, "Claude");
        assert_eq!(prefs.names(), ["Default", "Claude"]);
        assert!(prefs.select("Default"));
        assert_eq!(prefs.active().model, "qwen3");
        assert!(!prefs.select("nope"));
        assert_eq!(prefs.active, "Default");
        assert!(prefs.remove_active());
        assert_eq!(prefs.active().model, "claude-opus");
        assert!(!prefs.remove_active());
        assert_eq!(prefs.names(), ["Claude"]);
    }

    #[test]
    fn normalize_repairs_hand_edited_profiles() {
        let mut prefs = AnalysisPrefs {
            active: "gone".into(),
            save_host: false,
            profiles: vec![],
        };
        prefs.normalize();
        assert_eq!(prefs.names(), ["Default"]);
        assert_eq!(prefs.active, "Default");

        let mut prefs = AnalysisPrefs {
            active: "Local 2".into(),
            save_host: false,
            profiles: vec![named("Local", "a"), named("Local", "b"), named("", "c")],
        };
        prefs.normalize();
        assert_eq!(prefs.names(), ["Local", "Local 2", "Default"]);
        assert_eq!(prefs.active().model, "b");
    }

    #[test]
    fn the_context_default_depends_on_the_endpoint_and_is_never_optimistic_locally() {
        assert_eq!(settings(Endpoint::Ollama).context(), 4096);
        assert_eq!(settings(Endpoint::LlamaCpp).context(), 4096);
        assert_eq!(settings(Endpoint::Openai).context(), 32768);
        let set = AnalyzerSettings {
            context_tokens: 16384,
            ..AnalyzerSettings::default()
        };
        assert_eq!(set.context(), 16384);
    }

    #[test]
    fn the_reserve_is_the_larger_budget_or_a_default() {
        let mut set = AnalyzerSettings::default();
        assert_eq!(set.answer_reserve(), 2048);
        set.max_output_tokens = 500;
        assert_eq!(set.answer_reserve(), 500);
        set.max_thinking_tokens = 3000;
        assert_eq!(set.answer_reserve(), 3000);
    }

    #[test]
    fn raising_a_budget_lowers_the_limit() {
        let mut set = AnalyzerSettings {
            context_tokens: 16384,
            ..AnalyzerSettings::default()
        };
        let before = set.max_diff_bytes().unwrap();
        set.max_thinking_tokens = 8192;
        assert!(set.max_diff_bytes().unwrap() < before);
        assert_eq!(before, (16384 - 800 - 2048) * 2);
    }

    #[test]
    fn the_stock_ollama_context_allows_a_small_diff() {
        let set = settings(Endpoint::Ollama);
        assert_eq!(set.max_diff_bytes(), Some((4096 - 800 - 2048) * 2));
        assert_eq!(set.check_size(2000), SizeCheck::Fits);
        assert_eq!(
            set.check_size(5000),
            SizeCheck::TooLarge {
                bytes: 5000,
                limit: 2496
            }
        );
    }

    #[test]
    fn a_context_that_cannot_hold_the_budget_has_no_limit() {
        let set = AnalyzerSettings {
            max_thinking_tokens: 4096,
            ..AnalyzerSettings::default()
        };
        assert_eq!(set.max_diff_bytes(), None);
        assert_eq!(set.check_size(1), SizeCheck::ContextTooSmall);
        assert!(set.limit_text().contains("too small"));
        let exact = AnalyzerSettings {
            max_thinking_tokens: 4096 - 800,
            ..AnalyzerSettings::default()
        };
        assert_eq!(exact.max_diff_bytes(), None, "zero room is no room");
    }

    #[test]
    fn the_advice_names_the_servers_setting() {
        assert!(
            settings(Endpoint::Ollama)
                .context_advice()
                .contains("OLLAMA_CONTEXT_LENGTH")
        );
        assert!(
            settings(Endpoint::LlamaCpp)
                .context_advice()
                .contains("llama-server -c")
        );
    }

    fn result(verdict: Verdict) -> AnalysisResult {
        AnalysisResult {
            verdict,
            explanation: "a changed flag".into(),
            provenance: Provenance {
                endpoint: Endpoint::LlamaCpp,
                model: "qwen".into(),
                host: Some("llm.internal:8080".into()),
                trigger: Trigger::Auto,
                timestamp: "2026-11-08T04:30:00Z".into(),
            },
            prompt_tokens: Some(1200),
            warning: Some("careful".into()),
            unreliable: true,
        }
    }

    #[test]
    fn the_host_is_not_saved_unless_asked_and_the_thinking_never_is() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg/analysis");
        let original = result(Verdict::Consequential);

        save(&path, &original, false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("llm.internal") && !text.contains("host"));
        assert!(text.contains("\"endpoint\": \"llama-cpp\"") && text.contains("qwen"));
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.provenance.host, None);
        assert_eq!(
            AnalysisResult {
                provenance: Provenance {
                    host: original.provenance.host.clone(),
                    ..loaded.provenance.clone()
                },
                ..loaded
            },
            original
        );

        save(&path, &original, true).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("llm.internal"));
        assert_eq!(load(&path).unwrap(), original);
    }

    #[test]
    fn a_missing_or_bad_file_loads_as_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("analysis");
        assert!(load(&path).is_none());
        for bad in [
            "not json",
            "{}",
            r#"{"version":2,"verdict":"consequential","explanation":"","model":"m","endpoint":"ollama","timestamp":"t","trigger":"manual"}"#,
            r#"{"version":1,"verdict":"maybe","explanation":"","model":"m","endpoint":"ollama","timestamp":"t","trigger":"manual"}"#,
        ] {
            fs::write(&path, bad).unwrap();
            assert!(load(&path).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_file_with_extra_fields_and_without_the_optional_ones_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("analysis");
        fs::write(
            &path,
            r#"{"version":1,"verdict":"inconsequential","explanation":"","model":"m","endpoint":"openai","timestamp":"t","trigger":"manual","future":1}"#,
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.verdict, Verdict::Inconsequential);
        assert_eq!(loaded.provenance.host, None);
        assert!(!loaded.unreliable);
    }

    #[test]
    fn only_a_usable_model_answer_makes_a_result() {
        let provenance = result(Verdict::Consequential).provenance;
        let consequential = AnalysisResult::from_model(
            ModelVerdict::Consequential {
                explanation: "why".into(),
            },
            PromptCheck::Truncated {
                reported: 10,
                expected: 100,
            },
            Some(10),
            provenance.clone(),
        )
        .unwrap();
        assert!(consequential.unreliable && consequential.warning.is_some());
        assert_eq!(consequential.explanation, "why");
        assert!(
            AnalysisResult::from_model(
                ModelVerdict::Unrecognized,
                PromptCheck::Complete,
                None,
                provenance
            )
            .is_none()
        );
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_794_198_600), "2026-11-09T04:30:00Z");
    }

    #[test]
    fn the_settings_survive_a_toml_round_trip_and_missing_fields_default() {
        let set = AnalyzerSettings {
            endpoint: Endpoint::Openai,
            auto_timing: AutoTiming::DuringRun,
            model: "m".into(),
            ..AnalyzerSettings::default()
        };
        let text = toml::to_string(&set).unwrap();
        assert_eq!(toml::from_str::<AnalyzerSettings>(&text).unwrap(), set);
        assert_eq!(
            toml::from_str::<AnalyzerSettings>("model = \"x\"")
                .unwrap()
                .auto_cap,
            25
        );
        assert!(!AnalyzerSettings::default().auto_analyze);
    }
}
