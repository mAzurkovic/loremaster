use loremaster_core::estimate_tokens;
use serde::Serialize;

use crate::Entry;

const MAX_FIELD_CHARS: usize = 500;
const MAX_TOKENS: usize = 32_000;

/// One tool the agent used. Output, file contents and diffs are never kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolCall {
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One user prompt plus the agent's response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Turn {
    pub prompt: String,
    pub tool_calls: Vec<ToolCall>,
    pub reply: String,
}

impl Turn {
    /// True when nothing has been read into this turn.
    pub fn is_empty(&self) -> bool {
        self.prompt.is_empty() && self.tool_calls.is_empty() && self.reply.is_empty()
    }

    pub(crate) fn apply(&mut self, entry: Entry, call_ids: &mut Vec<String>) {
        match entry {
            Entry::Prompt(text) => self.prompt = text,
            Entry::Reply(text) => self.reply = text,
            Entry::ToolCall { id, call } => {
                call_ids.push(id);
                self.tool_calls.push(call);
            }
            Entry::ToolError { id, error } => {
                if let Some(index) = call_ids.iter().position(|known| *known == id) {
                    self.tool_calls[index].error = Some(error);
                }
            }
        }
    }

    /// Applies `f` to every text field. Redaction runs through this before [`Turn::prepare`],
    /// because truncating first could cut a secret into a form its rule no longer matches.
    pub fn map_text(&mut self, mut f: impl FnMut(&str) -> String) {
        self.prompt = f(&self.prompt);
        self.reply = f(&self.reply);
        for call in &mut self.tool_calls {
            call.command = call.command.as_deref().map(&mut f);
            call.error = call.error.as_deref().map(&mut f);
            for file in &mut call.files {
                *file = f(file);
            }
        }
    }

    /// Condenses the turn to JSON within the size limits, trimming oldest content first.
    pub fn prepare(mut self) -> String {
        for call in &mut self.tool_calls {
            for text in [&mut call.command, &mut call.error].into_iter().flatten() {
                *text = text.chars().take(MAX_FIELD_CHARS).collect();
            }
        }
        while estimate_tokens(&to_json(&self)) > MAX_TOKENS && !self.tool_calls.is_empty() {
            let excess = to_json(&self).chars().count() - MAX_TOKENS * 3;
            let mut removed = 0;
            let drop_count = self
                .tool_calls
                .iter()
                .take_while(|call| {
                    let keep_going = removed < excess;
                    removed += to_json(call).chars().count() + 1;
                    keep_going
                })
                .count();
            self.tool_calls.drain(..drop_count);
        }
        loop {
            let json = to_json(&self);
            let chars = json.chars().count();
            if chars <= MAX_TOKENS * 3 {
                return json;
            }
            let excess = chars - MAX_TOKENS * 3;
            let target = if self.prompt.is_empty() {
                &mut self.reply
            } else {
                &mut self.prompt
            };
            *target = target.chars().skip(excess).collect();
        }
    }
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("string fields always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(command: &str, error: Option<&str>) -> ToolCall {
        ToolCall {
            tool: "Bash".into(),
            command: Some(command.into()),
            error: error.map(str::to_owned),
            ..ToolCall::default()
        }
    }

    #[test]
    fn prepares_the_expected_json() {
        let turn = Turn {
            prompt: "run the tests".into(),
            tool_calls: vec![bash(
                "cargo test",
                Some("error[E0425]: cannot find value `x`"),
            )],
            reply: "Fixed the missing variable; tests pass.".into(),
        };
        assert_eq!(
            turn.prepare(),
            r#"{"prompt":"run the tests","tool_calls":[{"tool":"Bash","command":"cargo test","error":"error[E0425]: cannot find value `x`"}],"reply":"Fixed the missing variable; tests pass."}"#
        );
    }

