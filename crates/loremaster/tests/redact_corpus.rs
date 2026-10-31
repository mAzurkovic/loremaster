//! Every secret in the fixed corpus must be gone after redaction.

use loremaster::redact::Redactor;
use serde::Deserialize;

const CORPUS: &str = include_str!("secrets/corpus.jsonl");
const CUT_MARKER: &str = "<cut>";
const REQUIRED_CATEGORIES: [&str; 11] = [
    "aws",
    "gcp",
    "azure",
    "github",
    "slack",
    "stripe",
    "openai",
    "anthropic",
    "private-key",
    "jwt",
    "connection-string",
];

#[derive(Deserialize)]
struct Sample {
    category: String,
    text: String,
    secret: String,
}

fn check_corpus(corpus: &str) -> Result<String, String> {
    let mut redactor = Redactor::new("/nonexistent/config.toml");
    let mut total = 0;
    for (index, line) in corpus.lines().enumerate() {
        let number = index + 1;
        // The file keeps each sample split, so secret scanners do not flag these fake secrets.
        let line = line.replace(CUT_MARKER, "");
        let sample: Sample =
            serde_json::from_str(&line).map_err(|_| format!("bad fixture line {number}"))?;
        if !sample.text.contains(&sample.secret) {
            return Err(format!("bad fixture line {number}"));
        }
        if redactor.redact(&sample.text).contains(&sample.secret) {
            return Err(format!(
                "secret survived: {} line {number}",
                sample.category
            ));
        }
        total += 1;
    }
    Ok(format!("{total}/{total} redacted"))
}

#[test]
fn corpus_is_fully_redacted() {
    let summary = check_corpus(CORPUS).unwrap();
    println!("{summary}");
    assert_eq!(summary, "50/50 redacted");
}

#[test]
fn corpus_covers_every_category_at_least_three_times() {
    for category in REQUIRED_CATEGORIES {
        let needle = format!("\"category\": \"{category}\"");
        assert!(CORPUS.matches(&needle).count() >= 3, "{category}");
    }
}

#[test]
fn a_line_whose_secret_is_not_in_its_text_is_a_bad_fixture() {
    let corpus = r#"{"category":"jwt","text":"nothing here","secret":"abc"}"#;
    assert_eq!(check_corpus(corpus).unwrap_err(), "bad fixture line 1");
}

#[test]
fn a_surviving_secret_fails_with_its_category_and_line() {
    let corpus = r#"{"category":"custom","text":"plain words","secret":"plain"}"#;
    assert_eq!(
        check_corpus(corpus).unwrap_err(),
        "secret survived: custom line 1"
    );
}

#[test]
fn custom_pattern_written_to_config_applies_on_the_next_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut redactor = Redactor::new(&path);
    assert_eq!(redactor.redact("ticket ACME-123456"), "ticket ACME-123456");
    std::fs::write(&path, "[redact]\npatterns = [\"ACME-[0-9]{6}\"]\n").unwrap();
    assert_eq!(
        redactor.redact("ticket ACME-123456"),
        "ticket [REDACTED:custom-1]"
    );
}
