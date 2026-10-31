//! The local-run helper script.
#![cfg(feature = "dev")]

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::Sandbox;

fn repo_file(path: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
        .display()
        .to_string()
}

fn try_hook(configure: impl FnOnce(&mut Command)) -> Output {
    let mut command = Command::new(repo_file("scripts/try-hook.sh"));
    command.env("LOREMASTER_BIN", env!("CARGO_BIN_EXE_loremaster"));
    configure(&mut command);
    command.output().unwrap()
}

#[test]
fn refuses_to_run_without_a_sandbox() {
    let output = try_hook(|c| {
        c.env_remove("LOREMASTER_HOME");
    });
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "error: set LOREMASTER_HOME to a sandbox dir"
    );
}

#[test]
fn refuses_the_real_home() {
    let output = try_hook(|c| {
        c.env("HOME", "/Users/x")
            .env("LOREMASTER_HOME", "/Users/x/.loremaster");
    });
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "error: refusing to use the real Loremaster home"
    );
}

#[test]
fn refuses_the_real_home_with_a_trailing_slash() {
    let output = try_hook(|c| {
        c.env("HOME", "/Users/x")
            .env("LOREMASTER_HOME", "/Users/x/.loremaster/");
    });
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "error: refusing to use the real Loremaster home"
    );
}

#[test]
fn reports_a_stopped_daemon() {
    let sandbox = Sandbox::without_model();
    let output = try_hook(|c| {
        c.env("LOREMASTER_HOME", sandbox.paths.home());
    });
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "error: daemon not running at $LOREMASTER_HOME/sock"
    );
}

#[test]
fn shows_profile_recall_and_a_redacted_turn_for_both_agents() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    sandbox.wait_for_embedder();
    let seed = Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .args(["dev", "seed", &repo_file("scripts/samples.jsonl")])
        .env("LOREMASTER_HOME", sandbox.paths.home())
        .output()
        .unwrap();
    assert!(seed.status.success(), "{seed:?}");

    let output = try_hook(|c| {
        c.env("LOREMASTER_HOME", sandbox.paths.home());
    });
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains("== claude-code SessionStart"), "{stdout}");
    assert!(
        stdout.contains("Loremaster memory from past sessions"),
        "{stdout}"
    );
    assert!(stdout.contains("[REDACTED:github-pat]"), "{stdout}");
    assert!(!stdout.contains("ghp_"), "{stdout}");
    assert!(stdout.contains("(no output)"), "{stdout}");
    assert!(!stdout.contains("OLD-SUPERSEDED"), "{stdout}");
    assert!(!stdout.contains("OLD-TOMBSTONED"), "{stdout}");
}
