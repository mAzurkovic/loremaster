# M1. Local Client Core — Tasks

Source: `design.md` (this folder), `../design.md`, `../requirements.md`, `../milestones.md` M1.
Covers: FR4, FR9, FR14, FR20, FR21, FR22, FR23, FR24, FR26, FR27, FR28, FR29, FR30, FR36, NFR1, NFR3, NFR9.

Each subtask lists expected inputs and outputs, then `Fail:` cases (bad input, missing files, crashes, timeouts) with the expected behaviour. Every case is a required test with these exact values unless marked "example". Hooks never surface an error to the agent (FR29): every hook failure is exit 0 with empty stdout.

## 1. Workspace, core types, store [serial]
- [x] 1.1 Create Cargo workspace: root `Cargo.toml`, `rust-toolchain.toml`, crates `crates/loremaster` (bin, `dev` feature off by default), `crates/loremaster-agents`, `crates/loremaster-core`. `cargo build` and `cargo build --features dev` pass.
  - `cargo build --workspace` → exit 0.
  - `loremaster --help` → lists `hook`, `daemon`, `enable`; no `dev`.
  - `cargo run --features dev -- --help` → also lists `dev`.
  - Fail: release build, `loremaster dev seed x.jsonl` → clap error `unrecognized subcommand 'dev'`, exit 2.
- [x] 1.2 `loremaster-core`: `Record` (all fields from project Appendix A plus serde), ULID generation, `Hlc` with fixed-width `<wall_ms:13>-<counter:5>-<writer>` format and `tick`/`observe`. Unit tests: string order equals HLC order; HLC never goes backwards.
  - `Hlc::new("dev_a1b2")`, wall clock fixed at `1761750000123`, `tick()` twice → `"1761750000123-00000-dev_a1b2"`, `"1761750000123-00001-dev_a1b2"`.
  - `observe("1761750000999-00003-dev_x9")`, local wall still `1761750000123`, then `tick()` → `"1761750000999-00004-dev_a1b2"`.
  - Wall clock moves back to `1761749000000`, `tick()` → greater than previous value (`"1761750000999-00005-dev_a1b2"`).
  - `"1761750000999-00004-dev_a1b2" > "1761750000123-00001-dev_a1b2"` as strings → `true`.
  - `new_ulid()` → 26 chars, Crockford base32; two calls in order → second sorts after first.
  - `Record` serde round trip of project Appendix A example → equal struct; tombstone JSON omits `text`, `kind`, `scope`, `project_id`, `superseded_by`.
  - Fail: `Hlc::parse("garbage")` → `Err(InvalidHlc)`.
  - Fail: `Hlc::parse("123-1-dev_a")` (wrong width) → `Err(InvalidHlc)`.
  - Fail: `Record` JSON with `"type":"note"` → `Err` (unknown variant).
  - Fail: `Record` JSON without `memory_id` → `Err` (missing field).
- [x] 1.3 `crates/loremaster/src/config.rs`: home dir (`~/.loremaster`, override via `LOREMASTER_HOME` for tests and bench), file paths (`sock`, `store.db`, `spool`, `config.toml`, `models/`, `logs/`), `config.toml` parse of `[redact] patterns`, and `estimate_tokens(text) = ceil(chars / 3)`. Unit tests pass.
  - `HOME=/Users/x`, no `LOREMASTER_HOME` → `home() = /Users/x/.loremaster`, `sock() = /Users/x/.loremaster/sock`.
  - `LOREMASTER_HOME=/tmp/lm` → `store_db() = /tmp/lm/store.db`.
  - `config.toml` = `[redact]\npatterns = ["ACME-[0-9]{6}"]` → `patterns == ["ACME-[0-9]{6}"]`.
  - No `config.toml` → `patterns == []`.
  - `estimate_tokens("")` → `0`; `"abcd"` → `2`; 300 chars → `100`; `"héllo"` (5 chars) → `2`.
  - Fail: `HOME` and `LOREMASTER_HOME` both unset → `Err(NoHomeDir)`.
  - Fail: `config.toml` = `[redact\n` (invalid TOML) → `Err(ConfigParse { line: 1 })`.
  - Fail: `config.toml` = `[redact]\npatterns = "x"` (wrong type) → `Err(ConfigParse)`.
