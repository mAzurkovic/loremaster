use std::fs;
use std::path::{Path, PathBuf};

use loremaster_agents::{Adapter, AdapterError, ClaudeCodeAdapter, CodexAdapter, CoreEvent, Turn};

const SECRET: &str = "ghp_Z9xK2mQ7vL4tR8wN1pB6sD3fH5jG0cY2uE7a";

fn fixture(agent: &str, file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(agent)
        .join(file)
}

fn event(agent: &str, file: &str) -> String {
    fs::read_to_string(fixture(agent, file)).unwrap()
}

fn agents() -> [(&'static str, &'static dyn Adapter); 2] {
    [
        ("claude-code", &ClaudeCodeAdapter),
        ("codex", &CodexAdapter),
    ]
}

fn read_all(adapter: &dyn Adapter, path: &Path) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut offset = 0;
    loop {
        let (turn, next) = adapter.read_turn(path, offset).unwrap();
        if turn.is_empty() {
            return turns;
        }
        turns.push(turn);
        offset = next;
    }
}

#[test]
fn hook_events_parse_for_both_agents() {
    for (agent, adapter) in agents() {
        let CoreEvent::Capture {
            session_id,
            cwd,
            transcript_path,
            last_assistant_message,
        } = adapter.parse_event(&event(agent, "stop.json")).unwrap()
        else {
            panic!("stop is a capture");
        };
        assert!(!session_id.is_empty());
        assert_eq!(cwd, Path::new("/tmp/fixture-project"));
        assert!(transcript_path.is_some());
        assert_eq!(
            last_assistant_message.as_deref(),
            Some("Set a to 2 in config.toml.")
        );

        assert!(matches!(
            adapter.parse_event(&event(agent, "user_prompt_submit.json")).unwrap(),
            CoreEvent::Recall { prompt, .. } if prompt == "how do we deploy?"
        ));
        assert!(matches!(
            adapter
                .parse_event(&event(agent, "session_start.json"))
                .unwrap(),
            CoreEvent::SessionStart { .. }
        ));
        assert!(matches!(
            adapter
                .parse_event(&event(agent, "session_end.json"))
                .unwrap(),
            CoreEvent::SessionEnd { .. }
        ));
    }
}

#[test]
fn transcript_splits_into_three_turns_in_order() {
    for (agent, adapter) in agents() {
        let path = fixture(agent, "transcript.jsonl");
        let (first, next) = adapter.read_turn(&path, 0).unwrap();
        assert_eq!(first.prompt, "run the tests", "{agent}");
        assert_eq!(first.reply, "Fixed the missing variable; tests pass.");
        assert_eq!(first.tool_calls[0].command.as_deref(), Some("cargo test"));
        assert!(
            first.tool_calls[0]
                .error
                .as_deref()
                .unwrap()
                .contains("cannot find value `x`"),
            "{agent}"
        );
        assert_eq!(first.tool_calls.len(), 2);
        assert_eq!(first.tool_calls[1].error, None, "{agent}");

        let (second, after_second) = adapter.read_turn(&path, next).unwrap();
        assert_eq!(second.prompt, "clone the deploy repo");
        assert!(
            second.tool_calls[0]
                .command
                .as_deref()
                .unwrap()
                .contains(SECRET)
        );
        assert_eq!(second.reply, "Cloned the deploy repo.");

        let (third, end) = adapter.read_turn(&path, after_second).unwrap();
        assert_eq!(third.prompt, "update the config");
        assert_eq!(third.reply, "Set a to 2 in config.toml.");
        assert!(
            third.tool_calls[0].files[0].ends_with("config.toml"),
            "{agent}"
        );
        assert_eq!(end, fs::metadata(&path).unwrap().len());
    }
}

#[test]
fn trailing_partial_line_is_left_for_a_later_read() {
    for (agent, adapter) in agents() {
        let full_len = fs::metadata(fixture(agent, "transcript.jsonl"))
            .unwrap()
            .len();
        let path = fixture(agent, "transcript_truncated.jsonl");
        let mut offset = 0;
        loop {
            let (turn, next) = adapter.read_turn(&path, offset).unwrap();
            offset = next;
            if turn.is_empty() {
                break;
            }
        }
        assert_eq!(offset, full_len, "{agent}");
    }
}

