//! Removes secrets from text before it is stored or leaves the device.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{LazyLock, OnceLock};
use std::time::SystemTime;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use crate::config::load_patterns;

/// gitleaks `config/gitleaks.toml`, MIT licensed, at commit
/// 5f6ec56cfa6f8c67648d863582c7ae2f206d5c1c, without the sample keys in its `gcp-api-key` allowlist.
/// Allowlists are not read here, and secret scanners flag those keys.
const GITLEAKS_TOML: &str = include_str!("gitleaks.toml");
const GENERIC_RULE: &str = "generic-api-key";
const COMPILED_SIZE_LIMIT: usize = 64 * 1024 * 1024;
/// Many gitleaks rules accept only a few characters after a token. That suits config files, but
/// misses forms common in commands, like `curl -u sk_test_...:` or `?key=AIza...&x=1`.
const NARROW_TOKEN_END: &str = r#"(?:[\x60'"\s;]|\\[nr]|$)"#;
/// `/`, `+` and `=` stay inside a token, or base64 secrets would be cut short.
const TOKEN_END: &str = r"(?:[^a-zA-Z0-9_/+=-]|$)";
/// Secret forms gitleaks has no rule for, as `(id, pattern)`. The secret is the first group, or
/// the whole match when there is no group.
const EXTRA_RULES: [(&str, &str); 12] = [
    // Runs to the last `@` of the token, because a password may contain `@` or `/`.
    (
        "connection-string-password",
        r"\b[a-zA-Z][a-zA-Z0-9+.-]*://[^:/\s@]*:(\S+)@",
    ),
    (
        "password-assignment",
        r#"(?i)(?:password|passwd|pwd|_pass)\\?["']?\s*(?:=>|=|:)\s*(\\"(?:[^"\\]|\\[^"])*\\"|"[^"]*"|'[^']*'|[^;&\s]+)"#,
    ),
    // The gitleaks generic rule misses these keys when they start with `/` or `+`.
    (
        "aws-secret-access-key",
        r#"(?i)aws_?secret_?(?:access_?)?key["']?\s*[=:]\s*["']?([A-Za-z0-9/+=]{40})"#,
    ),
    (
        "password-flag",
        r#"(?i)--(?:password|passwd|pwd)[ =]\s*("[^"]*"|'[^']*'|\S+)"#,
    ),
    (
        "mysql-password-flag",
        r#"\bmysql(?:dump)?\b[^\n]*?\s-p("[^"]*"|'[^']*'|\S+)"#,
    ),
    (
        "docker-password-flag",
        r"\bdocker\s+login\b[^\n]*?\s-p\s+(\S+)",
    ),
    ("sshpass-password-flag", r"\bsshpass\b[^\n]*?\s-p\s*(\S+)"),
    (
        "bearer-auth-header",
        r#"(?i)\bauthorization["'`]?\s*[:=]?\s*["'`]?\s*f?["'`]?bearer\s+([A-Za-z0-9._~+/=-]{8,})"#,
    ),
    (
        "basic-auth-header",
        r"(?i)\bauthorization:\s*basic\s+([A-Za-z0-9+/=]+)",
    ),
    ("google-access-token", r"\bya29\.[0-9A-Za-z_-]{20,}"),
    // Project keys of lengths the gitleaks rule does not list.
    (
        "openai-project-key",
        r"\bsk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{20,}",
    ),
    // Output cut short may hold a key with no closing line, which the gitleaks rule needs.
    (
        "private-key",
        r"(?i)-----BEGIN[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----[\s\S]*?(?:-----END[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----|\z)",
    ),
];

#[derive(Deserialize)]
struct GitleaksConfig {
    rules: Vec<GitleaksRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitleaksRule {
    id: String,
    // Rules that only match file paths have no regex.
    regex: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    entropy: Option<f64>,
    secret_group: Option<usize>,
}

struct Rule {
    id: String,
    pattern: String,
    /// Compiled on first use. Most rules only run when their keyword appears, and hundreds of
    /// compiled patterns would otherwise sit in memory for the life of the daemon.
    regex: OnceLock<Regex>,
    entropy: f64,
    secret_group: Option<usize>,
    /// Has no keywords to prefilter on, so it runs on all text.
    always: bool,
}

fn compile_pattern(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .size_limit(COMPILED_SIZE_LIMIT)
        .build()
}

impl Rule {
    fn new(id: String, pattern: String, entropy: f64, secret_group: Option<usize>) -> Self {
        Self {
            id,
            pattern,
            regex: OnceLock::new(),
            entropy,
            secret_group,
            always: true,
        }
    }