- [x] 1.4 `crates/loremaster/src/store.rs`: open `store.db` in WAL, schema v1 for `memories`, `turn_queue`, `sessions`, `outbox`, `sync_state`, `injected` via `PRAGMA user_version`; typed read/write fns for memories, sessions (incl. `transcript_offset`), turn_queue, injected. Tests: fresh DB migrates to v1; reopen is a no-op.
  - Open new path → `user_version = 1`, `journal_mode = wal`, the 6 tables exist.
  - Open same path again → no error, `user_version = 1`.
  - `put_memory(r)` then `get_memory(r.memory_id)` → equal record, vector bytes equal.
  - `set_offset("s1", 4096)` then `get_session("s1").transcript_offset` → `4096`.
  - `mark_injected("s1", "m1")` twice → one row.
  - Fail: `store.db` with `user_version = 2` → `Err(SchemaTooNew { found: 2, supported: 1 })`; file unchanged.
  - Fail: `store.db` containing `not a database` → `Err(NotADatabase)`; file unchanged.
  - Fail: `get_memory("unknown")` → `Ok(None)`.
  - Fail: `get_session("unknown")` → `Ok(None)`.
- [x] 1.5 `crates/loremaster/src/project.rs`: `ProjectId::from_cwd` via `gix` `remote.origin.url`, normalization (lowercase host; strip scheme, `user@`, port, `.git`, `/`), fallback to last path segment, per-cwd cache. Tests: SSH and HTTPS give one ID; port, `.git`, no remote, no repo cases (FR36).
  | Input (`origin` URL or cwd) | Output |
  |---|---|
  | `git@github.com:Acme/Web.git` | `github.com/Acme/Web` |
  | `https://GitHub.com/Acme/Web` | `github.com/Acme/Web` |
  | `https://github.com/Acme/Web.git/` | `github.com/Acme/Web` |
  | `ssh://git@gitlab.example.com:2222/team/app.git` | `gitlab.example.com/team/app` |
  | `https://user@bitbucket.org/t/r` | `bitbucket.org/t/r` |
  | cwd `/home/x/tool`, repo with no remote | `tool` |
  | cwd `/home/x/scratch`, not a repo | `scratch` |
  | cwd `/home/x/web/src/api` inside repo with `origin` `git@github.com:Acme/Web.git` | `github.com/Acme/Web` |
  - Fail: `origin` = `not a url` → treated as no remote → last segment of cwd; warning logged.
  - Fail: corrupt `.git/config` → last segment of cwd; warning logged.
  - Fail: cwd that no longer exists (`/home/x/gone`) → `gone`.

## 2. Agent adapters [parallel, needs 1.2]
- [x] 2.1 `crates/loremaster-agents/src/lib.rs`: `Adapter` trait (`parse_event`, `read_turn(path, offset) -> (Turn, next_offset)`, `render_context`), `CoreEvent`, `Turn`, `ToolCall`. Compiles with doc comments on public items.
  - `adapter_for("claude-code")` → `ClaudeCodeAdapter`; `"cursor"` → `None`.
  - `parse_event` on `{"hook_event_name":"PreToolUse",...}` → `Err(UnsupportedEvent)`.
  - Fail: `parse_event("not json")` → `Err(InvalidJson)`.
  - Fail: hook JSON without `session_id` → `Err(MissingField("session_id"))`.
  - Fail: hook JSON with unknown extra fields → parses; extra fields ignored (agent upgrades add fields).
- [x] 2.2 `Turn::prepare` in `crates/loremaster-agents/src/turn.rs`: truncate `command` and `error` to 500 chars, drop tool output, file contents and diffs, trim oldest tool calls until under 32k est. tokens, serialize to prepared-turn JSON. Unit tests cover truncation and trim order.
  - Turn: prompt `"run the tests"`, Bash call `cargo test` with 10,000-char output and error `` error[E0425]: cannot find value `x` ``, reply `"Fixed the missing variable; tests pass."` →
    `{"prompt":"run the tests","tool_calls":[{"tool":"Bash","command":"cargo test","error":"error[E0425]: cannot find value `x`"}],"reply":"Fixed the missing variable; tests pass."}`
  - Edit call on `src/a.rs` with a 3,000-line diff → `{"tool":"Edit","files":["src/a.rs"]}` (no diff).
  - `command` of 800 chars → 500 chars.
  - 120 tool calls totalling ~40k est. tokens → oldest dropped until ≤ 32k; `prompt`, `reply` and newest calls kept.
  - Fail: turn with no tool calls → `"tool_calls":[]`.
  - Fail: prompt alone of ~40k est. tokens → prompt trimmed from its start, result ≤ 32k; `reply` kept whole.