#[test]
fn missing_transcript_is_reported() {
    for (_, adapter) in agents() {
        assert!(matches!(
            adapter.read_turn(Path::new("/nonexistent/transcript.jsonl"), 0),
            Err(AdapterError::TranscriptNotFound(_))
        ));
    }
}

#[test]
fn invalid_line_is_skipped_and_turns_around_it_survive() {
    for (agent, adapter) in agents() {
        let turns = read_all(adapter, &fixture(agent, "transcript_bad_line.jsonl"));
        let prompts: Vec<&str> = turns.iter().map(|t| t.prompt.as_str()).collect();
        assert_eq!(
            prompts,
            [
                "run the tests",
                "clone the deploy repo",
                "update the config"
            ],
            "{agent}"
        );
    }
}

#[test]
fn offset_past_the_end_restarts_at_zero() {
    for (agent, adapter) in agents() {
        let (turn, _) = adapter
            .read_turn(&fixture(agent, "transcript.jsonl"), u64::MAX)
            .unwrap();
        assert_eq!(turn.prompt, "run the tests");
    }
}

#[test]
fn offset_at_the_end_gives_an_empty_turn() {
    for (agent, adapter) in agents() {
        let path = fixture(agent, "transcript.jsonl");
        let end = fs::metadata(&path).unwrap().len();
        let (turn, next) = adapter.read_turn(&path, end).unwrap();
        assert!(turn.is_empty());
        assert_eq!(next, end);
    }
}

#[test]
fn context_renders_in_the_shape_agents_read() {
    for (_, adapter) in agents() {
        assert_eq!(
            adapter.render_context("UserPromptSubmit", "X"),
            r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"X"}}"#
        );
        assert_eq!(
            adapter.render_context("SessionStart", "X"),
            r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"X"}}"#
        );
    }
}

#[test]
fn tool_output_never_reaches_the_prepared_turn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let output = "o".repeat(10_000);
    let lines = [
        serde_json::json!({"type":"user","message":{"content":"run the tests"}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"1","name":"Bash","input":{"command":"cargo test"}}]}}),
        serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"1","content":output}]}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"2","name":"Write","input":{"file_path":"src/a.rs","content":"secret diff"}}]}}),
        serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"2","is_error":true,"content":[{"type":"text","text":"denied"}]}]}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&path, text).unwrap();
    let (turn, _) = ClaudeCodeAdapter.read_turn(&path, 0).unwrap();
    assert_eq!(
        turn.prepare(),
        r#"{"prompt":"run the tests","tool_calls":[{"tool":"Bash","command":"cargo test"},{"tool":"Write","files":["src/a.rs"],"error":"denied"}],"reply":"done"}"#
    );
}

#[test]
fn claude_ignores_side_chains_meta_and_compaction_lines() {
    for flag in ["isSidechain", "isMeta", "isCompactSummary"] {
        let line = serde_json::json!({"type":"user",flag:true,"message":{"content":"hidden"}});
        assert_eq!(ClaudeCodeAdapter.parse_line(&line), []);
    }
    let unknown_block =
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"image"}]}});
    assert_eq!(ClaudeCodeAdapter.parse_line(&unknown_block), []);
    let no_content = serde_json::json!({"type":"user","message":{}});
    assert_eq!(ClaudeCodeAdapter.parse_line(&no_content), []);
}

#[test]
fn codex_reads_argv_commands_and_rejects_malformed_calls() {
    use loremaster_agents::Entry;
    let line = |payload| serde_json::json!({"type":"response_item","payload":payload});

    let argv = line(
        serde_json::json!({"type":"function_call","name":"shell","call_id":"c","arguments":"{\"command\":[\"ls\",\"-a\"]}"}),
    );
    let Entry::ToolCall { call, .. } = &CodexAdapter.parse_line(&argv)[0] else {
        panic!("function call");
    };
    assert_eq!(call.command.as_deref(), Some("ls -a"));

    let no_command = line(
        serde_json::json!({"type":"function_call","name":"web","call_id":"c","arguments":"{}"}),
    );
    let Entry::ToolCall { call, .. } = &CodexAdapter.parse_line(&no_command)[0] else {
        panic!("function call");
    };
    assert_eq!(call.command, None);

    let bad_arguments = line(
        serde_json::json!({"type":"function_call","name":"x","call_id":"c","arguments":"nope"}),
    );
    assert_eq!(CodexAdapter.parse_line(&bad_arguments), []);
    let structured_output =
        line(serde_json::json!({"type":"function_call_output","call_id":"c","output":[]}));
    assert_eq!(CodexAdapter.parse_line(&structured_output), []);
}

