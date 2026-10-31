//! `loremaster enable` against temporary agent configurations.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".claude")).unwrap();
        Self { dir }
    }

    fn home(&self) -> &Path {
        self.dir.path()
    }

    fn settings(&self) -> PathBuf {
        self.home().join(".claude/settings.json")
    }

    fn enable(&self, agent: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_loremaster"))
            .args(["enable", agent])
            .env("HOME", self.home())
            .env_remove("LOREMASTER_HOME")
            .output()
            .unwrap()
    }

    fn json(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.settings()).unwrap()).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

#[test]
fn other_settings_and_hooks_are_kept_and_ours_are_added() {
    let setup = Setup::new();
    fs::write(
        setup.settings(),
        r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#,
    )
    .unwrap();
    assert!(setup.enable("claude-code").status.success());
    let settings = setup.json();
    assert_eq!(settings["model"], "opus");
    let command = format!(
        "{}/.loremaster/bin/loremaster hook claude-code Stop",
        setup.home().display()
    );
    assert_eq!(
        settings["hooks"]["Stop"],
        json!([
            {"hooks": [{"type": "command", "command": "say done"}]},
            {"hooks": [{"async": true, "command": command, "timeout": 10, "type": "command"}]},
        ])
    );
    for (event, timeout) in [
        ("SessionStart", 5),
        ("UserPromptSubmit", 2),
        ("SessionEnd", 3),
    ] {
        let handler = &settings["hooks"][event][0]["hooks"][0];
        assert_eq!(handler["timeout"], timeout, "{event}");
        assert!(handler.get("async").is_none(), "{event}");
        assert!(settings["hooks"][event][0].get("matcher").is_none());
    }
}

#[test]
fn running_twice_gives_identical_bytes() {
    let setup = Setup::new();
    assert!(setup.enable("claude-code").status.success());
    let first = fs::read(setup.settings()).unwrap();
    assert!(setup.enable("claude-code").status.success());
    assert_eq!(fs::read(setup.settings()).unwrap(), first);
}

#[test]
fn our_old_entry_is_replaced_not_duplicated() {
    let setup = Setup::new();
    let command = format!(
        "{}/.loremaster/bin/loremaster hook claude-code Stop",
        setup.home().display()
    );
    let existing = json!({"hooks": {"Stop": [
        {"hooks": [{"command": command, "timeout": 99, "type": "command"}]},
        {"hooks": [
            {"command": command, "timeout": 99, "type": "command"},
            {"command": "keep me", "type": "command"},
        ]},
        {"matcher": "no hooks here"},
    ]}});
    fs::write(setup.settings(), existing.to_string()).unwrap();
    assert!(setup.enable("claude-code").status.success());
    let groups = setup.json()["hooks"]["Stop"].as_array().unwrap().clone();
    assert_eq!(groups.len(), 3, "{groups:?}");
    assert_eq!(
        groups[0]["hooks"],
        json!([{"command": "keep me", "type": "command"}])
    );
    assert_eq!(groups[1], json!({"matcher": "no hooks here"}));
    assert_eq!(groups[2]["hooks"][0]["timeout"], 10);
}

#[test]
fn no_settings_file_creates_only_our_entries() {
    let setup = Setup::new();
    let output = setup.enable("claude-code");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Enabled Loremaster hooks"));
    let settings = setup.json();
    assert_eq!(settings.as_object().unwrap().len(), 1);
    assert_eq!(settings["hooks"].as_object().unwrap().len(), 4);
}

#[test]
fn invalid_json_is_refused_and_left_unchanged() {
    let setup = Setup::new();
    fs::write(setup.settings(), "{nope").unwrap();
    let output = setup.enable("claude-code");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with(&format!("cannot parse {}: ", setup.settings().display())),
        "{}",
        stderr(&output)
    );
    assert_eq!(fs::read_to_string(setup.settings()).unwrap(), "{nope");
}