- [x] 2.3 Fixtures: one short transcript with real Claude Code line shapes from `~/.claude/projects/`, 3 turns with tool calls, content replaced, saved under `crates/loremaster-agents/tests/fixtures/claude-code/`, plus one hook JSON per event.
  - Files → `transcript.jsonl`, `session_start.json`, `user_prompt_submit.json`, `stop.json`, `session_end.json`.
  - Fixtures embed one fake secret (`ghp_` + 36 alphanumerics) in a Bash command, used by 4.5.
  - `grep -ri "$USER" crates/loremaster-agents/tests/fixtures` → no matches.
  - Fail: fixtures include `transcript_truncated.jsonl` (ends mid-line) and `transcript_bad_line.jsonl` (one invalid JSON line mid-file), used by 2.4.
- [x] 2.4 `ClaudeCodeAdapter` in `src/claude_code.rs`: parse the four events, read turns from byte offset up to the last complete line, `render_context` as `hookSpecificOutput.additionalContext`. Fixture tests pass, including a trailing partial line.
  - `stop.json` → `CoreEvent::Capture { session_id, cwd, transcript_path, last_assistant_message: Some(..) }`.
  - `user_prompt_submit.json` → `CoreEvent::Recall { session_id, cwd, prompt }`.
  - `read_turn(transcript.jsonl, 0)` → turn 1 (its prompt and Bash commands), `next_offset` = byte after turn 1's last line; `read_turn(.., next_offset)` → turn 2.
  - `stop.json` with `"last_assistant_message": null` → `Turn.reply` = last assistant message text in `transcript.jsonl`.
  - Hook event with no `transcript_path` → turn with `reply` = `last_assistant_message`, no prompt or tool calls; neither present → `Err(EmptyTurn)`.
  - Transcript ending in partial line `{"type":"assist` → `next_offset` stops before it.
  - `render_context(UserPromptSubmit, "X")` → `{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"X"}}`.
  - Fail: `read_turn` on missing file → `Err(TranscriptNotFound)`.
  - Fail: `transcript_bad_line.jsonl` → bad line skipped and logged; surrounding turns read.
  - Fail: `offset` greater than file size (transcript replaced) → reads from 0.
  - Fail: `offset` at end of file → empty turn, `next_offset` unchanged.
- 2.5 Moved to P1 with Codex CLI. Parked in `../p1-codex/`.

## 3. Redaction, embedding, index, recall [parallel, needs 1]
- [x] 3.1 `crates/loremaster/src/redact.rs`: embed gitleaks `config/gitleaks.toml` at a pinned commit (`include_str!`, commit recorded in a comment), parse rules (regex, keywords, entropy, secret group), `aho-corasick` keyword prefilter, built-in `connection-string-password` rule, user patterns as `custom-<n>` (1-based), replacement `[REDACTED:<rule-id>]`, mtime-checked reload of `config.toml` keeping previous rules on invalid pattern. Unit tests pass.
  | Input | Output |
  |---|---|
  | `export AWS_ACCESS_KEY_ID=AKIAZ7Q4VXR2LMN6P3TD` | `export AWS_ACCESS_KEY_ID=[REDACTED:aws-access-token]` |
  | `git clone https://ghp_<36 alnum>@github.com/a/b` | `git clone https://[REDACTED:github-pat]@github.com/a/b` |
  | `DATABASE_URL=postgres://app:s3cretPw@db:5432/x` | `DATABASE_URL=postgres://app:[REDACTED:connection-string-password]@db:5432/x` |
  | `run cargo test in src/` | unchanged |
  - `config.toml` patterns `["ACME-[0-9]{6}"]`, input `ticket ACME-123456` → `ticket [REDACTED:custom-1]`.
  - Then `config.toml` changed to `["("]` → next call logs an error and still gives `ticket [REDACTED:custom-1]`.
  - Fail: `AKIAAAAAAAAAAAAAAAAA` (below entropy 3) → unchanged.
  - Fail: same secret twice in one input → both replaced.
  - Fail: two rules matching overlapping text → one `[REDACTED:...]`, no leftover fragment of the secret.
  - Fail: `config.toml` deleted after it had patterns → custom rules dropped on next call; gitleaks rules still apply.
  - Fail: invalid `config.toml` at daemon start → starts with gitleaks + built-in rules only; error logged.
  - Fail: every one of the 222 embedded gitleaks rules compiles (unit test), so a bad pin fails the build, not a user.