#[test]
fn codex_reports_each_kind_of_tool_failure() {
    use loremaster_agents::Entry;
    let cases = [
        (
            serde_json::json!({"type":"event_msg","payload":{"type":"patch_apply_end","call_id":"c","stderr":"no match","success":false}}),
            "no match",
        ),
        (
            serde_json::json!({"type":"event_msg","payload":{"type":"exec_command_end","call_id":"c","exit_code":2,"aggregated_output":"bad"}}),
            "bad",
        ),
        (
            serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"Process exited with code 1\nbroken"}}),
            "Process exited with code 1\nbroken",
        ),
    ];
    for (line, expected) in cases {
        assert_eq!(
            CodexAdapter.parse_line(&line),
            [Entry::ToolError {
                id: "c".into(),
                error: expected.into()
            }]
        );
    }
    let succeeded = serde_json::json!({"type":"event_msg","payload":{"type":"exec_command_end","call_id":"c","exit_code":0}});
    assert_eq!(CodexAdapter.parse_line(&succeeded), []);
}

#[test]
fn captured_turns_use_the_final_message_for_the_last_turn_only() {
    for (agent, adapter) in agents() {
        let path = fixture(agent, "transcript.jsonl");
        let (turns, end) = adapter
            .read_captured(Some(&path), 0, Some("final message"), false)
            .unwrap();
        assert_eq!(turns.len(), 3, "{agent}");
        assert_eq!(turns[0].0.reply, "Fixed the missing variable; tests pass.");
        assert_eq!(turns[2].0.reply, "final message");
        assert_eq!(turns[2].1, end);
        assert_eq!(end, fs::metadata(&path).unwrap().len());
    }
}

#[test]
fn null_final_message_falls_back_to_the_transcript_reply() {
    for (agent, adapter) in agents() {
        let path = fixture(agent, "transcript.jsonl");
        let (turns, _) = adapter.read_captured(Some(&path), 0, None, false).unwrap();
        assert_eq!(turns[2].0.reply, "Set a to 2 in config.toml.", "{agent}");
    }
}

#[test]
fn null_transcript_captures_the_final_message_alone() {
    let (turns, end) = CodexAdapter
        .read_captured(None, 7, Some("only reply"), false)
        .unwrap();
    assert_eq!(end, 7);
    assert_eq!(turns[0].0.reply, "only reply");
    assert_eq!(turns[0].0.prompt, "");
    assert!(turns[0].0.tool_calls.is_empty());
}

#[test]
fn null_transcript_and_null_final_message_is_an_empty_turn() {
    assert!(matches!(
        CodexAdapter.read_captured(None, 0, None, false),
        Err(AdapterError::EmptyTurn)
    ));
}

#[test]
fn nothing_new_captures_nothing() {
    let path = fixture("codex", "transcript.jsonl");
    let end = fs::metadata(&path).unwrap().len();
    let (turns, position) = CodexAdapter
        .read_captured(Some(&path), end, Some("again"), false)
        .unwrap();
    assert!(turns.is_empty());
    assert_eq!(position, end);
}

#[test]
fn late_flushed_output_without_a_prompt_is_dropped_but_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let late = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"late"}]}}"#;
    fs::write(&path, format!("{late}\n")).unwrap();
    let (turns, position) = ClaudeCodeAdapter
        .read_captured(Some(&path), 0, None, false)
        .unwrap();
    assert!(turns.is_empty());
    assert_eq!(position, fs::metadata(&path).unwrap().len());
}

#[test]
fn unreadable_transcript_is_an_io_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    fs::write(&path, "{}\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(matches!(
        ClaudeCodeAdapter.read_turn(&path, 0),
        Err(AdapterError::TranscriptIo(_))
    ));
}

#[test]
fn exit_code_text_inside_successful_output_is_not_a_failure() {
    let line = serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"Exit code: 0\nOutput:\nExit code: 1"}});
    assert_eq!(CodexAdapter.parse_line(&line), []);
    let no_header = serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"plain"}});
    assert_eq!(CodexAdapter.parse_line(&no_header), []);
}

