//! Running child processes under control.
//!
//! Every child is started in its own process group. When the work is
//! cancelled or runs out of time the whole group gets SIGTERM and, after a
//! short grace period, SIGKILL, so the grandchildren that tools like `nix
//! build` and `diffoscope` spawn die with them. A memory cap can be applied
//! by starting the child through `systemd-run --user --scope -p MemoryMax=…`.
//!
//! Waiting is done by polling, so a blocking read in the child can never keep
//! the caller from noticing a cancellation.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;

use crate::CancelToken;

/// How often a running child is checked for exit, cancellation and timeout.
const POLL: Duration = Duration::from_millis(50);
/// Time between SIGTERM and SIGKILL.
const GRACE: Duration = Duration::from_secs(2);
/// How long output still held open by orphaned grandchildren is waited for
/// once the child itself has exited.
const DRAIN: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SystemdRun {
    /// Use `systemd-run --user --scope` for memory caps when it works here,
    /// otherwise run the child without a cap.
    #[default]
    Auto,
    /// Never use it.
    Off,
}

/// Limits for a child process.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessPolicy {
    /// Memory cap in bytes, applied to commands marked with
    /// [`Cmd::memory_limited`].
    pub memory_max: Option<u64>,
    /// Wall-clock limit per child.
    pub timeout: Option<Duration>,
    pub systemd_run: SystemdRun,
}

/// A policy plus the token that cancels everything run under it.
#[derive(Debug, Clone, Default)]
pub struct Control {
    pub token: CancelToken,
    pub policy: ProcessPolicy,
}

impl Control {
    pub fn new(token: CancelToken, policy: ProcessPolicy) -> Self {
        Self { token, policy }
    }

    /// No limits, never cancelled.
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// Whether a configured memory cap can actually be enforced, so a caller
    /// can tell the user when it can't.
    pub fn memory_cap_enforceable(&self) -> bool {
        self.policy.memory_max.is_none() || self.memory_cap_enforced()
    }

    /// Whether there is a memory cap and it is being enforced.
    fn memory_cap_enforced(&self) -> bool {
        self.scope_wanted() && systemd_scope_available()
    }

    fn scope_wanted(&self) -> bool {
        self.policy.memory_max.is_some() && self.policy.systemd_run == SystemdRun::Auto
    }
}

/// Whether `systemd-run --user --scope` works here (needs a user manager).
pub fn systemd_scope_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// A command to run. Unlike `std::process::Command` it can be wrapped in
/// `systemd-run`, so its standard streams are chosen when it is run.
#[derive(Debug, Clone)]
pub struct Cmd {
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, Option<OsString>)>,
    cwd: Option<PathBuf>,
    stdin: Option<Vec<u8>>,
    memory_limited: bool,
    foreground: bool,
}