- [x] 3.2 `crates/loremaster/tests/secrets/corpus.jsonl` (~50 `{category, text, secret}` lines across FR21 categories) and `tests/redact_corpus.rs`: no `secret` survives; custom pattern written to a temp `config.toml` applies on next call (FR21, FR22).
  - Example line: `{"category":"jwt","text":"curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl' api/me","secret":"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl"}`.
  - Categories with ≥ 3 lines each: `aws`, `gcp`, `azure`, `github`, `slack`, `stripe`, `openai`, `anthropic`, `private-key`, `jwt`, `connection-string`.
  - Each sample in the file is split by a `<cut>` marker, which the test removes first, so that secret scanners do not flag it.
  - Test output → `50/50 redacted`; any survivor fails with its category and line number.
  - Fail: corpus line whose `secret` is not inside `text` → test fails with `bad fixture line <n>`.
- [x] 3.3 `crates/loremaster/src/embed.rs`: model download from `Qdrant/bge-small-en-v1.5-onnx-Q` commit `aa8f8b060edb00e03bfdd08813a2949946c8ba55` (5 files), SHA-256 pinned in code (computed from the real files), atomic write; `Embedder` on `fastembed` 7.5.0 (`default-features = false`, `ort-download-binaries-native-tls`), user-defined model, CLS pooling, normalized 384 dims, `Mutex` + `spawn_blocking`. Tests: bad hash rejected; two similar sentences score higher than unrelated ones.
  - Empty `models/` → 5 files in `models/bge-small-en-v1.5-q/`: `model_optimized.onnx` (66,465,124 bytes), `tokenizer.json` (711,396), `config.json` (706), `special_tokens_map.json` (695), `tokenizer_config.json` (1,242).
  - One file with a wrong hash → `Err(HashMismatch { file })`, file not kept.
  - `embed(["Prefers pnpm over npm"])` → 1 vector, length 384, L2 norm 1.0 ± 1e-5.
  - `cos("Prefers pnpm over npm", "Use pnpm instead of npm")` > `cos("Prefers pnpm over npm", "The capital of France is Paris")`.
  - Fail: no network on first start → `Err(Download)` logged; daemon keeps running; download retried on next daemon start.
  - Fail: connection drops mid-download → temp file deleted; nothing in `models/bge-small-en-v1.5-q/` for that file.
  - Fail: `embed([])` → `[]`.
  - Fail: text over the model's 512-token input → truncated by tokenizer; still one 384-dim vector.
- [x] 3.4 `crates/loremaster/src/index.rs`: contiguous `Vec<f32>` plus parallel metadata behind `RwLock`, load from store, insert, mark not live, filtered dot-product search (scope user or matching project, live only). Tests: project-X rows never returned for project Y; superseded and deleted rows never returned (FR9, FR26, FR27, FR30).
  - Rows: A user live; B project `github.com/a/x` live; C project `github.com/a/y` live; D user, `superseded_by` set; E user, `deleted`.
  - `search(q, project = "github.com/a/x")` → candidates `{A, B}` only.
  - `search(q, project = "github.com/a/y")` → `{A, C}`.
  - `mark_not_live(A)` then search → A absent.
  - 50k random unit vectors, one search → returns in < 20 ms (release build).
  - Fail: insert 383-dim vector → `Err(DimMismatch { expected: 384, got: 383 })`.
  - Fail: search on empty index → `[]`.
  - Fail: row with a different `model_id` → excluded from search until re-embedded.