#[test]
fn session_start_reports_its_source() {
    let event =
        r#"{"hook_event_name":"SessionStart","session_id":"s","cwd":"/x","source":"compact"}"#;
    assert!(matches!(
        ClaudeCodeAdapter.parse_event(event).unwrap(),
        CoreEvent::SessionStart { source: Some(source), .. } if source == "compact"
    ));
}

#[test]
fn a_queued_prompt_waits_for_its_own_reply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let prompt = |text: &str| serde_json::json!({"type":"user","message":{"content":text}});
    let reply = |text: &str| serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}});
    let first = format!("{}\n{}\n", prompt("prompt one"), reply("reply one"));
    let queued = format!("{}\n", prompt("prompt two"));
    fs::write(&path, format!("{first}{queued}")).unwrap();

    let (turns, position) = ClaudeCodeAdapter
        .read_captured(Some(&path), 0, Some("reply one"), false)
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].0.prompt, "prompt one");
    assert_eq!(position, first.len() as u64);

    let (later, _) = ClaudeCodeAdapter
        .read_captured(Some(&path), position, Some("reply two"), false)
        .unwrap();
    assert_eq!(later[0].0.prompt, "prompt two");
    assert_eq!(later[0].0.reply, "reply two");
}

#[test]
fn a_lagging_reply_still_attaches_to_its_own_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let prompt = |text: &str| serde_json::json!({"type":"user","message":{"content":text}});
    let reply = |text: &str| serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}});
    fs::write(
        &path,
        format!(
            "{}\n{}\n{}\n",
            prompt("one"),
            reply("reply one"),
            prompt("two")
        ),
    )
    .unwrap();
    let (turns, _) = ClaudeCodeAdapter
        .read_captured(Some(&path), 0, Some("reply two"), false)
        .unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].0.reply, "reply two");
}

#[test]
fn claude_system_lines_are_not_prompts() {
    for text in [
        "<task-notification>\n<task-id>x</task-id></task-notification>",
        "<local-command-caveat>Caveat</local-command-caveat>",
        "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>",
    ] {
        let line = serde_json::json!({"type":"user","message":{"content":text}});
        assert_eq!(ClaudeCodeAdapter.parse_line(&line), [], "{text}");
    }
}

#[test]
fn claude_slash_command_with_arguments_is_a_prompt() {
    let text = "<command-name>/to-design</command-name>\n<command-args>add a cache</command-args>";
    let line = serde_json::json!({"type":"user","message":{"content":text}});
    assert_eq!(
        ClaudeCodeAdapter.parse_line(&line),
        [loremaster_agents::Entry::Prompt(text.to_owned())]
    );
}

#[test]
fn a_turn_with_no_reply_yet_waits_until_its_reply_arrives_or_the_session_ends() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let prompt = serde_json::json!({"type":"user","message":{"content":"pp"}}).to_string();
    let reply =
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"rr"}]}})
            .to_string();
    fs::write(&path, format!("{prompt}\n{}", &reply[..10])).unwrap();

    let (turns, position) = ClaudeCodeAdapter
        .read_captured(Some(&path), 0, None, false)
        .unwrap();
    assert!(turns.is_empty());
    assert_eq!(position, 0);

    let (over, _) = ClaudeCodeAdapter
        .read_captured(Some(&path), 0, None, true)
        .unwrap();
    assert_eq!(over[0].0.prompt, "pp");
    assert_eq!(over[0].0.reply, "");

    fs::write(&path, format!("{prompt}\n{reply}\n")).unwrap();
    let (turns, _) = ClaudeCodeAdapter
        .read_captured(Some(&path), 0, None, false)
        .unwrap();
    assert_eq!(turns[0].0.reply, "rr");
}

#[test]
fn only_the_unfinished_last_turn_waits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let prompt = |text: &str| serde_json::json!({"type":"user","message":{"content":text}});
    let reply =
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"rr"}]}});
    let first = format!("{}\n{reply}\n", prompt("one"));
    fs::write(&path, format!("{first}{}\n", prompt("two"))).unwrap();
    let (turns, position) = ClaudeCodeAdapter
        .read_captured(Some(&path), 0, None, false)
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(position, first.len() as u64);
}