    #[test]
    fn edit_call_serializes_files_only() {
        let call = ToolCall {
            tool: "Edit".into(),
            files: vec!["src/a.rs".into()],
            ..ToolCall::default()
        };
        assert_eq!(to_json(&call), r#"{"tool":"Edit","files":["src/a.rs"]}"#);
    }

    #[test]
    fn long_command_and_error_are_cut_to_500_chars() {
        let turn = Turn {
            tool_calls: vec![bash(&"é".repeat(800), Some(&"e".repeat(800)))],
            ..Turn::default()
        };
        let value: serde_json::Value = serde_json::from_str(&turn.prepare()).unwrap();
        let call = &value["tool_calls"][0];
        assert_eq!(call["command"].as_str().unwrap().chars().count(), 500);
        assert_eq!(call["error"].as_str().unwrap().chars().count(), 500);
    }

    #[test]
    fn oversized_turn_drops_oldest_tool_calls() {
        let tool_calls: Vec<_> = (0..120)
            .map(|i| {
                bash(
                    &format!("{i:03}{}", "c".repeat(497)),
                    Some(&"e".repeat(500)),
                )
            })
            .collect();
        let turn = Turn {
            prompt: "the prompt".into(),
            tool_calls,
            reply: "the reply".into(),
        };
        let json = turn.prepare();
        assert!(estimate_tokens(&json) <= MAX_TOKENS);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["prompt"], "the prompt");
        assert_eq!(value["reply"], "the reply");
        let calls = value["tool_calls"].as_array().unwrap();
        assert!(calls.len() < 120 && calls.len() > 80, "{}", calls.len());
        assert!(
            calls.last().unwrap()["command"]
                .as_str()
                .unwrap()
                .starts_with("119")
        );
        assert!(!calls[0]["command"].as_str().unwrap().starts_with("000"));
    }

    #[test]
    fn turn_without_tool_calls_keeps_an_empty_list() {
        let turn = Turn {
            prompt: "p".into(),
            reply: "r".into(),
            ..Turn::default()
        };
        assert_eq!(
            turn.prepare(),
            r#"{"prompt":"p","tool_calls":[],"reply":"r"}"#
        );
    }

    #[test]
    fn oversized_prompt_is_trimmed_from_its_start() {
        let turn = Turn {
            prompt: format!("old{}new", "x".repeat(120_000)),
            reply: "whole reply".into(),
            ..Turn::default()
        };
        let json = turn.prepare();
        assert!(estimate_tokens(&json) <= MAX_TOKENS);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["prompt"].as_str().unwrap().ends_with("new"));
        assert!(!value["prompt"].as_str().unwrap().starts_with("old"));
        assert_eq!(value["reply"], "whole reply");
    }

    #[test]
    fn oversized_reply_is_trimmed_once_the_prompt_is_gone() {
        let turn = Turn {
            prompt: "p".repeat(100_000),
            reply: format!("old{}new", "r".repeat(100_000)),
            ..Turn::default()
        };
        let json = turn.prepare();
        assert!(estimate_tokens(&json) <= MAX_TOKENS);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["prompt"], "");
        assert!(value["reply"].as_str().unwrap().ends_with("new"));
    }

    #[test]
    fn map_text_reaches_every_field() {
        let mut turn = Turn {
            prompt: "a".into(),
            tool_calls: vec![ToolCall {
                tool: "Edit".into(),
                command: Some("a".into()),
                files: vec!["a".into()],
                error: Some("a".into()),
            }],
            reply: "a".into(),
        };
        turn.map_text(|text| text.replace('a', "b"));
        assert_eq!(
            to_json(&turn),
            r#"{"prompt":"b","tool_calls":[{"tool":"Edit","command":"b","files":["b"],"error":"b"}],"reply":"b"}"#
        );
    }

    #[test]
    fn tool_error_attaches_to_its_call_and_ignores_unknown_ids() {
        let mut turn = Turn::default();
        let mut ids = Vec::new();
        turn.apply(
            Entry::ToolCall {
                id: "1".into(),
                call: bash("x", None),
            },
            &mut ids,
        );
        turn.apply(
            Entry::ToolError {
                id: "unknown".into(),
                error: "lost".into(),
            },
            &mut ids,
        );
        assert_eq!(turn.tool_calls[0].error, None);
        turn.apply(
            Entry::ToolError {
                id: "1".into(),
                error: "boom".into(),
            },
            &mut ids,
        );
        assert_eq!(turn.tool_calls[0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn is_empty_notices_each_field() {
        assert!(Turn::default().is_empty());
        assert!(
            !Turn {
                prompt: "p".into(),
                ..Turn::default()
            }
            .is_empty()
        );
        assert!(
            !Turn {
                reply: "r".into(),
                ..Turn::default()
            }
            .is_empty()
        );
        assert!(
            !Turn {
                tool_calls: vec![bash("x", None)],
                ..Turn::default()
            }
            .is_empty()
        );
    }
}