- [x] 3.5 `crates/loremaster/src/recall.rs`: `profile(project_id)` (returns text and ids; kind weight x 0.5^(age/90), 150 user tokens, then project, then remaining user, cap 300, no episodes) and `for_prompt` (episode decay 0.5^(age/30), tau 0.6, cap 500, skip `injected`, record injected), rendered in the design's text format. Tests: caps never exceeded; nothing passing tau gives `None`; episode age shown (FR23, FR24, FR28).
  - Store: user `preference` "Prefers pnpm over npm"; project `github.com/acme/web` `decision` "Deploys go through ArgoCD, not manual kubectl". `profile("github.com/acme/web")` →
    ```
    Loremaster memory from past sessions. Newer user instructions override it.
    User:
    - [preference] Prefers pnpm over npm
    Project github.com/acme/web:
    - [decision] Deploys go through ArgoCD, not manual kubectl
    ```
  - `profile("github.com/acme/api")` → only the header and `User:` section.
  - 200 user facts → output `estimate_tokens` ≤ 300; first fact listed has the highest score.
  - `for_prompt("s1", "github.com/acme/web", "how do we deploy?")` →
    ```
    Loremaster memories relevant to this prompt:
    - [decision] Deploys go through ArgoCD, not manual kubectl
    ```
  - Same call again in `s1` → `None` (already injected); in `s2` → same text again.
  - `for_prompt(.., "what's the weather in Oslo?")` → `None`.
  - Episode 12 days old matching the prompt → line `- [episode, 12 days ago] <text>`.
  - 100 matching facts → output `estimate_tokens` ≤ 500.
  - Fail: no memories at all → `profile` returns `None`.
  - Fail: one fact longer than the remaining budget → skipped; later, shorter facts still added; total ≤ cap.
  - Fail: embedder not loaded → `for_prompt` returns `None`; `profile` still works (needs no embeddings).
  - Fail: prompt `""` or only whitespace → `None`.

## 4. Daemon, hook, spool, enable [serial, needs 2 and 3]
- [x] 4.1 `crates/loremaster/src/server.rs` + `loremaster daemon` subcommand: bind `sock`, one JSON-line request per connection, dispatch four verbs, `{}` on unparseable input; startup loads store, index, model (download if missing), replays `spool` then removes what it finished, logs via `tracing` to `logs/loremasterd.log`.
  - `printf 'garbage\n' | nc -U $LOREMASTER_HOME/sock` → `{}`.
  - Stale `sock` file from a killed daemon → new daemon removes it and binds.
  - `spool` with 2 capture lines for a 2-turn transcript → after start: `spool` empty, `turn_queue` has 2 rows.
  - `logs/loremasterd.log` exists and has a `listening` line.
  - Fail: second `loremaster daemon` while one runs → stderr `loremasterd already running`, exit 1; first daemon unaffected.
  - Fail: `store.db` schema too new → stderr `store.db is from a newer Loremaster (schema 2)`, exit 1.
  - Fail: malformed spool line → skipped and logged; other lines replayed; spool truncated.
  - Fail: client closes connection before reply → no panic; next request served.
- [x] 4.2 Verb handlers in `server.rs`: `session_start` (upsert session, profile), `recall` (embed, `for_prompt`), `capture` and `session_end` (reply first, then `read_turn` from offset, prepare, redact, one transaction inserting `turn_queue` and advancing `transcript_offset`; `session_end` sets `ended`). Tests against fixtures pass.
  - `{"verb":"recall","agent":"claude-code","event":{"session_id":"s1","cwd":"<acme/web repo>","prompt":"how do we deploy?","hook_event_name":"UserPromptSubmit"}}` → `{"additional_context":"Loremaster memories relevant to this prompt:\n- [decision] Deploys go through ArgoCD, not manual kubectl"}`.
  - `{"verb":"session_start",...}` in a project with no memories and no user facts → `{}`.
  - `{"verb":"capture",...stop.json}` on the 3-turn fixture → `{}` in < 5 ms; then `turn_queue`: 3 `pending` rows, one per turn, turn 2 with `[REDACTED:github-pat]`; `sessions.transcript_offset` = end of the last complete line.
  - Same capture sent twice → second adds no row (offset already at end).
  - `{"verb":"session_end",...}` → `sessions.ended = 1`, any remaining turn queued.
  - Fail: `{"verb":"recall","agent":"cursor",...}` → `{}`.
  - Fail: `{"verb":"forget",...}` → `{}`.
  - Fail: capture with missing transcript file → `{}`; no row; offset unchanged; error logged.
  - Fail: SQLite write fails mid-transaction (test failpoint) → rollback; no row; offset unchanged; next capture queues the turn.
  - Fail: embedder not loaded: `recall` → `{}`; `session_start` → profile text.