    /// A rule from user input, where a bad pattern must be reported rather than assumed away.
    fn checked(id: String, pattern: &str) -> Result<Self, regex::Error> {
        let regex = compile_pattern(pattern)?;
        let rule = Self::new(id, pattern.to_owned(), 0.0, None);
        rule.regex.get_or_init(|| regex);
        Ok(rule)
    }

    /// Built-in patterns are all compiled by a test, so this cannot fail in a released binary.
    fn regex(&self) -> &Regex {
        self.regex.get_or_init(|| {
            compile_pattern(&self.pattern)
                .unwrap_or_else(|e| panic!("rule {} compiles: {e}", self.id))
        })
    }

    /// Adds each secret in `text`, as `(start, end, rule rank)`, to `spans`.
    fn find(&self, text: &str, rank: usize, spans: &mut Vec<(usize, usize, usize)>) {
        for captures in self.regex().captures_iter(text) {
            let secret = match self.secret_group {
                Some(group) => captures.get(group),
                None => captures.iter().skip(1).flatten().find(|m| !m.is_empty()),
            }
            .or_else(|| captures.get(0));
            if let Some(secret) = secret
                && !secret.is_empty()
                && shannon_entropy(secret.as_str()) >= self.entropy
            {
                spans.push((secret.start(), secret.end(), rank));
            }
        }
    }
}

/// Shannon entropy in bits per byte.
fn shannon_entropy(text: &str) -> f64 {
    let mut counts = [0_usize; 256];
    for byte in text.bytes() {
        counts[usize::from(byte)] += 1;
    }
    let len = text.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let share = count as f64 / len;
            -share * share.log2()
        })
        .sum()
}

struct BuiltinRules {
    rules: Vec<Rule>,
    keywords: AhoCorasick,
    /// Rule indexes for each keyword pattern.
    keyword_rules: Vec<Vec<usize>>,
}

fn builtin_rules() -> BuiltinRules {
    let config: GitleaksConfig =
        toml::from_str(GITLEAKS_TOML).expect("embedded gitleaks config parses");
    let mut sources: Vec<GitleaksRule> = config
        .rules
        .into_iter()
        .filter(|r| r.regex.is_some())
        .collect();
    // Secrets also match the generic rule; a specific rule gives the better name when they tie.
    sources.sort_by_key(|rule| rule.id == GENERIC_RULE);

    let mut rules: Vec<Rule> = EXTRA_RULES
        .iter()
        .map(|(id, pattern)| Rule::new((*id).to_owned(), (*pattern).to_owned(), 0.0, None))
        .collect();
    let mut keyword_rules: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for source in sources {
        let index = rules.len();
        let pattern = source
            .regex
            .as_deref()
            .unwrap_or_default()
            .replace(NARROW_TOKEN_END, TOKEN_END);
        let mut rule = Rule::new(
            source.id.clone(),
            pattern,
            source.entropy.unwrap_or(0.0),
            source.secret_group,
        );
        rule.always = source.keywords.is_empty();
        rules.push(rule);
        for keyword in source.keywords {
            keyword_rules
                .entry(keyword.to_ascii_lowercase())
                .or_default()
                .push(index);
        }
    }
    let keywords = AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .build(keyword_rules.keys())
        .expect("keywords compile");
    BuiltinRules {
        rules,
        keywords,
        keyword_rules: keyword_rules.into_values().collect(),
    }
}

static BUILTIN: LazyLock<BuiltinRules> = LazyLock::new(builtin_rules);

/// Replaces secrets with `[REDACTED:<rule-id>]`, using built-in rules plus the patterns in
/// `config.toml`, which are picked up on the first call after the file changes.
pub struct Redactor {
    config_path: PathBuf,
    config_stamp: Option<(SystemTime, u64)>,
    custom: Vec<Rule>,
}

