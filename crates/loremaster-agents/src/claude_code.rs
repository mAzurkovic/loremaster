use serde_json::Value;

use crate::{Adapter, Entry, ToolCall};

/// Claude Code hooks and JSONL transcripts.
pub struct ClaudeCodeAdapter;

impl Adapter for ClaudeCodeAdapter {
    fn parse_line(&self, line: &Value) -> Vec<Entry> {
        let is_true = |field: &str| line[field].as_bool().unwrap_or(false);
        if is_true("isSidechain")
            || is_true("isMeta")
            || is_true("isCompactSummary")
            || is_true("isApiErrorMessage")
        {
            return Vec::new();
        }
        let content = &line["message"]["content"];
        match line["type"].as_str() {
            Some("user") => user_entries(content),
            Some("attachment") => queued_prompt(&line["attachment"]).into_iter().collect(),
            Some("assistant") => content
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(assistant_entry)
                .collect(),
            _ => Vec::new(),
        }
    }
}

fn user_entries(content: &Value) -> Vec<Entry> {
    if let Some(text) = content.as_str() {
        return if is_system_text(text) {
            Vec::new()
        } else {
            vec![Entry::Prompt(text.to_owned())]
        };
    }
    let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
    let text = block_text(blocks);
    let errors = blocks.iter().filter_map(|block| {
        let is_error = block["type"] == "tool_result" && block["is_error"] == true;
        is_error.then(|| Entry::ToolError {
            id: block["tool_use_id"].as_str().unwrap_or_default().to_owned(),
            error: tool_result_text(&block["content"]),
        })
    });
    (!text.is_empty() && !is_system_text(&text))
        .then(|| Entry::Prompt(text.clone()))
        .into_iter()
        .chain(errors)
        .collect()
}

/// A prompt the user typed while the agent was working. It may have no `user` line of its own.
fn queued_prompt(attachment: &Value) -> Option<Entry> {
    // Harness events carry their own origin. A line with no origin at all is taken as the user's.
    let origin = &attachment["origin"]["kind"];
    let is_human_prompt = attachment["type"] == "queued_command"
        && attachment["commandMode"] == "prompt"
        && (origin.is_null() || origin == "human");
    is_human_prompt
        .then(|| attachment["prompt"].as_str())
        .flatten()
        .map(|text| Entry::Prompt(text.to_owned()))
}

/// Lines the harness writes in the user's place. A slash command with arguments stays, because the
/// arguments are what the user asked for.
fn is_system_text(text: &str) -> bool {
    text.starts_with("<task-notification>")
        || text.starts_with("[Request interrupted")
        || text.starts_with("<local-command-")
        || (text.starts_with("<command-") && text.contains("<command-args></command-args>"))
}

fn assistant_entry(block: &Value) -> Option<Entry> {
    match block["type"].as_str()? {
        "text" => Some(Entry::Reply(block["text"].as_str()?.to_owned())),
        "tool_use" => {
            let input = &block["input"];
            let path = input["file_path"]
                .as_str()
                .or(input["notebook_path"].as_str());
            let tool = block["name"].as_str()?;
            let edits_files = matches!(tool, "Edit" | "MultiEdit" | "Write" | "NotebookEdit");
            Some(Entry::ToolCall {
                id: block["id"].as_str()?.to_owned(),
                call: ToolCall {
                    tool: tool.to_owned(),
                    command: input["command"].as_str().map(str::to_owned),
                    files: path
                        .filter(|_| edits_files)
                        .map(str::to_owned)
                        .into_iter()
                        .collect(),
                    error: None,
                },
            })
        }
        _ => None,
    }
}

fn block_text(blocks: &[Value]) -> String {
    let texts: Vec<&str> = blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    texts.join("\n")
}

fn tool_result_text(content: &Value) -> String {
    content.as_str().map_or_else(
        || block_text(content.as_array().map(Vec::as_slice).unwrap_or_default()),
        str::to_owned,
    )
}
