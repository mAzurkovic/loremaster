# Codex CLI adapter (parked for P1)

Moved out of the P0 build. See `../design.md` "P1: Codex CLI" and `../requirements.md` "P1: More Agents".

- `codex.rs`: `CodexAdapter`, the `Adapter` implementation. Goes back in `crates/loremaster-agents/src/`.
- `fixtures/`: test fixtures. Go back in `crates/loremaster-agents/tests/fixtures/codex/`.
- `adapter_tests.rs`: the adapter test file from when both agents were built, including the Codex-only cases.

This has never run against a real Codex session. Codex was not installed when it was written, so the transcript format comes from Codex's published source (`codex-rs`). Before relying on it:
- Replace the fixture transcript with one from a real session.
- Confirm Codex accepts empty stdout from a Stop hook. Its docs say Stop expects JSON.