impl Redactor {
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            config_stamp: None,
            custom: Vec::new(),
        }
    }

    fn reload_config(&mut self) {
        let stamp = std::fs::metadata(&self.config_path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if stamp == self.config_stamp {
            return;
        }
        self.config_stamp = stamp;
        let compiled = load_patterns(&self.config_path)
            .map_err(|e| e.to_string())
            .and_then(|patterns| {
                patterns
                    .iter()
                    .enumerate()
                    .map(|(i, pattern)| {
                        Rule::checked(format!("custom-{}", i + 1), pattern)
                            .map_err(|e| e.to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()
            });
        match compiled {
            Ok(custom) => self.custom = custom,
            Err(e) => tracing::error!(error = %e, "kept previous secret patterns"),
        }
    }

    /// Returns `text` with every secret replaced.
    pub fn redact(&mut self, text: &str) -> String {
        self.reload_config();
        let builtin = &*BUILTIN;
        let mut active: Vec<bool> = builtin.rules.iter().map(|rule| rule.always).collect();
        for hit in builtin.keywords.find_overlapping_iter(text) {
            for &index in &builtin.keyword_rules[hit.pattern().as_usize()] {
                active[index] = true;
            }
        }

        let mut spans = Vec::new();
        for (index, rule) in builtin.rules.iter().enumerate() {
            if active[index] {
                rule.find(text, index, &mut spans);
            }
        }
        let custom_base = builtin.rules.len();
        for (index, rule) in self.custom.iter().enumerate() {
            rule.find(text, custom_base + index, &mut spans);
        }
        spans.sort_unstable();

        let mut output = String::with_capacity(text.len());
        let mut position = 0;
        let mut spans = spans.into_iter().peekable();
        while let Some((start, mut end, rank)) = spans.next() {
            // Merge overlaps so no fragment of a secret survives between two replacements.
            while let Some(&(next_start, next_end, _)) = spans.peek() {
                if next_start >= end {
                    break;
                }
                end = end.max(next_end);
                spans.next();
            }
            let id = builtin
                .rules
                .get(rank)
                .unwrap_or_else(|| &self.custom[rank - custom_base])
                .id
                .as_str();
            output.push_str(&text[position..start]);
            output.push_str("[REDACTED:");
            output.push_str(id);
            output.push(']');
            position = end;
        }
        output.push_str(&text[position..]);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    const GITHUB_TOKEN: &str = "ghp_Z9xK2mQ7vL4tR8wN1pB6sD3fH5jG0cY2uE7a";

    fn redact(text: &str) -> String {
        Redactor::new("/nonexistent/config.toml").redact(text)
    }

    // Fake secrets below are split with `concat!`, so secret scanners do not flag this file.

    #[test]
    fn known_secrets_are_replaced_in_place() {
        for (input, expected) in [
            (
                concat!("export AWS_ACCESS_KEY_ID=AKIA", "Z7Q4VXR2LMN6P3TD").to_owned(),
                "export AWS_ACCESS_KEY_ID=[REDACTED:aws-access-token]".to_owned(),
            ),
            (
                format!("git clone https://{GITHUB_TOKEN}@github.com/a/b"),
                "git clone https://[REDACTED:github-pat]@github.com/a/b".to_owned(),
            ),
            (
                "DATABASE_URL=postgres://app:s3cretPw@db:5432/x".to_owned(),
                "DATABASE_URL=postgres://app:[REDACTED:connection-string-password]@db:5432/x"
                    .to_owned(),
            ),
            (
                "run cargo test in src/".to_owned(),
                "run cargo test in src/".to_owned(),
            ),
        ] {
            assert_eq!(redact(&input), expected);
        }
    }

    #[test]
    fn passwords_in_every_connection_string_form_are_replaced() {
        const MASK: &str = "[REDACTED:connection-string-password]";
        const ASSIGNED: &str = "[REDACTED:password-assignment]";
        for (input, expected) in [
            (
                "postgres://app:p@ss@db:5432/x",
                format!("postgres://app:{MASK}@db:5432/x"),
            ),
            (
                "postgres://app:pa/ss9@db:5432/x",
                format!("postgres://app:{MASK}@db:5432/x"),
            ),
            (
                "postgres://db/x?user=u&password=hunter2",
                format!("postgres://db/x?user=u&password={ASSIGNED}"),
            ),
            (
                "jdbc:sqlserver://h;user=sa;password=S3cret!;",
                format!("jdbc:sqlserver://h;user=sa;password={ASSIGNED};"),
            ),
            (
                "Server=db;User Id=sa;Password=S3cret!;",
                format!("Server=db;User Id=sa;Password={ASSIGNED};"),
            ),
            (
                "PGPASSWORD=hunter2 psql -h db",
                format!("PGPASSWORD={ASSIGNED} psql -h db"),
            ),
        ] {
            assert_eq!(redact(input), expected);
        }
    }

    #[test]
    fn base64_secrets_are_replaced_whole() {
        for input in [
            concat!(
                "AWS_SECRET_ACCESS_KEY=wJalrXUtn",
                "FEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
            ),
            concat!(
                "AccountKey=Eby8vdM02xNOcqFl",
                "qUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
            ),
            "secret_token = abcDEF123+ghiJKL456/mnoPQR789=",
        ] {
            let output = redact(input);
            assert!(output.ends_with(']'), "{output}");
            assert!(
                !output.contains("KBHB")
                    && !output.contains("EXAMPLEKEY")
                    && !output.contains("mnoPQR")
            );
        }
    }

    #[test]
    fn aws_secret_key_starting_with_a_symbol_is_replaced() {
        for key in [
            concat!("/75YX8XM5mSLFy5UB", "IHEYeD7p4Zdl/AK6j0Kgodk"),
            concat!("+75YX8XM5mSLFy5UB", "IHEYeD7p4Zdl/AK6j0Kgodk"),
        ] {
            assert_eq!(
                redact(&format!("AWS_SECRET_ACCESS_KEY={key}")),
                "AWS_SECRET_ACCESS_KEY=[REDACTED:aws-secret-access-key]"
            );
        }
    }

    #[test]
    fn private_key_with_no_end_line_is_replaced() {
        assert_eq!(
            redact("-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA7bq0"),
            "[REDACTED:private-key]"
        );
    }

    #[test]
    fn complete_private_key_keeps_the_text_after_it() {
        assert_eq!(
            redact("-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\nnext line"),
            "[REDACTED:private-key]\nnext line"
        );
    }

    #[test]
    fn quoted_passwords_leave_no_fragment() {
        for (input, expected) in [
            (
                "Password=\"my secret pw\";",
                "Password=[REDACTED:password-assignment];",
            ),
            (
                "PGPASSWORD='two words here'",
                "PGPASSWORD=[REDACTED:password-assignment]",
            ),
            (
                "{\"password\": \"hunter2secret\"}",
                "{\"password\": [REDACTED:password-assignment]}",
            ),
        ] {
            assert_eq!(redact(input), expected);
        }
    }

    #[test]
    fn password_flags_and_google_tokens_are_replaced() {
        for (input, expected) in [
            (
                "mysql -u root -pS3cretPass1",
                "mysql -u root -p[REDACTED:mysql-password-flag]",
            ),
            (
                "pg_dump --password hunter2secret",
                "pg_dump --password [REDACTED:password-flag]",
            ),
            (
                "password: hunter2secret",
                "password: [REDACTED:password-assignment]",
            ),
            (
                "ya29.a0AfB_byC1234567890abcdefg",
                "[REDACTED:google-access-token]",
            ),
        ] {
            assert_eq!(redact(input), expected);
        }
    }

    #[test]
    fn more_password_forms_leave_no_fragment() {
        for (input, expected) in [
            (
                r#"curl -d "{\"password\":\"S3cret!Pass\"}" https://api.x/login"#,
                r#"curl -d "{\"password\":[REDACTED:password-assignment]}" https://api.x/login"#,
            ),
            (
                r#"password => "hunter2 x""#,
                "password => [REDACTED:password-assignment]",
            ),
            (
                r#"pg_dump --password "my secret pw" db"#,
                "pg_dump --password [REDACTED:password-flag] db",
            ),
            (
                r#"mysql -u root -p"two words""#,
                "mysql -u root -p[REDACTED:mysql-password-flag]",
            ),
            (
                "Authorization: Basic dXNlcjpwYXNzd29yZDEyMw==",
                "Authorization: Basic [REDACTED:basic-auth-header]",
            ),
            (
                "docker login -u me -p S3cretPass1 registry.x",
                "docker login -u me -p [REDACTED:docker-password-flag] registry.x",
            ),
            (
                "sshpass -p S3cretPass1 ssh x",
                "sshpass -p [REDACTED:sshpass-password-flag] ssh x",
            ),
            (
                "DB_PASS=hunter2secret ./run",
                "DB_PASS=[REDACTED:password-assignment] ./run",
            ),
            (
                concat!(
                    "OPENAI=sk-proj-",
                    "abcdefghijklmnopqrstuvwxyz0123456789abcd"
                ),
                "OPENAI=[REDACTED:openai-project-key]",
            ),
        ] {
            assert_eq!(redact(input), expected);
        }
    }

    #[test]
    fn bearer_tokens_in_any_header_form_are_replaced() {
        const TOKEN: &str = concat!("qZ7xK2mQ7vL4tR8w", "N1pB6sD3fH5jG0cY2uE7aW9k");
        for template in [
            "Authorization: Bearer @",
            r#"headers = {"Authorization": "Bearer @"}"#,
            "fetch(url, {headers: {Authorization: `Bearer @`}})",
            r#"wget --header="Authorization: Bearer @" https://x.io"#,
            r#"http GET x.io "Authorization:Bearer @""#,
        ] {
            let output = redact(&template.replace('@', TOKEN));
            assert!(!output.contains(TOKEN), "{output}");
            assert!(output.contains("[REDACTED:"), "{output}");
        }
    }

    #[test]
    fn low_entropy_lookalike_is_left_alone() {
        assert_eq!(redact("AKIAAAAAAAAAAAAAAAAA"), "AKIAAAAAAAAAAAAAAAAA");
    }

    #[test]
    fn a_secret_appearing_twice_is_replaced_twice() {
        assert_eq!(
            redact(&format!("{GITHUB_TOKEN} and {GITHUB_TOKEN}")),
            "[REDACTED:github-pat] and [REDACTED:github-pat]"
        );
    }

    fn write_config(path: &Path, patterns: &str) {
        // Different sizes keep the change visible on filesystems with coarse timestamps.
        fs::write(path, format!("[redact]\npatterns = {patterns}\n")).unwrap();
    }

    #[test]
    fn overlapping_rules_leave_one_replacement_and_no_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(
            &path,
            r#"["ghp_Z9xK", "2mQ7vL4tR8wN1pB6sD3fH5jG0cY2uE7a trailing"]"#,
        );
        let output = Redactor::new(&path).redact(&format!("{GITHUB_TOKEN} trailing text"));
        assert_eq!(output, "[REDACTED:custom-1] text");
    }

    #[test]
    fn custom_patterns_apply_on_the_next_call_after_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut redactor = Redactor::new(&path);
        assert_eq!(redactor.redact("ticket ACME-123456"), "ticket ACME-123456");
        write_config(&path, r#"["ACME-[0-9]{6}"]"#);
        assert_eq!(
            redactor.redact("ticket ACME-123456"),
            "ticket [REDACTED:custom-1]"
        );
    }

    #[test]
    fn invalid_pattern_keeps_the_previous_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut redactor = Redactor::new(&path);
        write_config(&path, r#"["ACME-[0-9]{6}"]"#);
        assert_eq!(
            redactor.redact("ticket ACME-123456"),
            "ticket [REDACTED:custom-1]"
        );
        write_config(&path, r#"["(", "unused pattern"]"#);
        assert_eq!(
            redactor.redact("ticket ACME-123456"),
            "ticket [REDACTED:custom-1]"
        );
    }

    #[test]
    fn deleting_the_config_drops_custom_rules_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut redactor = Redactor::new(&path);
        write_config(&path, r#"["ACME-[0-9]{6}"]"#);
        redactor.redact("x");
        fs::remove_file(&path).unwrap();
        assert_eq!(redactor.redact("ticket ACME-123456"), "ticket ACME-123456");
        assert_eq!(redactor.redact(GITHUB_TOKEN), "[REDACTED:github-pat]");
    }

    #[test]
    fn invalid_config_at_start_still_applies_builtin_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[redact\n").unwrap();
        assert_eq!(
            Redactor::new(&path).redact(GITHUB_TOKEN),
            "[REDACTED:github-pat]"
        );
    }

    #[test]
    fn custom_pattern_with_a_group_redacts_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, r#"["pin=([0-9]+)"]"#);
        assert_eq!(
            Redactor::new(&path).redact("pin=4242 ok"),
            "pin=[REDACTED:custom-1] ok"
        );
    }

    #[test]
    fn pattern_matching_nothing_visible_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, r#"["x*"]"#);
        assert_eq!(Redactor::new(&path).redact("abc"), "abc");
    }

    #[test]
    fn tokens_followed_by_punctuation_are_still_found() {
        assert_eq!(
            redact(concat!("curl -u sk_test_", "aB3dE6gH9jK2mN5pQ8sT1vW4:")),
            "curl -u [REDACTED:stripe-access-token]:"
        );
    }

    #[test]
    fn explicit_secret_group_is_honored() {
        let rule = Rule::new("r".into(), "(a)(b)".into(), 0.0, Some(2));
        let mut spans = Vec::new();
        rule.find("ab", 0, &mut spans);
        assert_eq!(spans, [(1, 2, 0)]);
    }

    #[test]
    fn every_embedded_gitleaks_rule_compiles() {
        let config: GitleaksConfig = toml::from_str(GITLEAKS_TOML).unwrap();
        assert_eq!(config.rules.len(), 222);
        let with_regex = config.rules.iter().filter(|r| r.regex.is_some()).count();
        assert_eq!(BUILTIN.rules.len(), with_regex + EXTRA_RULES.len());
        for rule in &BUILTIN.rules {
            rule.regex();
        }
    }

    #[test]
    fn entropy_matches_hand_computed_values() {
        assert_eq!(shannon_entropy("aaaa"), 0.0);
        assert_eq!(shannon_entropy("ab"), 1.0);
        assert_eq!(shannon_entropy("abcd"), 2.0);
    }
}
