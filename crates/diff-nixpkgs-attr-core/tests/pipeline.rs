//! End-to-end run of the pipeline with a real git repository and fake `nix`,
//! `nix-eval-jobs` and `diffoscope` first in PATH. The fakes use directories
//! as "store paths". PATH is process-wide, so everything is one test.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use diff_nixpkgs_attr_core::{
    CancelToken, Config, Control, Event, Flow, Side, Status, Tool, ToolStatus, build::BuildEvent,
    run_pipeline,
};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.path("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A fake store path with a content file and a build log.
    fn store_path(&self, name: &str, content: &str, log: &str) -> String {
        let path = self.path("store").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("file"), content).unwrap();
        fs::write(self.path("store").join(format!("{name}.log")), log).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn job(attr: &str, drv: &str, out: &str) -> String {
    format!(r#"{{"attr":"{attr}","drvPath":"{drv}","outputs":{{"out":"{out}"}}}}"#)
}

fn run(cfg: &Config, ctl: &Control) -> (diff_nixpkgs_attr_core::Report, Vec<Event>) {
    let mut events = Vec::new();
    let report = run_pipeline(cfg, ctl, &mut |event| {
        events.push(event);
        Flow::Continue
    })
    .unwrap();
    (report, events)
}

#[test]
fn compares_changed_outputs_end_to_end() {
    let fx = Fixture {
        dir: tempfile::tempdir().unwrap(),
    };
    fs::create_dir(fx.path("bin")).unwrap();

    // The repository: two commits; the base is the first.
    let repo = fx.path("nixpkgs");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "master"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "two"]);
    let short = git(&repo, &["rev-parse", "--short", "HEAD"]);

    // Package `a` changes, `b` doesn't.
    let (old_a, new_a) = (
        fx.store_path("old-a", "old\n", "same\nold-line\n"),
        fx.store_path("new-a", "new\n", "same\nnew-line\n"),
    );
    let same_b = fx.store_path("same-b", "b\n", "b\n");
    fs::write(
        fx.path("old.jsonl"),
        format!(
            "{}\n{}\n",
            job("a", "/drv/old-a.drv", &old_a),
            job("b", "/drv/b.drv", &same_b)
        ),
    )
    .unwrap();
    fs::write(
        fx.path("new.jsonl"),
        format!(
            "{}\n{}\n",
            job("a", "/drv/new-a.drv", &new_a),
            job("b", "/drv/b.drv", &same_b)
        ),
    )
    .unwrap();

    let dir = fx.path("");
    let dir = dir.display();
    fx.script(
        "nix-eval-jobs",
        &format!(
            r#"for a; do last=$a; done
case "$last" in *rev=*) cat {dir}/old.jsonl;; *) cat {dir}/new.jsonl;; esac
echo "eval-stderr" >&2"#
        ),
    );
    fx.script(
        "nix",
        &format!(
            r#"case "$1" in
  build) echo "$*" >> {dir}/nix-build-args; cat >> {dir}/nix-build-stdin
         echo '@nix {{"action":"msg","level":3,"msg":"built it"}}' >&2;;
  log) cat "$2.log";;
