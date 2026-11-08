//! End-to-end tests: the real binary in a real git repository, with fake
//! `nix`, `nix-eval-jobs` and `diffoscope` first in PATH (the fakes treat
//! plain directories as store paths).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            dir: TempDir::new().unwrap(),
        };
        fs::create_dir_all(env.path("bin")).unwrap();
        fs::create_dir_all(env.path("work")).unwrap();

        git(&env.path("work"), &["init", "-q", "-b", "master"]);
        git(
            &env.path("work"),
            &["commit", "-q", "--allow-empty", "-m", "one"],
        );
        git(
            &env.path("work"),
            &["commit", "-q", "--allow-empty", "-m", "two"],
        );

        let store = |name: &str, content: &str, log: &str| {
            let path = env.path("store").join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("file"), content).unwrap();
            fs::write(env.path("store").join(format!("{name}.log")), log).unwrap();
            path.display().to_string()
        };
        let job = |attr: &str, drv: &str, out: &str| {
            format!(r#"{{"attr":"{attr}","drvPath":"{drv}","outputs":{{"out":"{out}"}}}}"#)
        };
        let (old_a, new_a) = (
            store("old-a", "old\n", "same\nold-line\n"),
            store("new-a", "new\n", "same\nnew-line\n"),
        );
        let same_b = store("same-b", "b\n", "b\n");
        fs::write(
            env.path("old.jsonl"),
            format!(
                "{}\n{}\n",
                job("a", "/drv/old-a.drv", &old_a),
                job("b", "/drv/b.drv", &same_b)
            ),
        )
        .unwrap();
        fs::write(
            env.path("new.jsonl"),
            format!(
                "{}\n{}\n",
                job("a", "/drv/new-a.drv", &new_a),
                job("b", "/drv/b.drv", &same_b)
            ),
        )
        .unwrap();

        let dir = env.path("");
        let dir = dir.display();
        env.script(
            "nix-eval-jobs",
            &format!(
                r#"for a; do last=$a; done
case "$last" in *rev=*) cat {dir}/old.jsonl;; *) cat {dir}/new.jsonl;; esac"#
            ),
        );
        env.script(
            "nix",
            &format!(
                r#"case "$1" in
  build) cat >/dev/null; echo "$*" >> {dir}/nix-build-args;;
  log) cat "$2.log";;
esac"#
            ),
        );
        env.script(
            "diffoscope",
            r#"for arg; do before=$last; last=$arg; done
exec diff -r --no-dereference "$before" "$last""#,
        );
        env
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.path("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_with_path(&self, path: &str, args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_diff-nixpkgs-attr"))
            .args(args)
            .current_dir(self.path("work"))
            .env("PATH", path)
            .env_remove("NO_COLOR")
            .env_remove("SYSTEM")
            .env_remove("WORKERS")
            .env_remove("DIFF_PAGER")
            .output()
            .expect("running the binary");
        Run {
            code: output.status.code().expect("exit code"),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn run(&self, args: &[&str]) -> Run {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap()
        );
        self.run_with_path(&path, args)
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn compares_prints_summarises_and_saves() {
    let env = Env::new();
    let run = env.run(&["-b", "HEAD~1", "pkgs.set"]);
    assert_eq!(run.code, 0, "{}", run.stderr);

    let stdout = &run.stdout;
    assert!(stdout.contains("\n set.a \n"), "{stdout}");
    assert!(stdout.contains("+new-line"), "{stdout}");
    assert!(stdout.contains("--- set.a [out]\n"), "{stdout}");
    assert!(
        stdout.contains("Differences found in 1 attribute(s):"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  set.a  difflog, diffoscope-nix [out]\n"),
        "{stdout}"
    );
    let saved = stdout
        .lines()
        .find_map(|l| l.strip_prefix("Differences saved to: "))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(Path::new(saved).starts_with(env.path("work")));
    assert!(Path::new(saved).join("set.a/difflog").is_file());
    assert!(Path::new(saved).join("set.a/diffoscope/out").is_file());

    let stderr = &run.stderr;
    assert!(stderr.contains(">>> set: evaluating base"), "{stderr}");
    assert!(
        stderr.contains(">>> set: 1 changed non-debug outputs (1 package(s))"),
        "{stderr}"
    );
    assert!(
        stderr.contains(">>> building the old side (1 derivations)"),
        "{stderr}"
    );
    assert!(
        !stderr.contains('\x1b'),
        "no colour when not a terminal: {stderr:?}"
    );
}

#[test]
fn dry_run_lists_what_would_be_diffed() {
    let env = Env::new();
    let run = env.run(&["-n", "-b", "HEAD~1", "set"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stdout.contains("would diff set.a [out]\n"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("Differences"), "{}", run.stdout);
    let args = fs::read_to_string(env.path("nix-build-args")).unwrap();
    assert!(args.lines().all(|l| l.contains("--dry-run")), "{args}");
    assert_eq!(
        fs::read_dir(env.path("work"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("differences-")
            })
            .count(),
        0
    );
}

#[test]
fn nothing_changed_is_quiet_success() {
    let env = Env::new();
    fs::copy(env.path("new.jsonl"), env.path("old.jsonl")).unwrap();
    let run = env.run(&["-b", "HEAD~1", "set"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr.contains("no changed non-debug outputs"),
        "{}",
        run.stderr
    );
}

#[test]
fn missing_diffoscope_is_reported() {
    let env = Env::new();
    fs::remove_file(env.path("bin/diffoscope")).unwrap();
    // A PATH holding only the fakes and git, so no real diffoscope is found.
    let git = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    std::os::unix::fs::symlink(git.trim(), env.path("bin/git")).unwrap();
    let run = env.run_with_path(&env.path("bin").display().to_string(), &["set"]);
    assert_eq!(run.code, 127, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("error: required program not found in PATH: diffoscope"),
        "{}",
        run.stderr
    );
}

#[test]
fn bad_input_is_rejected() {
    let env = Env::new();
    let none = env.run(&[]);
    assert_eq!(none.code, 2);
    assert!(none.stderr.contains("Usage:"), "{}", none.stderr);

    let invalid = env.run(&["not valid!"]);
    assert_eq!(invalid.code, 1);
    assert!(
        invalid
            .stderr
            .contains("invalid attribute path: not valid!"),
        "{}",
        invalid.stderr
    );

    let size = env.run(&["--memory-max", "8X", "set"]);
    assert_eq!(size.code, 2);
}
