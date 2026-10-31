//! The short-lived process an agent runs for each hook. It relays to the daemon and never makes
//! the agent wait or fail.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use loremaster_agents::{Adapter, CoreEvent, adapter_for};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Verb;
use crate::config::Paths;
use crate::spool::{self, SpoolEntry};

fn verb_for(event: &str) -> Option<(Verb, Duration)> {
    match event {
        "SessionStart" => Some((Verb::SessionStart, Duration::from_secs(1))),
        "UserPromptSubmit" => Some((Verb::Recall, Duration::from_millis(90))),
        "Stop" => Some((Verb::Capture, Duration::from_millis(40))),
        "SessionEnd" => Some((Verb::SessionEnd, Duration::from_secs(1))),
        _ => None,
    }
}

#[derive(Deserialize)]
struct Reply {
    additional_context: Option<String>,
}

fn ask_daemon(sock: &Path, request: &str, deadline: Duration) -> std::io::Result<Reply> {
    let stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(deadline))?;
    stream.set_write_timeout(Some(deadline))?;
    (&stream).write_all(request.as_bytes())?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// Keeps a pointer to the turn so the daemon can pick it up when it next starts.
fn spool_event(paths: &Paths, adapter: &dyn Adapter, agent: &str, verb: Verb, input: &str) {
    let (session_id, cwd, transcript_path) = match adapter.parse_event(input) {
        Ok(CoreEvent::Capture {
            session_id,
            cwd,
            transcript_path,
            ..
        })
        | Ok(CoreEvent::SessionEnd {
            session_id,
            cwd,
            transcript_path,
        }) => (session_id, cwd, transcript_path),
        _ => return,
    };
    let entry = SpoolEntry {
        agent: agent.to_owned(),
        verb,
        session_id,
        transcript_path,
        cwd,
    };
    if let Err(e) = spool::append(&paths.spool(), &entry) {
        eprintln!("loremaster: cannot write spool: {e}");
    }
}

/// Handles one hook call and returns what to print for the agent, if anything.
pub fn run(paths: &Paths, agent: &str, event: &str, input: &str) -> Option<String> {
    let adapter = adapter_for(agent)?;
    let (verb, deadline) = verb_for(event)?;
    let parsed: Value = serde_json::from_str(input).ok()?;
    let request = format!(
        "{}\n",
        json!({"verb": verb, "agent": agent, "event": parsed})
    );
    match ask_daemon(&paths.sock(), &request, deadline) {
        Ok(reply) => reply
            .additional_context
            .map(|text| adapter.render_context(event, &text)),
        Err(_) => {
            if matches!(verb, Verb::Capture | Verb::SessionEnd) {
                spool_event(paths, adapter, agent, verb, input);
            }
            None
        }
    }
}