esac"#
        ),
    );
    fx.script(
        "diffoscope",
        r#"for arg; do before=$last; last=$arg; done
printf '\033[31m'; diff -r --no-dereference "$before" "$last"; rc=$?; printf '\033[0m'; exit $rc"#,
    );
    // SAFETY: this is the only test in this binary, so no other thread is
    // reading the environment.
    unsafe {
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                fx.path("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        );
    }

    let out = fx.path("out");
    let mut cfg = Config::new(&repo, vec!["pkgs.set".into()], &out);
    cfg.base = Some("HEAD~1".into());

    // ---- dry run: builds with --dry-run, compares nothing
    cfg.dry_run = true;
    let (report, events) = run(&cfg, &Control::unlimited());
    assert_eq!(report.status, Status::DryRun);
    let would: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::WouldDiff { attr, output, .. } => Some(format!("{attr}[{output}]")),
            _ => None,
        })
        .collect();
    assert_eq!(would, ["set.a[out]"]);
    let args = fs::read_to_string(fx.path("nix-build-args")).unwrap();
    assert_eq!(args.lines().count(), 2, "{args}");
    assert!(
        args.lines()
            .all(|l| l.contains("--stdin") && l.contains("--dry-run")),
        "{args}"
    );
    assert!(!out.exists());
    let stdin = fs::read_to_string(fx.path("nix-build-stdin")).unwrap();
    assert_eq!(stdin, "/drv/old-a.drv^out\n/drv/new-a.drv^out\n");

    // ---- real run
    cfg.dry_run = false;
    let (report, events) = run(&cfg, &Control::new(CancelToken::new(), Default::default()));
    assert_eq!(report.status, Status::Completed);
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].attr, "set.a");
    assert!(report.entries[0].difflog);
    assert_eq!(report.entries[0].diffoscope_outputs, ["out"]);

    let saved = report.saved_to.expect("saved");
    assert_eq!(saved, out.join(format!("differences-set-master-{short}")));
    let difflog = fs::read_to_string(saved.join("set.a/difflog")).unwrap();
    assert!(
        difflog.contains("-old-line") && difflog.contains("+new-line"),
        "{difflog}"
    );
    let diffoscope = fs::read_to_string(saved.join("set.a/diffoscope/out")).unwrap();
    assert!(
        diffoscope.contains("old") && diffoscope.contains("new"),
        "{diffoscope}"
    );
    assert!(
        !diffoscope.contains('\x1b'),
        "colours must not be saved: {diffoscope:?}"
    );

    let statuses: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolFinished { tool, status, .. } => Some((*tool, *status)),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses,
        [
            (Tool::Difflog, ToolStatus::Different),
            (Tool::Diffoscope, ToolStatus::Different)
        ]
    );
    // What the frontend sees is the raw tool output, colours included.
    let shown: Vec<u8> = events
        .iter()
        .filter_map(|e| match e {
            Event::Output {
                tool: Tool::Diffoscope,
                data,
            } => Some(data.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(String::from_utf8_lossy(&shown).starts_with("\x1b[31m"));
    assert!(events.iter().any(|e| matches!(e,
        Event::Build { side: Side::Base, event: BuildEvent::Message { text, .. } } if text == "built it")));
    assert!(events.iter().any(|e| matches!(e,
        Event::ToolLine { tool: "nix-eval-jobs", line } if line == "eval-stderr")));

    // ---- a previous run's results for the package are replaced
    fs::write(saved.join("set.a/stale"), "x").unwrap();
    let (again, _) = run(&cfg, &Control::unlimited());
    assert_eq!(again.status, Status::Completed);
    assert!(!saved.join("set.a/stale").exists());

    // ---- nothing changed: no results, nothing built
    let before = fs::read_to_string(fx.path("nix-build-args")).unwrap();
    fs::copy(fx.path("new.jsonl"), fx.path("old.jsonl")).unwrap();
    let (none, events) = run(&cfg, &Control::unlimited());
    assert_eq!(none.status, Status::NothingChanged);
    assert!(none.entries.is_empty() && none.saved_to.is_none());
    assert_eq!(
        fs::read_to_string(fx.path("nix-build-args")).unwrap(),
        before
    );
    assert!(events.iter().any(|e| matches!(e, Event::Collected { .. })));

    // ---- a cancelled token stops before doing anything
    let token = CancelToken::new();
    token.cancel();
    let (cancelled, _) = run(&cfg, &Control::new(token, Default::default()));
    assert_eq!(cancelled.status, Status::Cancelled);

    // ---- stopping from the event handler ends the run between packages
    fs::write(
        fx.path("old.jsonl"),
        format!("{}\n", job("a", "/drv/old-a.drv", &old_a)),
    )
    .unwrap();
    let stopped_report = run_pipeline(&cfg, &Control::unlimited(), &mut |e| {
        if matches!(e, Event::PackageStarting { .. }) {
            Flow::Stop
        } else {
            Flow::Continue
        }
    })
    .unwrap();
    assert!(stopped_report.stopped_early);
}
