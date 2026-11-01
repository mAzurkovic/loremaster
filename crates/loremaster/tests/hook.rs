//! The hook process, as an agent runs it.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use common::{Sandbox, acme_web_repo, embedder, hook_input, memory};
use loremaster::now_ms;
use loremaster_core::Kind;

const DEPLOY: &str = "Deploys go through ArgoCD, not manual kubectl";

fn assert_silent_success(output: &std::process::Output) {
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}

/// A fake daemon replying with `reply` after `delay`.
fn fake_daemon(sandbox: &Sandbox, delay: Duration, reply: &'static str) {
    let listener = UnixListener::bind(sandbox.paths.sock()).unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            thread::sleep(delay);
            let _ = (&stream).write_all(reply.as_bytes());
        }
    });
}

#[test]
fn daemon_up_prompt_prints_the_context_for_the_agent() {
    let sandbox = Sandbox::with_model();
    let repo = acme_web_repo(sandbox.dir.path());
    sandbox
        .store()
        .put_memory(&memory(
            &embedder(),
            Some(Kind::Decision),
            Some("github.com/acme/web"),
            DEPLOY,
            now_ms(),
        ))
        .unwrap();
    let _daemon = sandbox.start_daemon();
    sandbox.wait_for_embedder();
    let agent = "claude-code";
    let input = hook_input(
        agent,
        "user_prompt_submit.json",
        sandbox.dir.path(),
        Some(&repo),
    );
    let output = sandbox.hook(agent, "UserPromptSubmit", &input);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!(
            "{{\"hookSpecificOutput\":{{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"Loremaster memories relevant to this prompt:\\n- [decision] {DEPLOY}\"}}}}"
        )
    );
}

#[test]
fn daemon_down_prompt_is_silent_and_writes_no_spool() {
    let sandbox = Sandbox::without_model();
    let agent = "claude-code";
    let input = hook_input(agent, "user_prompt_submit.json", sandbox.dir.path(), None);
    assert_silent_success(&sandbox.hook(agent, "UserPromptSubmit", &input));
    assert!(!sandbox.paths.spool().exists());
}

#[test]
fn daemon_down_stop_spools_a_pointer_without_text() {
    let sandbox = Sandbox::without_model();
    let dir = sandbox.dir.path();
    let input = hook_input("claude-code", "stop.json", dir, None);
    assert_silent_success(&sandbox.hook("claude-code", "Stop", &input));
    let event: serde_json::Value = serde_json::from_str(&input).unwrap();
    let expected = serde_json::json!({
        "agent": "claude-code",
        "verb": "capture",
        "session_id": event["session_id"],
        "transcript_path": event["transcript_path"],
        "cwd": event["cwd"],
    });
    let text = fs::read_to_string(sandbox.paths.spool()).unwrap();
    let line: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(line, expected);
    assert!(!text.contains("Set a to 2"));
}

#[test]
fn daemon_down_session_end_also_spools() {
    let sandbox = Sandbox::without_model();
    let input = hook_input("claude-code", "session_end.json", sandbox.dir.path(), None);
    assert_silent_success(&sandbox.hook("claude-code", "SessionEnd", &input));
    let text = fs::read_to_string(sandbox.paths.spool()).unwrap();
    assert!(text.contains("\"verb\":\"session_end\""), "{text}");
}

#[test]
fn slow_daemon_is_abandoned_at_the_deadline() {
    let sandbox = Sandbox::without_model();
    fake_daemon(
        &sandbox,
        Duration::from_millis(200),
        "{\"additional_context\":\"late\"}\n",
    );
    let input = hook_input(
        "claude-code",
        "user_prompt_submit.json",
        sandbox.dir.path(),
        None,
    );
    let output = sandbox.hook("claude-code", "UserPromptSubmit", &input);
    assert_silent_success(&output);
}

#[test]
fn slow_daemon_on_stop_spools_the_turn() {
    let sandbox = Sandbox::without_model();
    fake_daemon(&sandbox, Duration::from_millis(200), "{}\n");
    let input = hook_input("claude-code", "stop.json", sandbox.dir.path(), None);
    assert_silent_success(&sandbox.hook("claude-code", "Stop", &input));
    assert!(sandbox.paths.spool().exists());
}

#[test]
fn bad_daemon_reply_is_silent() {
    let sandbox = Sandbox::without_model();
    fake_daemon(&sandbox, Duration::ZERO, "not json\n");
    let input = hook_input(
        "claude-code",
        "user_prompt_submit.json",
        sandbox.dir.path(),
        None,
    );
    assert_silent_success(&sandbox.hook("claude-code", "UserPromptSubmit", &input));
}

#[test]
fn unusable_invocations_are_silent() {
    let sandbox = Sandbox::without_model();
    let input = hook_input("claude-code", "stop.json", sandbox.dir.path(), None);
    for (agent, event, stdin) in [
        ("cursor", "Stop", input.as_str()),
        ("claude-code", "PreToolUse", input.as_str()),
        ("claude-code", "Stop", ""),
        ("claude-code", "Stop", "not json"),
        ("claude-code", "Stop", "{}"),
    ] {
        assert_silent_success(&sandbox.hook(agent, event, stdin));
    }
    assert!(!sandbox.paths.spool().exists());
}

#[test]
fn unwritable_spool_is_silent_on_stdout_and_reported_on_stderr() {
    let sandbox = Sandbox::without_model();
    let home = sandbox.dir.path().join("locked");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap();
    let input = hook_input("claude-code", "stop.json", sandbox.dir.path(), None);
    let mut child = Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .args(["hook", "claude-code", "Stop"])
        .env("LOREMASTER_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    assert_silent_success(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot write spool"));
}

#[test]
fn no_home_at_all_is_still_silent() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .args(["hook", "claude-code", "Stop"])
        .env_remove("LOREMASTER_HOME")
        .env_remove("HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    assert_silent_success(&child.wait_with_output().unwrap());
}

#[test]
fn closed_stdout_is_still_a_clean_exit() {
    let sandbox = Sandbox::without_model();
    fake_daemon(
        &sandbox,
        Duration::ZERO,
        "{\"additional_context\":\"text\"}\n",
    );
    let input = hook_input(
        "claude-code",
        "session_start.json",
        sandbox.dir.path(),
        None,
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .args(["hook", "claude-code", "SessionStart"])
        .env("LOREMASTER_HOME", sandbox.paths.home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
}