#[test]
fn unexpected_shapes_are_refused_naming_the_field() {
    for (text, field) in [
        (r#"{"hooks":[]}"#, "`hooks`"),
        (r#"{"hooks":{"Stop":{}}}"#, "`hooks.Stop`"),
    ] {
        let setup = Setup::new();
        fs::write(setup.settings(), text).unwrap();
        let output = setup.enable("claude-code");
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains(field), "{}", stderr(&output));
        assert_eq!(fs::read_to_string(setup.settings()).unwrap(), text);
    }
}

#[test]
fn missing_agent_directory_is_refused() {
    let setup = Setup::new();
    fs::remove_dir(setup.home().join(".claude")).unwrap();
    let claude = setup.enable("claude-code");
    assert_eq!(claude.status.code(), Some(1));
    assert_eq!(stderr(&claude), "Claude Code not found (~/.claude missing)");
}

#[test]
fn unknown_agent_is_a_usage_error_listing_the_choices() {
    for agent in ["cursor", "codex"] {
        let output = Setup::new().enable(agent);
        assert_eq!(output.status.code(), Some(2));
        let message = stderr(&output);
        assert!(message.contains("claude-code"), "{message}");
    }
}

#[test]
fn symlinked_settings_stay_a_symlink_and_no_temp_file_remains() {
    let setup = Setup::new();
    let real = setup.home().join("dotfiles-settings.json");
    fs::write(&real, "{}").unwrap();
    std::os::unix::fs::symlink(&real, setup.settings()).unwrap();
    assert!(setup.enable("claude-code").status.success());
    assert!(
        fs::symlink_metadata(setup.settings())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&real)
            .unwrap()
            .contains("loremaster hook")
    );
    let leftovers = fs::read_dir(setup.home().join(".claude")).unwrap().count();
    assert_eq!(leftovers, 1);
}

#[test]
fn unreadable_settings_path_is_reported() {
    let setup = Setup::new();
    fs::create_dir(setup.settings()).unwrap();
    let output = setup.enable("claude-code");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("cannot read or write"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn unset_home_is_reported() {
    let output = Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .args(["enable", "claude-code"])
        .env_remove("HOME")
        .env_remove("LOREMASTER_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn binary_is_linked_where_hooks_point_and_an_existing_one_is_kept() {
    let setup = Setup::new();
    assert!(setup.enable("claude-code").status.success());
    let binary = setup.home().join(".loremaster/bin/loremaster");
    let target = fs::read_link(&binary).unwrap();
    assert!(target.ends_with("loremaster"));
    fs::remove_file(&binary).unwrap();
    fs::write(&binary, "installed").unwrap();
    assert!(setup.enable("claude-code").status.success());
    assert_eq!(fs::read_to_string(&binary).unwrap(), "installed");
}

#[test]
fn home_path_with_a_space_is_quoted_for_the_shell() {
    let setup = Setup::new();
    let spaced = setup.home().join("Jane Doe's");
    fs::create_dir_all(spaced.join(".claude")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .args(["enable", "claude-code"])
        .env("HOME", &spaced)
        .env_remove("LOREMASTER_HOME")
        .output()
        .unwrap();
    assert!(output.status.success());
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(spaced.join(".claude/settings.json")).unwrap())
            .unwrap();
    let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let run = Command::new("sh")
        .args([
            "-c",
            &command.replace(" hook claude-code Stop", " --version"),
        ])
        .output()
        .unwrap();
    assert!(run.status.success(), "{command}: {run:?}");
}

#[test]
fn settings_file_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let setup = Setup::new();
    fs::write(setup.settings(), "{}").unwrap();
    fs::set_permissions(setup.settings(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(setup.enable("claude-code").status.success());
    let mode = fs::metadata(setup.settings()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o644);
}

#[test]
fn dangling_binary_link_is_replaced() {
    let setup = Setup::new();
    let bin = setup.home().join(".loremaster/bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/nonexistent/old-build", bin.join("loremaster")).unwrap();
    assert!(setup.enable("claude-code").status.success());
    assert!(bin.join("loremaster").exists());
}

#[test]
fn sandbox_home_is_carried_in_the_hook_command() {
    let setup = Setup::new();
    let sandbox = setup.home().join("sandbox");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_loremaster"))
            .args(["enable", "claude-code"])
            .env("HOME", setup.home())
            .env("LOREMASTER_HOME", &sandbox)
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    assert!(run().status.success());
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(setup.settings()).unwrap()).unwrap();
    let groups = settings["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0]["hooks"][0]["command"],
        format!(
            "LOREMASTER_HOME={0} {0}/bin/loremaster hook claude-code Stop",
            sandbox.display()
        )
    );
}
