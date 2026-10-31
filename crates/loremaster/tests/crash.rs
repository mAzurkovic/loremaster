//! No captured turn is lost or duplicated when the daemon dies.

mod common;

use std::fs;
use std::time::Duration;

use common::{Sandbox, hook_input, request, wait_for};
use loremaster_agents::adapter_for;

/// Byte offsets of the end of each turn in an agent's fixture transcript.
fn turn_ends(agent: &str) -> Vec<u64> {
    let path = common::fixture_dir(agent).join("transcript.jsonl");
    let adapter = adapter_for(agent).unwrap();
    let mut ends = Vec::new();
    let mut offset = 0;
    loop {
        let (turn, next) = adapter.read_turn(&path, offset).unwrap();
        if turn.is_empty() {
            return ends;
        }
        ends.push(next);
        offset = next;
    }
}

fn write_transcript_through(agent: &str, dir: &std::path::Path, end: u64) {
    let bytes = fs::read(common::fixture_dir(agent).join("transcript.jsonl")).unwrap();
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("transcript.jsonl"), &bytes[..end as usize]).unwrap();
}

fn assert_silent(output: &std::process::Output) {
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn daemon_killed_mid_session_loses_no_turn() {
    let agent = "claude-code";
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    let ends = turn_ends(agent);
    assert_eq!(ends.len(), 3);
    let stop = hook_input(agent, "stop.json", &dir, None);
    let session_end = hook_input(agent, "session_end.json", &dir, None);

    write_transcript_through(agent, &dir, ends[0]);
    let daemon = sandbox.start_daemon();
    assert_eq!(sandbox.ask(&request("capture", agent, &stop)), "{}");
    wait_for(|| sandbox.turns().len() == 1, "first turn");
    daemon.kill9();

    write_transcript_through(agent, &dir, ends[1]);
    assert_silent(&sandbox.hook(agent, "Stop", &stop));
    write_transcript_through(agent, &dir, ends[2]);
    assert_silent(&sandbox.hook(agent, "SessionEnd", &session_end));
    let spooled = fs::read_to_string(sandbox.paths.spool()).unwrap();
    assert_eq!(spooled.lines().count(), 2, "{agent}");

    let _daemon = sandbox.start_daemon();
    let turns = sandbox.turns();
    assert_eq!(turns.len(), 3, "{agent}");
    for (turn, prompt) in turns.iter().zip([
        "run the tests",
        "clone the deploy repo",
        "update the config",
    ]) {
        assert!(turn.contains(prompt), "{turn}");
    }
    assert!(!sandbox.paths.spool().exists());
    assert_eq!(
        sandbox.query_i64("SELECT coalesce(max(ended), 0) FROM sessions"),
        1
    );
    assert!(!turns.iter().any(|turn| turn.contains("ghp_")));
}

#[test]
fn daemon_killed_between_reply_and_commit_queues_the_turn_once() {
    let agent = "claude-code";
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    let ends = turn_ends(agent);
    write_transcript_through(agent, &dir, ends[0]);
    let stop = hook_input(agent, "stop.json", &dir, None);

    let daemon = sandbox.start_daemon();
    // Holding the write lock leaves the daemon stuck just before its commit.
    let blocker = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(sandbox.ask(&request("capture", agent, &stop)), "{}");
    std::thread::sleep(Duration::from_millis(100));
    daemon.kill9();
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(sandbox.query_i64("SELECT count(*) FROM turn_queue"), 0);

    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.ask(&request("capture", agent, &stop)), "{}");
    wait_for(|| sandbox.turns().len() == 1, "turn queued after restart");
    assert_eq!(sandbox.ask(&request("capture", agent, &stop)), "{}");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(sandbox.turns().len(), 1);
}
