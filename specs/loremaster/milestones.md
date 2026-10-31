# Loremaster Milestones

Source: `design.md`, `requirements.md` (P0). Tracking document: update `Status` as work progresses (`not started`, `in progress`, `done`).

FR38 applies to every milestone: each exit criterion holds in Claude Code, the only P0 agent.

Build order: M1 and M2 in parallel; then M3, M4 and M5 in parallel. M4 and M5 finish their last exit checks after M3.

```
M1 ─┐
    ├─► M3 Sync ───────────────┐
M2 ─┤                          ├─► final exit checks
    ├─► M4 Extraction (build) ─┤
    └─► M5 Setup (build) ──────┘
```

## M1. Local client core
- Delivers: `loremasterd` + hook CLI on one device, no server. Claude Code gets profile and recall from local memory; turns captured and redacted into `turn_queue`.
- Builds: `loremasterd` (`HookServer`, socket), `loremaster` CLI (`hook`, `enable`, `dev seed`), `ClaudeCodeAdapter`, `Redactor`, `Embedder`, `Index`, `Recall` (profile + per prompt), `ProjectId`, SQLite store (all 6 tables), spool, `loremaster-core` crate (record, ULID, HLC).
- Covers: FR4, FR9 (recall filter), FR14 (capture), FR20, FR21, FR22, FR23, FR24, FR26, FR27, FR28, FR29, FR30 (recall filter), FR36, NFR1, NFR3, NFR9.
- Exit criteria:
  - Recall adds <= 100 ms P95 at 50k seeded memories (NFR1).
  - Capture adds <= 50 ms P95 (NFR3).
  - Secret corpus 100% redacted; custom pattern redacted after config change (FR21, FR22).
  - Profile <= 300 tokens, per-prompt <= 500 tokens, nothing injected when nothing passes tau (FR23, FR24).
  - Project-scoped memory never injected in another project; SSH and HTTPS remotes give one project ID (FR26, FR36).
  - Service killed mid-session: agent proceeds, spool replayed on restart, no turn lost (FR4, FR29).
  - Superseded and tombstoned seeded rows never injected (FR9, FR30).
- Needs: none.
- Design: yes.
- Status: in progress. PR #1 open.

## M2. Identity and service foundation
- Delivers: AWS stack with GitHub device-flow login and auth middleware. `loremaster login` issues a per-device token, over SSH too.
- Builds: SAM template (`infra/template.yaml`, dev and prod envs), GitHub OAuth App (device flow), `POST /v1/devices`, `GET /v1/devices`, `DELETE /v1/devices/{id}`, auth middleware, DDB `users`, `devices`, `loremaster login`, API types in `loremaster-core`.
- Covers: FR12, NFR6 (DDB), NFR7.
- Exit criteria:
  - Login from a headless Linux box via URL + code.
  - 11th device refused with the list of current devices; replacing one succeeds and the replaced token is rejected on the next request (FR12).
  - Deleted device token rejected on the next request.
  - Token for user A cannot read or write user B (NFR7).
- Needs: none. `loremaster login` joins M1's CLI once both exist.
- Features:
  - F1. CI/CD: cache the Cargo build and the embedding model in CI, require CI on `main` (done), and run the latency checks on a nightly schedule instead of on each PR. Docs: tasks. Needs: none. Status: not started.
  - The rest of M2 is split into features when it starts.
- Status: not started.

## M3. Sync
- Delivers: memories converge across a user's devices, offline and in parallel. New device bootstraps with progress. Tested with seeded and manually written records; no extraction needed.
- Builds: DDB `memories` table (Appendix A), `POST /v1/sync/push`, `GET /v1/sync/pull`, 50k cap eviction, `SyncClient` (HLC, outbox, cadence, bootstrap), post-pull dedup.
- Covers: FR1, FR5, FR6, FR7, FR8, FR9, FR11, FR32, FR33, NFR2, NFR4.
- Exit criteria:
  - Record written on device A recallable on B within 10 min while A's session continues (FR1).
  - Device offline then reconnected: both directions synced within 10 min, nothing lost (FR5, FR8).
  - Two devices create concurrently: all memories on both (FR6). Same memory changed on both: newest wins on both (FR7).
  - Offline device does not resurrect a deleted memory (FR9).
  - 50k memories bootstrap on a new device within 15 min with visible progress; recall works mid-bootstrap (FR11).
  - Session start <= 2 s P95 (NFR2).
  - Above 50k: oldest evicted in class order, applied on every device (FR33).
- Needs: M1, M2.
- Design: yes.
- Status: not started.

