//! Installs Loremaster's hook entries into an agent's configuration.

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::config::{Paths, create_private_dir};

/// An agent `enable` can configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    ClaudeCode,
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
        }
    }
}

/// Why hook entries could not be installed. The settings file is left as it was.
#[derive(Debug, Error)]
pub enum EnableError {
    #[error("{0}")]
    AgentMissing(String),
    #[error("cannot parse {path}: {reason}")]
    Parse { path: String, reason: String },
    #[error("cannot read or write {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// `(event, timeout seconds, async)`.
const EVENTS: [(&str, u64, bool); 4] = [
    ("SessionStart", 5, false),
    ("UserPromptSubmit", 2, false),
    ("Stop", 10, true),
    ("SessionEnd", 3, false),
];

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> EnableError + '_ {
    move |source| EnableError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn hook_entry(prefix: &str, agent: Agent, event: &str, timeout: u64, is_async: bool) -> Value {
    let command = format!("{prefix}{} {event}", agent.name());
    let handler = if is_async {
        json!({"async": true, "command": command, "timeout": timeout, "type": "command"})
    } else {
        json!({"command": command, "timeout": timeout, "type": "command"})
    };
    json!({"hooks": [handler]})
}

/// Takes our handlers out of an event's groups, dropping groups they leave empty.
fn without_ours(groups: Vec<Value>, prefix: &str) -> Vec<Value> {
    let is_ours = |handler: &Value| {
        handler["command"]
            .as_str()
            .is_some_and(|command| command.starts_with(prefix))
    };
    groups
        .into_iter()
        .filter_map(|mut group| {
            let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return Some(group);
            };
            let before = handlers.len();
            handlers.retain(|handler| !is_ours(handler));
            (handlers.len() == before || !handlers.is_empty()).then_some(group)
        })
        .collect()
}

/// Merges our entries into settings text. Entries from other tools stay. Output is deterministic, so
/// running this again changes nothing.
pub fn merge_settings(
    existing: Option<&str>,
    prefix: &str,
    agent: Agent,
    path: &Path,
) -> Result<String, EnableError> {
    let parse_error = |reason: String| EnableError::Parse {
        path: path.display().to_string(),
        reason,
    };
    let mut settings: Map<String, Value> = match existing {
        Some(text) => serde_json::from_str(text).map_err(|e| parse_error(e.to_string()))?,
        None => Map::new(),
    };
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| parse_error("field `hooks` is not an object".to_owned()))?;
    for (event, timeout, is_async) in EVENTS {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = groups else {
            return Err(parse_error(format!(
                "field `hooks.{event}` is not an array"
            )));
        };
        let mut merged = without_ours(std::mem::take(groups), prefix);
        merged.push(hook_entry(prefix, agent, event, timeout, is_async));
        *groups = merged;
    }
    let mut text = serde_json::to_string_pretty(&settings).expect("JSON values always serialize");
    text.push('\n');
    Ok(text)
}

/// Writes via a temp file and rename, so a crash leaves the old settings whole. Follows a symlink
/// first, so dotfile managers keep working.
fn write_atomically(path: &Path, text: &str) -> Result<(), EnableError> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let dir = target.parent().unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(io_error(dir))?;
    temp.write_all(text.as_bytes()).map_err(io_error(&target))?;
    temp.as_file().sync_all().map_err(io_error(&target))?;
    // A temp file is private; a settings file keeps whatever access it had.
    if let Ok(existing) = fs::metadata(&target) {
        temp.as_file()
            .set_permissions(existing.permissions())
            .map_err(io_error(&target))?;
    }
    temp.persist(&target)
        .map_err(|e| e.error)
        .map_err(io_error(&target))?;
    Ok(())
}

fn settings_path(agent: Agent, user_home: &Path) -> Result<PathBuf, EnableError> {
    let (dir, file, label) = match agent {
        Agent::ClaudeCode => (".claude", "settings.json", "Claude Code"),
    };
    let agent_dir = user_home.join(dir);
    if !agent_dir.is_dir() {
        return Err(EnableError::AgentMissing(format!(
            "{label} not found (~/{dir} missing)"
        )));
    }
    Ok(agent_dir.join(file))
}

/// Puts this binary where hook entries point, unless something is already there.
fn link_binary(paths: &Paths) -> Result<(), EnableError> {
    let binary = paths.home().join("bin/loremaster");
    if binary.exists() {
        return Ok(());
    }
    // Nothing usable is there, but a link whose target is gone would still block the new one.
    if binary.is_symlink() {
        fs::remove_file(&binary).map_err(io_error(&binary))?;
    }
    let bin = paths.home().join("bin");
    create_private_dir(&bin).map_err(io_error(&bin))?;
    let exe = std::env::current_exe().map_err(io_error(&binary))?;
    symlink(exe, &binary).map_err(io_error(&binary))
}

/// Quotes a path for the shell that runs hook commands, when it needs it.
fn shell_quote(text: &str) -> String {
    let is_plain = |c: char| c.is_ascii_alphanumeric() || "/._-+=:@%".contains(c);
    if text.chars().all(is_plain) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// Installs hook entries for `agent` and returns what to tell the user.
pub fn enable(agent: Agent, paths: &Paths, user_home: &Path) -> Result<String, EnableError> {
    let path = settings_path(agent, user_home)?;
    let existing = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_error(&path)(e)),
    };
    link_binary(paths)?;
    let binary = paths.home().join("bin/loremaster");
    let quoted = shell_quote(&binary.display().to_string());
    // An agent started elsewhere must reach this same home, not the default one.
    let prefix = if paths.home() == user_home.join(".loremaster") {
        format!("{quoted} hook ")
    } else {
        let home = shell_quote(&paths.home().display().to_string());
        format!("LOREMASTER_HOME={home} {quoted} hook ")
    };
    let text = merge_settings(existing.as_deref(), &prefix, agent, &path)?;
    write_atomically(&path, &text)?;
    Ok(format!("Enabled Loremaster hooks in {}", path.display()))
}