- [x] 4.3 `crates/loremaster/src/hook.rs` + `loremaster hook <agent> <event>`: read stdin, blocking connect, deadlines (1 s, 90 ms, 40 ms, 1 s), print `render_context` output, on failure spool `{agent, verb, session_id, transcript_path, cwd}` for capture and session_end (`spool.rs`, `O_APPEND`), always exit 0. Tests: daemon absent gives empty output and spool line.
  - Daemon up, stdin `user_prompt_submit.json` (deploy prompt) → stdout `{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"Loremaster memories relevant to this prompt:\n- [decision] ..."}}`, exit 0.
  - Daemon down, `UserPromptSubmit` → stdout empty, exit 0, `spool` unchanged.
  - Daemon down, `Stop` → stdout empty, exit 0, `spool` gains `{"agent":"claude-code","verb":"capture","session_id":"<id>","transcript_path":"<path>","cwd":"<cwd>"}`.
  - Daemon delays recall 200 ms (test hook) → empty stdout, process exits within 150 ms.
  - Stdin `not json` → stdout empty, exit 0.
  - Fail: `loremaster hook cursor Stop` → stdout empty, exit 0.
  - Fail: `loremaster hook claude-code PreToolUse` → stdout empty, exit 0.
  - Fail: empty stdin → stdout empty, exit 0.
  - Fail: daemon replies `not json` → stdout empty, exit 0.
  - Fail: daemon down and `spool` not writable → stdout empty, exit 0; error on stderr only.
- [x] 4.4 `crates/loremaster/src/enable.rs` + `loremaster enable claude-code`: merge hook entries into `~/.claude/settings.json` per design table (absolute command path, no matcher, timeouts, `async` on Stop), replace only entries whose command starts with our prefix, stable key order. Tests: other hooks kept; two runs give byte-identical files.
  - Existing `{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}`, `HOME=/Users/x` → `model` kept, `say done` kept, plus 4 entries; Stop entry:
    `{"hooks":[{"async":true,"command":"/Users/x/.loremaster/bin/loremaster hook claude-code Stop","timeout":10,"type":"command"}]}`.
  - Run twice → identical SHA-256 of the file.
  - Existing entry with our prefix and `timeout: 99` → replaced, not duplicated.
  - No settings file → file created with only our 4 entries.
  - Fail: `settings.json` is invalid JSON → stderr `cannot parse /Users/x/.claude/settings.json: <error>`, exit 1, file unchanged.
  - Fail: `settings.json` `hooks` is an array, not an object → stderr names the field, exit 1, file unchanged.
  - Fail: no `~/.claude` dir → stderr `Claude Code not found (~/.claude missing)`, exit 1.
  - Fail: `loremaster enable cursor` → clap error listing `claude-code`, exit 2. `loremaster enable codex` is the same.
  - Fail: write is temp file + rename: process killed mid-write → original file intact.
- [x] 4.5 `crates/loremaster/tests/crash.rs`: start daemon in temp home, run hooks over a fixture session, `kill -9` daemon, run Stop and SessionEnd hooks (exit 0, no output, spooled), restart, assert every turn in `turn_queue` and no raw secret in it (FR4, FR14, FR20, FR29).
  - Fixture session with 3 turns: turn 1 captured live, daemon killed, turns 2 and 3 Stop + SessionEnd hooks run → each exit 0, empty stdout, 2 spool lines.
  - Restart → `turn_queue` has 3 rows in turn order; `spool` empty; `sessions.ended = 1`.
  - `select turn from turn_queue` contains no `ghp_` string.
  - Fail: daemon killed after replying to capture but before commit (test failpoint) → after restart and the next Stop, that turn is in `turn_queue` exactly once.

## 5. Dev tools and CI [serial, needs 4]
- [x] 5.1 `crates/loremaster/src/dev.rs` `dev seed <file.jsonl>`: one `Record` per line, fill missing `memory_id`, `created_at`, `hlc`, embed with real model, write store and index. Test: seeded file recalled by a matching prompt.
  - Line `{"type":"fact","kind":"preference","scope":"user","text":"Prefers pnpm over npm"}` → stdout `seeded 1`; row has 26-char `memory_id`, `created_at` ≈ now, `hlc` set, 384-dim `vector`, `model_id = "bge-small-en-v1.5-q"`.
  - Then `for_prompt(.., "which package manager should I use?")` → includes `- [preference] Prefers pnpm over npm`.
  - Fail: file is validated before anything is written; any bad line → all errors printed, nothing seeded, exit 1.
  - Fail: line 1 `text` over 2,000 chars → `line 1: text over 2000 chars`.
  - Fail: line 3 invalid JSON → `line 3: invalid JSON`.
  - Fail: `"scope":"project"` without `project_id` → `line <n>: project scope needs project_id`.
  - Fail: missing file → `cannot read <path>`, exit 1.
  - Fail: model not available (offline, never downloaded) → `embedding model not available`, exit 1.
