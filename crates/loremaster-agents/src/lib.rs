//! Maps each agent's hook events and transcript format onto a common model.

mod claude_code;
mod turn;

use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

pub use claude_code::ClaudeCodeAdapter;
pub use turn::{ToolCall, Turn};

/// Why an agent event or transcript could not be read.
#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("hook input is not valid JSON")]
    InvalidJson,
    #[error("unsupported hook event: {0}")]
    UnsupportedEvent(String),
    #[error("hook input has no {0}")]
    MissingField(&'static str),
    #[error("transcript not found: {0}")]
    TranscriptNotFound(PathBuf),
    #[error("cannot read transcript: {0}")]
    TranscriptIo(#[from] io::Error),
    #[error("event has neither a transcript nor a final message")]
    EmptyTurn,
}

/// An agent lifecycle event, independent of the agent that reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    SessionStart {
        session_id: String,
        cwd: PathBuf,
        transcript_path: Option<PathBuf>,
        source: Option<String>,
    },
    Recall {
        session_id: String,
        cwd: PathBuf,
        prompt: String,
    },
    Capture {
        session_id: String,
        cwd: PathBuf,
        transcript_path: Option<PathBuf>,
        last_assistant_message: Option<String>,
    },
    SessionEnd {
        session_id: String,
        cwd: PathBuf,
        transcript_path: Option<PathBuf>,
    },
}

/// Whether the last turn found by a capture read is safe to consume, given that consuming it means
/// anything the agent writes for it later has nowhere to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastTurn {
    /// No later read will come, so take whatever is there.
    Finished,
    /// A turn just ended. With no final message, a turn with no reply yet may still be being written.
    MaybeRunning,
    /// The agent may be mid-turn, and any text so far may not be its reply.
    Running,
}

/// What one transcript line contributes to a turn.
#[derive(Debug, PartialEq, Eq)]
pub enum Entry {
    Prompt(String),
    Reply(String),
    ToolCall { id: String, call: ToolCall },
    ToolError { id: String, error: String },
}

/// Connects Loremaster to one agent.
pub trait Adapter: Sync {
    /// Reads one line of this agent's transcript format.
    fn parse_line(&self, line: &Value) -> Vec<Entry>;

    /// Parses hook input from the agent. Unknown fields are ignored, so agent upgrades that add
    /// fields keep working.
    fn parse_event(&self, json: &str) -> Result<CoreEvent, AdapterError> {
        let value: Value = serde_json::from_str(json).map_err(|_| AdapterError::InvalidJson)?;
        let event = value["hook_event_name"]
            .as_str()
            .ok_or(AdapterError::MissingField("hook_event_name"))?;
        let required = |field: &'static str| {
            value[field]
                .as_str()
                .map(str::to_owned)
                .ok_or(AdapterError::MissingField(field))
        };
        let optional = |field: &str| value[field].as_str().map(str::to_owned);
        let session_id = required("session_id")?;
        let cwd = PathBuf::from(required("cwd")?);
        let transcript_path = optional("transcript_path").map(PathBuf::from);
        match event {
            "SessionStart" => Ok(CoreEvent::SessionStart {
                session_id,
                cwd,
                transcript_path,
                source: optional("source"),
            }),
            "UserPromptSubmit" => Ok(CoreEvent::Recall {
                session_id,
                cwd,
                prompt: required("prompt")?,
            }),
            "Stop" => Ok(CoreEvent::Capture {
                session_id,
                cwd,
                transcript_path,
                last_assistant_message: optional("last_assistant_message"),
            }),
            "SessionEnd" => Ok(CoreEvent::SessionEnd {
                session_id,
                cwd,
                transcript_path,
            }),
            other => Err(AdapterError::UnsupportedEvent(other.to_owned())),
        }
    }

    /// Reads the next turn at or after byte `offset`, returning it with the offset to resume from.
    /// A turn ends where the next prompt starts. A trailing partial line is left for a later read.
    /// An offset past the end of the file means the agent replaced it, so reading restarts at 0.
    fn read_turn(&self, path: &Path, offset: u64) -> Result<(Turn, u64), AdapterError> {
        let mut file = File::open(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => AdapterError::TranscriptNotFound(path.to_owned()),
            _ => AdapterError::TranscriptIo(e),
        })?;
        let mut position = if offset > file.metadata()?.len() {
            0
        } else {
            offset
        };
        file.seek(SeekFrom::Start(position))?;
        let mut reader = BufReader::new(file);

        let mut turn = Turn::default();
        let mut call_ids: Vec<String> = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            reader.read_until(b'\n', &mut line)?;
            if line.last() != Some(&b'\n') {
                break;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                tracing::warn!(path = %path.display(), position, "skipped invalid transcript line");
                position += line.len() as u64;
                continue;
            };
            let entries = self.parse_line(&value);
            // An agent may record one prompt twice in a row. Once the turn has any response, the same
            // text is the user asking again.
            let has_response = !turn.reply.is_empty() || !turn.tool_calls.is_empty();
            let starts_next_turn = entries
                .iter()
                .any(|e| matches!(e, Entry::Prompt(text) if has_response || *text != turn.prompt));
            if starts_next_turn && !turn.is_empty() {
                break;
            }
            for entry in entries {
                turn.apply(entry, &mut call_ids);
            }
            position += line.len() as u64;
        }
        Ok((turn, position))
    }

    /// Reads every turn not yet captured, for a turn-end event. Each turn comes with the offset to
    /// resume from after it, followed by the final offset. The agent's `last_message` is preferred
    /// for the last turn's reply because the transcript may lag behind it. Output flushed after an
    /// earlier capture, with no prompt or tool calls of its own, adds nothing and is dropped.
    /// `last_turn` says whether the last turn read is consumed or left for a later read.
    fn read_captured(
        &self,
        path: Option<&Path>,
        offset: u64,
        last_message: Option<&str>,
        last_turn: LastTurn,
    ) -> Result<(Vec<(Turn, u64)>, u64), AdapterError> {
        let Some(path) = path else {
            let reply = last_message.ok_or(AdapterError::EmptyTurn)?;
            tracing::warn!("event has no transcript; capturing the final message alone");
            let turn = Turn {
                reply: reply.to_owned(),
                ..Turn::default()
            };
            return Ok((vec![(turn, offset)], offset));
        };
        let mut turns = Vec::new();
        let mut position = offset;
        loop {
            let (turn, next) = self.read_turn(path, position)?;
            if turn.is_empty() && next == position {
                break;
            }
            position = next;
            if !turn.prompt.is_empty() || !turn.tool_calls.is_empty() {
                turns.push((turn, position));
            }
        }
        let hold_back = match (last_turn, turns.last()) {
            (LastTurn::Running, Some(_)) => true,
            (LastTurn::MaybeRunning, Some((last, _))) => {
                last_message.is_none() && last.reply.is_empty()
            }
            _ => false,
        };
        if hold_back {
            turns.pop();
            position = turns.last().map_or(offset, |(_, resume)| *resume);
        }
        // A prompt queued whiley in the transcript. The final
        // message belongs to the previous turn, so leave this one for the next read.
        if let ([.., (previous, resume), _], Some(message)) = (turns.as_slice(), last_message)
            && previous.reply == message
        {
            position = *resume;
            turns.pop();
        }
        if let (Some((turn, _)), Some(message)) = (turns.last_mut(), last_message) {
            turn.reply = message.to_owned();
        }
        Ok((turns, position))
    }

    /// Renders text for injection in the form the agent reads from hook stdout.
    fn render_context(&self, event: &str, text: &str) -> String {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Specific<'a> {
            hook_event_name: &'a str,
            additional_context: &'a str,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Output<'a> {
            hook_specific_output: Specific<'a>,
        }
        let output = Output {
            hook_specific_output: Specific {
                hook_event_name: event,
                additional_context: text,
            },
        };
        serde_json::to_string(&output).expect("string fields always serialize")
    }
}