impl Cmd {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            envs: Vec::new(),
            cwd: None,
            stdin: None,
            memory_limited: false,
            foreground: false,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, A>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }

    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Self {
        self.envs.push((key.as_ref().to_owned(), None));
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Data fed to the child's standard input. Without it stdin is `/dev/null`.
    pub fn stdin_data(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// Apply the policy's memory cap to this command.
    pub fn memory_limited(mut self) -> Self {
        self.memory_limited = true;
        self
    }

    /// Keep the child in the parent's process group, which a command that
    /// uses the terminal (a pager, say) needs: a background group can't read
    /// from it. Cancelling then only stops the child itself, not its own
    /// children; the terminal's Ctrl-C reaches those anyway.
    pub fn foreground(mut self) -> Self {
        self.foreground = true;
        self
    }

    /// The command as it would be run, e.g. for showing in a dry run.
    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|part| shell_quote(&part.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn build(&self, ctl: &Control) -> (Command, Option<String>) {
        let (mut command, unit) = match ctl.policy.memory_max {
            Some(bytes) if self.memory_limited && ctl.memory_cap_enforced() => {
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                let unit = format!(
                    "nix-utility-{}-{}",
                    std::process::id(),
                    COUNTER.fetch_add(1, Ordering::Relaxed)
                );
                let mut command = Command::new("systemd-run");
                command
                    .args(["--user", "--scope", "--quiet", "--collect"])
                    .arg(format!("--unit={unit}"))
                    .arg("-p")
                    .arg(format!("MemoryMax={bytes}"))
                    .args(["-p", "MemorySwapMax=0", "--"])
                    .arg(&self.program);
                (command, Some(unit))
            }
            _ => (Command::new(&self.program), None),
        };
        command.args(&self.args);
        for (key, value) in &self.envs {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        (command, unit)
    }
}

/// Quote `word` for a POSIX shell, leaving plain words alone.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// What a standard output stream of the child is connected to.
pub enum Out<'a> {
    /// `/dev/null`.
    Null,
    /// The parent's own stream.
    Inherit,
    /// Delivered in chunks, on the thread that called [`run`].
    Sink(&'a mut dyn FnMut(&[u8])),
}

impl Out<'_> {
    fn stdio(&self) -> Stdio {
        match self {
            Out::Null => Stdio::null(),
            Out::Inherit => Stdio::inherit(),
            Out::Sink(_) => Stdio::piped(),
        }
    }
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Exited(ExitStatus),
    /// The token was cancelled; the process group has been killed.
    Cancelled,
    /// The policy's timeout expired; the process group has been killed.
    TimedOut,
}

impl Termination {
    pub fn success(self) -> bool {
        matches!(self, Termination::Exited(status) if status.success())
    }

    pub fn code(self) -> Option<i32> {
        match self {
            Termination::Exited(status) => status.code(),
            _ => None,
        }
    }

    /// Ended by SIGPIPE (or the shell's 128 + SIGPIPE status): whatever it
    /// was writing to went away, e.g. the user quit a pager early.
    pub fn broken_pipe(self) -> bool {
        let sigpipe = Signal::SIGPIPE as i32;
        matches!(self, Termination::Exited(status)
            if status.signal() == Some(sigpipe) || status.code() == Some(128 + sigpipe))
    }

    /// Killed by SIGKILL while a memory cap was enforced, which is how the
    /// cgroup OOM killer shows up. Without an enforced cap a SIGKILL came
    /// from elsewhere (the kernel's own OOM killer, say).
    pub fn killed_by_memory_limit(self, ctl: &Control) -> bool {
        matches!(self, Termination::Exited(status)
            if status.signal() == Some(Signal::SIGKILL as i32)
                && ctl.memory_cap_enforced())
    }
}

/// Everything a child wrote, for commands whose output is small.
#[derive(Debug)]
pub struct Captured {
    pub termination: Termination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Captured {
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

pub fn run_captured(cmd: &Cmd, ctl: &Control) -> io::Result<Captured> {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let termination = run(
        cmd,
        ctl,
        Out::Sink(&mut |chunk| stdout.extend_from_slice(chunk)),
        Out::Sink(&mut |chunk| stderr.extend_from_slice(chunk)),
    )?;
    Ok(Captured {
        termination,
        stdout,
        stderr,
    })
}

#[derive(Clone, Copy)]
enum Which {
    Stdout,
    Stderr,
}

fn pump(mut reader: impl Read + Send + 'static, which: Which, tx: mpsc::Sender<(Which, Vec<u8>)>) {
    thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send((which, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn deliver(out: &mut Out<'_>, chunk: &[u8]) {
    if let Out::Sink(sink) = out {
        sink(chunk);
    }
}

/// Run `cmd` to completion, cancellation or timeout.
pub fn run(
    cmd: &Cmd,
    ctl: &Control,
    mut stdout: Out<'_>,
    mut stderr: Out<'_>,
) -> io::Result<Termination> {
    if ctl.token.is_cancelled() {
        return Ok(Termination::Cancelled);
    }

    let (mut command, unit) = cmd.build(ctl);
    if !cmd.foreground {
        command.process_group(0);
    }
    command
        .stdin(if cmd.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(stdout.stdio())
        .stderr(stderr.stdio());
    let mut child = command.spawn()?;

    if let (Some(mut pipe), Some(data)) = (child.stdin.take(), cmd.stdin.clone()) {
        // A thread, so a child that doesn't read its input can't block us.
        thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }

    let (tx, rx) = mpsc::channel();
    if let (Out::Sink(_), Some(pipe)) = (&stdout, child.stdout.take()) {
        pump(pipe, Which::Stdout, tx.clone());
    }
    if let (Out::Sink(_), Some(pipe)) = (&stderr, child.stderr.take()) {
        pump(pipe, Which::Stderr, tx.clone());
    }
    drop(tx);

    let mut dispatch = |(which, chunk): (Which, Vec<u8>)| match which {
        Which::Stdout => deliver(&mut stdout, &chunk),
        Which::Stderr => deliver(&mut stderr, &chunk),
    };

    let deadline = ctl.policy.timeout.map(|timeout| Instant::now() + timeout);
    let mut readers_open = true;
    let termination = loop {
        if readers_open {
            match rx.recv_timeout(POLL) {
                Ok(item) => dispatch(item),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => readers_open = false,
            }
        } else {
            thread::sleep(POLL);
        }
        if let Some(status) = child.try_wait()? {
            break Termination::Exited(status);
        }
        if ctl.token.is_cancelled() {
            terminate(&mut child, unit.as_deref(), !cmd.foreground);
            break Termination::Cancelled;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            terminate(&mut child, unit.as_deref(), !cmd.foreground);
            break Termination::TimedOut;
        }
    };

    // Whatever the child wrote before it ended.
    let drain_until = Instant::now() + DRAIN;
    while readers_open {
        let left = drain_until.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(item) => dispatch(item),
            Err(_) => readers_open = false,
        }
    }
    Ok(termination)
}

/// Stop the child's whole process group: SIGTERM, then SIGKILL after a grace
/// period (and SIGKILL for stragglers even if the leader already left).
fn terminate(child: &mut Child, unit: Option<&str>, own_group: bool) {
    let pid = Pid::from_raw(child.id() as i32);
    let signal = |signal| {
        let _ = if own_group {
            killpg(pid, signal)
        } else {
            kill(pid, signal)
        };
    };
    signal(Signal::SIGTERM);
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(20));
    }
    signal(Signal::SIGKILL);
    if let Some(unit) = unit {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", "--quiet"])
            .arg(format!("{unit}.scope"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    fn sh(script: &str) -> Cmd {
        Cmd::new("sh").args(["-c", script])
    }

    fn alive(pid: i32) -> bool {
        kill(Pid::from_raw(pid), None).is_ok()
    }

    fn wait_for(path: &std::path::Path) -> String {
        for _ in 0..200 {
            if let Ok(text) = fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return text;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("{} never appeared", path.display());
    }

    #[test]
    fn captures_output_and_status() {
        let out =
            run_captured(&sh("echo out; echo err >&2; exit 3"), &Control::unlimited()).unwrap();
        assert_eq!(out.stdout_lossy(), "out\n");
        assert_eq!(out.stderr_lossy(), "err\n");
        assert_eq!(out.termination.code(), Some(3));
    }

    #[test]
    fn feeds_stdin() {
        let cmd = Cmd::new("cat").stdin_data("hello");
        let out = run_captured(&cmd, &Control::unlimited()).unwrap();
        assert_eq!(out.stdout_lossy(), "hello");
    }

    #[test]
    fn missing_program_is_an_error() {
        assert!(run_captured(&Cmd::new("/nonexistent/prog"), &Control::unlimited()).is_err());
    }

    #[test]
    fn already_cancelled_runs_nothing() {
        let ctl = Control::unlimited();
        ctl.token.cancel();
        let out = run_captured(&sh("echo hi"), &ctl).unwrap();
        assert_eq!(out.termination, Termination::Cancelled);
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn cancel_kills_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let cmd = sh(&format!("sleep 60 & echo $! > {}; wait", pidfile.display()));
        let ctl = Control::unlimited();
        let canceller = {
            let token = ctl.token.clone();
            let pidfile = pidfile.clone();
            thread::spawn(move || {
                let pid: i32 = wait_for(&pidfile).trim().parse().unwrap();
                token.cancel();
                pid
            })
        };
        let started = Instant::now();
        let out = run_captured(&cmd, &ctl).unwrap();
        let grandchild = canceller.join().unwrap();
        assert_eq!(out.termination, Termination::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(20));
        for _ in 0..100 {
            if !alive(grandchild) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("grandchild {grandchild} survived cancellation");
    }

    #[test]
    fn sigterm_ignoring_child_is_killed_after_grace() {
        let ctl = Control::unlimited();
        let token = ctl.token.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            token.cancel();
        });
        let out = run_captured(&sh("trap '' TERM; while :; do sleep 1; done"), &ctl).unwrap();
        canceller.join().unwrap();
        assert_eq!(out.termination, Termination::Cancelled);
    }

    #[test]
    fn foreground_cancel_stops_only_the_child() {
        let ctl = Control::unlimited();
        let token = ctl.token.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            token.cancel();
        });
        let out = run_captured(&sh("sleep 60").foreground(), &ctl).unwrap();
        canceller.join().unwrap();
        assert_eq!(out.termination, Termination::Cancelled);
    }

    #[test]
    fn timeout_stops_the_child() {
        let ctl = Control::new(
            CancelToken::new(),
            ProcessPolicy {
                timeout: Some(Duration::from_millis(200)),
                ..ProcessPolicy::default()
            },
        );
        let out = run_captured(&sh("sleep 60"), &ctl).unwrap();
        assert_eq!(out.termination, Termination::TimedOut);
    }

    #[test]
    fn streams_output_while_running() {
        let chunks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = chunks.clone();
        let termination = run(
            &sh("echo one; sleep 0.2; echo two"),
            &Control::unlimited(),
            Out::Sink(&mut move |chunk| seen.lock().unwrap().push(chunk.to_vec())),
            Out::Null,
        )
        .unwrap();
        assert!(termination.success());
        let all: Vec<u8> = chunks.lock().unwrap().concat();
        assert_eq!(all, b"one\ntwo\n");
        assert!(chunks.lock().unwrap().len() >= 2);
    }

    #[test]
    fn memory_cap_without_systemd_run_still_runs() {
        let ctl = Control::new(
            CancelToken::new(),
            ProcessPolicy {
                memory_max: Some(1 << 30),
                systemd_run: SystemdRun::Off,
                ..ProcessPolicy::default()
            },
        );
        assert!(!ctl.memory_cap_enforceable());
        let out = run_captured(&sh("echo ok").memory_limited(), &ctl).unwrap();
        assert_eq!(out.stdout_lossy(), "ok\n");
    }

    #[test]
    fn a_broken_pipe_is_recognised() {
        let ctl = Control::unlimited();
        let killed = run_captured(&sh("kill -PIPE $$"), &ctl).unwrap();
        assert!(killed.termination.broken_pipe());
        let shell_status = run_captured(&sh("exit 141"), &ctl).unwrap();
        assert!(shell_status.termination.broken_pipe());
        let other = run_captured(&sh("exit 1"), &ctl).unwrap();
        assert!(!other.termination.broken_pipe());
    }

    #[test]
    fn display_quotes_words() {
        assert_eq!(
            Cmd::new("echo").args(["a b", "it's", "plain"]).display(),
            r"echo 'a b' 'it'\''s' plain"
        );
    }
}