## M4. Extraction
- Delivers: facts and episodes extracted automatically and recalled in later sessions, in either agent, on any device.
- Builds: `POST /v1/extract`, `POST /v1/reconcile`, `POST /v1/episode` (prompts, Haiku 5.5 calls, size limits, daily token cap), `Pipeline`, `SessionTracker`, `loremaster dev eval`.
- Covers: FR2, FR3, FR14, FR15, FR16, FR17, FR18, FR19, FR25, FR30, FR31.
- Exit criteria:
  - Eval set: >= 80% expected memories stored, <= 20% junk (FR15).
  - Eval set: >= 80% injected memories relevant, 0 project leaks (FR25, FR26).
  - Eval set: 100% contradiction cases end with only the newer fact injected (FR31).
  - One episode per session, on SessionEnd or 60 min idle (FR3, FR17).
  - Episode recallable on device B within 10 min of session end, or 60 min after last turn if no session end (FR2, FR3).
  - Offline: turns stay queued, processed on reconnect; 429 leaves turns queued (FR19).
  - Fact told in one session recalled in a later session on the same device and on another device (FR1).
- Needs: M1, M2 to start. M3 for the FR1, FR2 and FR3 cross-device exit checks.
- Design: yes.
- Status: not started.

## M5. Setup and lifecycle
- Delivers: one command takes a device from nothing to capturing; clean uninstall and account deletion.
- Builds: `install.sh`, GitHub Actions release builds (3 targets), `loremaster setup` (login, service unit, agent detection, hooks), `status`, `uninstall`, `update`, `DELETE /v1/account`.
- Covers: FR10, FR13, FR34, FR37, NFR8.
- Exit criteria:
  - Fresh macOS and Linux device reaches capturing within 5 min with one command and browser login (FR10).
  - Agent installed after Loremaster: `loremaster enable <agent>` makes all memory recallable next session (FR37).
  - Uninstall removes hooks, frees the device slot, keeps `store.db` unless `--purge` (FR13).
  - Account deletion: all device tokens rejected immediately, all DDB rows gone, local stores untouched (FR34).
  - Binaries run on macOS arm64, Linux x86_64/arm64 (NFR8).
  - `loremaster update` installs the latest release and restarts the service; previous-version clients still work against `/v1`.
- Needs: M1, M2 to start. M3 for the account-deletion exit check (`memories` rows).
- Status: not started.

## Deferred
- NFR5 (99.5% availability). Met by managed-service SLAs per design D19; no monitoring in P0, so no milestone verifies it.
- US15 to US20, FR35 and P1 Memory Management: out of scope for P0 per requirements. Codex CLI is the first P1 agent.

## Coverage
| Requirement or component | Milestones |
|---|---|
| FR1, FR5, FR6, FR7, FR8, FR11, FR32, FR33 | M3 |
| FR2, FR3, FR15, FR16, FR17, FR18, FR19, FR25, FR31 | M4 |
| FR4 | M1 |
| FR9, FR30 | M1 (recall filter), M3 (tombstone sync), M4 (supersede) |
| FR10, FR13, FR34, FR37 | M5 |
| FR12 | M2 |
| FR14 | M1 (capture), M4 (processing) |
| FR20, FR21, FR22, FR23, FR24, FR26, FR27, FR28, FR29, FR36 | M1 |
| FR38 | M1 to M5 |
| NFR1, NFR3, NFR9 | M1 |
| NFR2, NFR4 | M3 |
| NFR5 | Deferred |
| NFR6 | M2, M3 |
| NFR7 | M2 |
| NFR8 | M1 (runs), M5 (release builds) |
| `ClaudeCodeAdapter` | M1 |
| `loremasterd`, `HookServer`, `Redactor`, `Embedder`, `Index`, `Recall`, `ProjectId` | M1 |
| `loremaster` CLI | M1 (`hook`, `enable`, `dev seed`), M2 (`login`), M4 (`dev eval`), M5 (`setup`, `status`, `uninstall`, `update`) |
| SQLite store | M1 |
| `SyncClient` | M3 |
| `Pipeline`, `SessionTracker` | M4 |
| GitHub OAuth App, auth middleware, `users`, `devices` | M2 |
| `memories` DDB table | M3 |
| Sync endpoints, 50k cap | M3 |
| LLM endpoints | M4 |
| Account deletion | M5 |
| `install.sh`, release builds | M5 |

## Open Questions
- OQ1. Labeled eval set (20 Claude Code sessions) must exist before M4 can exit. Owner: user.
- OQ2. Design OQ4 (tunables) set during M4.