/// Returns the adapter for an agent name used in hook commands.
pub fn adapter_for(agent: &str) -> Option<&'static dyn Adapter> {
    match agent {
        "claude-code" => Some(&ClaudeCodeAdapter),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_for_knows_only_supported_agents() {
        let claude = adapter_for("claude-code").unwrap();
        let line = serde_json::json!({"type":"user","message":{"content":"hi"}});
        assert_eq!(claude.parse_line(&line), [Entry::Prompt("hi".into())]);
        assert!(adapter_for("codex").is_none());
        assert!(adapter_for("cursor").is_none());
    }

    #[test]
    fn parse_event_rejects_bad_input() {
        let adapter = ClaudeCodeAdapter;
        assert!(matches!(
            adapter.parse_event("not json"),
            Err(AdapterError::InvalidJson)
        ));
        assert!(matches!(
            adapter.parse_event(r#"{"hook_event_name":"PreToolUse","session_id":"s","cwd":"/x"}"#),
            Err(AdapterError::UnsupportedEvent(e)) if e == "PreToolUse"
        ));
        assert!(matches!(
            adapter.parse_event(r#"{"hook_event_name":"Stop","cwd":"/x"}"#),
            Err(AdapterError::MissingField("session_id"))
        ));
        assert!(matches!(
            adapter.parse_event(r#"{"session_id":"s","cwd":"/x"}"#),
            Err(AdapterError::MissingField("hook_event_name"))
        ));
        assert!(matches!(
            adapter.parse_event(r#"{"hook_event_name":"Stop","session_id":"s"}"#),
            Err(AdapterError::MissingField("cwd"))
        ));
        assert!(matches!(
            adapter.parse_event(
                r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","cwd":"/x"}"#
            ),
            Err(AdapterError::MissingField("prompt"))
        ));
    }

    #[test]
    fn parse_event_ignores_unknown_fields() {
        let event = ClaudeCodeAdapter
            .parse_event(
                r#"{"hook_event_name":"SessionEnd","session_id":"s","cwd":"/x","brand_new":{"a":1}}"#,
            )
            .unwrap();
        assert_eq!(
            event,
            CoreEvent::SessionEnd {
                session_id: "s".into(),
                cwd: "/x".into(),
                transcript_path: None,
            }
        );
    }

    #[test]
    fn read_turn_reports_a_directory_as_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            ClaudeCodeAdapter.read_turn(dir.path(), 0),
            Err(AdapterError::TranscriptIo(_))
        ));
    }
}
