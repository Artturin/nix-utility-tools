//! End-to-end tests: run the real binary with a fake `diffoscope` first in
//! PATH, which prints its arguments and diffs the two trees it was given.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const FAKE_DIFFOSCOPE: &str = r#"#!/bin/sh
echo "args: $*" >&2
for arg; do before=$last; last=$arg; done
exec diff -r --no-dereference "$before" "$last"
"#;

const HASH_A: &str = "rjxgv8qsq5kdyz0zk1fqj6d9a7d0n3sv";
const HASH_B: &str = "0123456789abcdfghijklmnpqrsvwxyz";

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
        fs::create_dir(env.path("bin")).unwrap();
        let fake = env.path("bin/diffoscope");
        fs::write(&fake, FAKE_DIFFOSCOPE).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        env
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }

    /// A store-like tree whose files and symlink mention `hash`.
    fn tree(&self, name: &str, hash: &str) -> String {
        let root = self.path(name);
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(
            root.join("lib/libfoo.txt"),
            format!("rpath /nix/store/{hash}-glibc/lib\n"),
        )
        .unwrap();
        fs::write(root.join("plain"), "no hashes here\n").unwrap();
        symlink(format!("/nix/store/{hash}-glibc/lib"), root.join("link")).unwrap();
        root.to_str().unwrap().to_owned()
    }

    fn run(&self, args: &[&str], with_fake: bool) -> Run {
        let path = if with_fake {
            format!(
                "{}:{}",
                self.path("bin").display(),
                std::env::var("PATH").unwrap()
            )
        } else {
            self.path("empty").display().to_string()
        };
        let output = Command::new(env!("CARGO_BIN_EXE_diffoscope-nix"))
            .args(args)
            .env("PATH", path)
            .output()
            .expect("running the binary");
        Run {
            code: output.status.code().expect("exit code"),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

#[test]
fn trees_differing_only_in_store_hashes_are_equal() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let b = env.tree("b", HASH_B);
    let run = env.run(&[&a, &b], true);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.is_empty(), "{}", run.stdout);
}

#[test]
fn real_differences_still_show_and_exit_status_is_diffoscopes() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let b = env.tree("b", HASH_B);
    fs::write(Path::new(&b).join("plain"), "changed\n").unwrap();
    let run = env.run(&[&a, &b], true);
    assert_eq!(run.code, 1);
    assert!(run.stdout.contains("changed"), "{}", run.stdout);
}

#[test]
fn inputs_are_left_untouched() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let b = env.tree("b", HASH_B);
    env.run(&[&a, &b], true);
    let content = fs::read_to_string(Path::new(&a).join("lib/libfoo.txt")).unwrap();
    assert!(content.contains(HASH_A));
}

#[test]
fn extra_args_follow_the_paths_and_text_color_is_forced() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let b = env.tree("b", HASH_B);
    let run = env.run(&[&a, &b, "--", "--html", "out.html", "-n", "--help"], true);
    assert!(
        run.stderr
            .contains("args: --text-color always --html out.html -n --help "),
        "{}",
        run.stderr
    );
}

#[test]
fn temp_dir_is_removed_even_though_copies_are_read_only() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let b = env.tree("b", HASH_B);
    fs::set_permissions(Path::new(&a).join("lib"), fs::Permissions::from_mode(0o555)).unwrap();
    let run = env.run(&[&a, &b], true);
    fs::set_permissions(Path::new(&a).join("lib"), fs::Permissions::from_mode(0o755)).unwrap();
    let copy = run
        .stderr
        .lines()
        .find_map(|l| l.strip_prefix("args: "))
        .and_then(|l| l.split_whitespace().rev().nth(1))
        .expect("copy path")
        .to_owned();
    assert!(!Path::new(&copy).exists(), "{copy} left behind");
}

#[test]
fn dry_run_lists_what_would_change_without_running_diffoscope() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let b = env.tree("b", HASH_B);
    let run = env.run(&["--dry-run", &a, &b, "--html", "x"], true);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("  file    {a}/lib/libfoo.txt (store hashes)"))
    );
    assert!(run.stdout.contains(&format!("  symlink {a}/link")));
    assert!(!run.stdout.contains("plain"));
    assert!(run.stdout.contains(
        "[dry-run] would run: diffoscope --text-color always --html x <tmp>/a/a <tmp>/b/b"
    ));
    assert!(!run.stderr.contains("args:"));
}

#[test]
fn keep_build_id_prints_a_banner() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let b = env.tree("b", HASH_B);
    let run = env.run(&["--keep-build-id", &a, &b], true);
    assert!(
        run.stderr
            .contains("diffoscope-nix: GNU build IDs are KEPT"),
        "{}",
        run.stderr
    );
    assert!(!env.run(&[&a, &b], true).stderr.contains("KEPT"));
}

#[test]
fn missing_diffoscope_exits_127() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let run = env.run(&[&a, &a], false);
    assert_eq!(run.code, 127);
    assert!(run.stderr.contains("diffoscope not found in PATH"));
}

#[test]
fn missing_path_exits_2() {
    let env = Env::new();
    let a = env.tree("a", HASH_A);
    let run = env.run(&[&a, "/nonexistent/path"], true);
    assert_eq!(run.code, 2);
    assert!(run.stderr.contains("no such path: /nonexistent/path"));
}

#[test]
fn too_few_paths_is_a_usage_error() {
    let env = Env::new();
    let run = env.run(&["only-one"], true);
    assert_eq!(run.code, 2);
}

#[test]
fn unknown_option_before_the_paths_is_rejected() {
    let env = Env::new();
    let run = env.run(&["--bogus", "a", "b"], true);
    assert_eq!(run.code, 2);
}