- [x] 5.2 `dev bench`: temp `LOREMASTER_HOME`, 50k memories with random unit vectors in one transaction, start daemon, 1,000 child-process runs per verb with fixture hook JSON, print P50/P95, exit non-zero over budget (recall 100 ms, capture 50 ms) (NFR1, NFR3).
  - Output (example timings):
    ```
    seeded 50000 memories in 1.2s
    verb           agent        p50     p95     budget  result
    session_start  claude-code  14ms    22ms    2000ms  ok
    recall         claude-code  19ms    33ms    100ms   ok
    recall_long    claude-code  30ms    40ms    100ms   ok
    capture        claude-code  4ms     7ms     50ms    ok
    session_end    claude-code  5ms     9ms     1000ms  ok
    ```
  - All within budget → exit 0. Any P95 over budget → that row `FAIL`, exit 1.
  - Temp home deleted on exit.
  - Fail: daemon does not start within 30 s → `daemon did not start:` plus last 20 log lines, exit 1, temp home deleted.
- [x] 5.3 `.github/workflows/ci.yml`: on `macos-15` (arm64), `ubuntu-latest` (x86_64), `ubuntu-24.04-arm` (arm64): `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --all-features`, `cargo run --release --features dev -- dev bench`. Workflow green. The search speed and bench steps do not block a merge.
  - Push to a branch → 3 jobs, each with steps fmt, clippy, test, bench, all green.
  - A test made to fail → that job red.
- [x] 5.4 `scripts/samples.jsonl` (~20 seed records: user and project scope, each kind, one episode, one superseded, one tombstone) and `scripts/try-hook.sh`: with `LOREMASTER_HOME` set and daemon running, sends one fixture hook event per verb from `crates/loremaster-agents/tests/fixtures/` and prints each output plus new `turn_queue` rows. Running it per design "Local run" shows profile, recall output and a captured, redacted turn.
  - Output (example):
    ```
    == claude-code SessionStart
    {"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"Loremaster memory from past sessions. ..."}}
    == claude-code UserPromptSubmit
    {"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"Loremaster memories relevant to this prompt:\n- ..."}}
    == claude-code Stop
    (no output)
    turn_queue +1: {"prompt":"...","tool_calls":[{"tool":"Bash","command":"git clone https://[REDACTED:github-pat]@github.com/..."}],"reply":"..."}
    == claude-code SessionEnd
    (no output)
    ```
  - The superseded and tombstoned samples never appear in any output.
  - `LOREMASTER_HOME` unset → `error: set LOREMASTER_HOME to a sandbox dir`, exit 1.
  - Fail: `LOREMASTER_HOME` = `$HOME/.loremaster` → `error: refusing to use the real Loremaster home`, exit 1.
  - Fail: daemon not running → `error: daemon not running at $LOREMASTER_HOME/sock`, exit 1.

## Dependencies
- 1 blocks everything. 2 needs only 1.2.
- 2 and 3 run in parallel.
- 3.2 needs 3.1. 3.5 needs 3.3 and 3.4.
- 4 needs 2 and 3. 4.5 needs 4.1 to 4.3.
- 5 needs 4. 5.1 needs only 1.4 and 3.3 and can start early. 5.4 needs 5.1 and 2.3.

## Exit criteria map (milestones.md M1)
| Criterion | Task |
|---|---|
| Recall <= 100 ms P95 at 50k (NFR1) | 5.2 |
| Capture <= 50 ms P95 (NFR3) | 5.2 |
| Secret corpus 100%, custom pattern (FR21, FR22) | 3.1, 3.2 |
| Profile <= 300, prompt <= 500, nothing below tau (FR23, FR24) | 3.5 |
| No cross-project injection, SSH = HTTPS ID (FR26, FR36) | 1.5, 3.4 |
| Daemon killed mid-session, spool replay, no turn lost (FR4, FR29) | 4.3, 4.5 |
| Superseded and tombstoned never injected (FR9, FR30) | 3.4, 5.4 |
| Claude Code, the only P0 agent (FR38) | 2.4, 5.2, 5.4 |

## Open Questions
- None.
