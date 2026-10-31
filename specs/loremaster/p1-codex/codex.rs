use serde_json::Value;

use crate::{Adapter, Entry, ToolCall};

/// Codex CLI hooks and rollout JSONL transcripts.
pub struct CodexAdapter;

impl Adapter for CodexAdapter {
    fn parse_line(&self, line: &Value) -> Vec<Entry> {
        let payload = &line["payload"];
        let entry = match (line["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("user_message")) => text_entry(payload, Entry::Prompt),
            (Some("event_msg"), Some("agent_message")) => text_entry(payload, Entry::Reply),
            (Some("event_msg"), Some("patch_apply_end")) => patch_failure(payload),
            (Some("event_msg"), Some("exec_command_end")) => exec_failure(payload),
            (Some("response_item"), Some("function_call")) => function_call(payload),
            (Some("response_item"), Some("custom_tool_call")) => custom_tool_call(payload),
            (Some("response_item"), Some("local_shell_call")) => local_shell_call(payload),
            (Some("response_item"), Some("function_call_output")) => output_failure(payload),
            _ => None,
        };
        entry.into_iter().collect()
    }
}

fn text_entry(payload: &Value, wrap: fn(String) -> Entry) -> Option<Entry> {
    payload["message"]
        .as_str()
        .map(|text| wrap(text.to_owned()))
}

fn call_id(payload: &Value) -> Option<String> {
    payload["call_id"].as_str().map(str::to_owned)
}

fn tool_call(id: String, tool: &str, command: Option<String>, files: Vec<String>) -> Entry {
    Entry::ToolCall {
        id,
        call: ToolCall {
            tool: tool.to_owned(),
            command,
            files,
            error: None,
        },
    }
}

/// Shell tools pass the command as a string or an argv array, under `command` or `cmd`.
fn command_text(value: &Value) -> Option<String> {
    let command = if value["command"].is_null() {
        &value["cmd"]
    } else {
        &value["command"]
    };
    command.as_str().map(str::to_owned).or_else(|| {
        let parts: Vec<&str> = command
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        Some(parts.join(" "))
    })
}

fn function_call(payload: &Value) -> Option<Entry> {
    let arguments: Value = serde_json::from_str(payload["arguments"].as_str()?).ok()?;
    Some(tool_call(
        call_id(payload)?,
        payload["name"].as_str()?,
        command_text(&arguments),
        Vec::new(),
    ))
}

fn custom_tool_call(payload: &Value) -> Option<Entry> {
    let input = payload["input"].as_str().unwrap_or_default();
    let files = input
        .lines()
        .filter_map(|line| {
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                .iter()
                .find_map(|marker| line.strip_prefix(marker))
        })
        .map(str::to_owned)
        .collect();
    Some(tool_call(
        call_id(payload)?,
        payload["name"].as_str()?,
        None,
        files,
    ))
}

fn local_shell_call(payload: &Value) -> Option<Entry> {
    Some(tool_call(
        call_id(payload)?,
        "local_shell",
        command_text(&payload["action"]),
        Vec::new(),
    ))
}

fn failure(payload: &Value, error: String) -> Option<Entry> {
    Some(Entry::ToolError {
        id: call_id(payload)?,
        error,
    })
}

fn patch_failure(payload: &Value) -> Option<Entry> {
    if payload["success"] != false {
        return None;
    }
    failure(
        payload,
        payload["stderr"].as_str().unwrap_or_default().to_owned(),
    )
}

fn exec_failure(payload: &Value) -> Option<Entry> {
    if payload["exit_code"].as_i64().unwrap_or(0) == 0 {
        return None;
    }
    failure(
        payload,
        payload["aggregated_output"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

/// Shell output reaches the rollout as formatted text; a failure shows as a non-zero exit line.
fn output_failure(payload: &Value) -> Option<Entry> {
    let output = payload["output"].as_str()?;
    // Only the first such line is the header; later ones are the command's own output.
    let code = output.lines().find_map(|line| {
        ["Exit code: ", "Process exited with code "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
    })?;
    if code.trim() == "0" {
        return None;
    }
    failure(payload, output.to_owned())
}
