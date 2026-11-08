//! End-to-end tests. A fake `nix` that just `cat`s the file named by its
//! argument stands in for the real one; `git` must be on PATH.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let nix = bin.join("nix");
        fs::write(
            &nix,
            "#!/bin/sh\n[ \"$1\" = log ] || exit 64\nexec cat -- \"$2\"\n",
        )
        .unwrap();
        fs::set_permissions(&nix, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.dir.path().join("bin")).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        Command::new(env!("CARGO_BIN_EXE_difflog"))
            .args(args)
            .env("PATH", path)
            .env_remove("RUST_LOG")
            .env_remove("NO_COLOR")
            .current_dir(self.dir.path())
            .output()
            .unwrap()
    }
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn identical_logs_exit_0() {
    let env = Env::new();
    let a = env.file("a-1.log", "building\ndone\n");
    let b = env.file("b-1.log", "building\ndone\n");
    let out = env.run(&[s(&a), s(&b)]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
    assert!(stderr(&out).contains("No changes detected"));
}

#[test]
fn boring_differences_are_normalized_away() {
    let env = Env::new();
    let a = env.file(
        "a.log",
        "\x1b[1mcc\x1b[0m /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x make -j4\n[1/9] CC\n",
    );
    let b = env.file(
        "b.log",
        "cc /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x make -j32\n[7/9] CC\n",
    );
    let out = env.run(&[s(&a), s(&b)]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[test]
fn real_difference_exit_1_and_shown() {
    let env = Env::new();
    let a = env.file("a.log", "one\ntwo\n");
    let b = env.file("b.log", "one\nthree\n");
    let out = env.run(&[s(&a), s(&b)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let diff = stdout(&out);
    assert!(diff.contains("-two"), "{diff}");
    assert!(diff.contains("+three"), "{diff}");
    assert!(diff.contains("a/a.log.log"), "{diff}");
    assert!(diff.contains("b/b.log.log"), "{diff}");
    assert!(stderr(&out).contains("Changes detected"));
}

#[test]
fn keep_flags_expose_differences() {
    let env = Env::new();
    let a = env.file("a.log", "-- Configuring done (1.0s)\nmake -j2\n");
    let b = env.file("b.log", "-- Configuring done (9.5s)\nmake -j8\n");
    assert_eq!(env.run(&[s(&a), s(&b)]).status.code(), Some(0));
    assert_eq!(
        env.run(&[s(&a), s(&b), "--keep-timings"]).status.code(),
        Some(1)
    );
    assert_eq!(
        env.run(&[s(&a), s(&b), "--keep-jobs"]).status.code(),
        Some(1)
    );
}

#[test]
fn sort_lines_hides_reordering() {
    let env = Env::new();
    let a = env.file("a.log", "x\ny\nz\n");
    let b = env.file("b.log", "z\nx\ny\n");
    assert_eq!(env.run(&[s(&a), s(&b)]).status.code(), Some(1));
    let out = env.run(&[s(&a), s(&b), "--sort-lines"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("SORTED"));
}

#[test]
fn git_diff_options_are_passed_through() {
    let env = Env::new();
    let body = |x: &str| format!("1\n2\n3\n4\n5\n6\n7\n{x}\n9\n10\n11\n12\n13\n14\n");
    let a = env.file("a.log", &body("old"));
    let b = env.file("b.log", &body("new"));
    let wide = stdout(&env.run(&[s(&a), s(&b)]));
    assert!(wide.contains(" 5\n"), "{wide}");
    let narrow = stdout(&env.run(&[s(&a), s(&b), "-U0"]));
    assert!(!narrow.contains(" 5\n"), "{narrow}");
    assert!(
        narrow.contains("-old") && narrow.contains("+new"),
        "{narrow}"
    );
}

#[test]
fn symlinks_are_followed_unless_no_follow() {
    let env = Env::new();
    let real = env.file("real.log", "same\n");
    let other = env.file("other.log", "same\n");
    let link = env.dir.path().join("result");
    symlink(&real, &link).unwrap();

    let out = env.run(&[s(&link), s(&other)]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("result ->"), "{}", stderr(&out));

    let out = env.run(&[s(&link), s(&other), "--no-follow"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stderr(&out).contains("result ->"), "{}", stderr(&out));
}

#[test]
fn dry_run_runs_nothing() {
    let env = Env::new();
    // These would make `nix log` fail if they were actually run.
    let out = env.run(&["--dry-run", "does-not-exist-1", "does not exist 2", "-U0"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("nix log does-not-exist-1"), "{err}");
    assert!(err.contains("nix log 'does not exist 2'"), "{err}");
    assert!(err.contains("git diff --no-index --no-prefix -U0"), "{err}");
    assert!(stdout(&out).is_empty());
}

#[test]
fn nix_failure_exit_2() {
    let env = Env::new();
    let a = env.file("a.log", "x\n");
    let out = env.run(&[s(&a), "missing.log"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("nix log"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[test]
fn missing_nix_exit_2() {
    let env = Env::new();
    let a = env.file("a.log", "x\n");
    let out = Command::new(env!("CARGO_BIN_EXE_difflog"))
        .args([s(&a), s(&a)])
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("failed to run `nix log`"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn usage_errors_exit_2() {
    let env = Env::new();
    let out = env.run(&["only-one"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Usage"), "{}", stderr(&out));
}

#[test]
fn quiet_hides_notes() {
    let env = Env::new();
    let a = env.file("a.log", "x\n");
    let out = env.run(&[s(&a), s(&a), "-q"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stderr(&out).is_empty(), "{}", stderr(&out));
}
