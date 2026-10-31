# Loremaster — Notes

Cross-device memory for AI agents. Memory lives locally on each of a user's devices and syncs through the cloud, so an agent remembers the user on any machine.

These notes capture landscape research and concepts explored before writing requirements. They are background, not design. Decisions marked **Leaning** are provisional until requirements are written.

---

## 1. Problem

1. Agent memory is scoped to the device the agent runs on. Switch machines and the agent forgets you.
2. Memory is lost when a machine dies or is reformatted.
3. Users have little visibility into, or control over, what the agent remembers.

---

## 2. Landscape (as of Oct 2026)

Most sources are vendor blogs and READMEs; claims are unverified.

### Built into the agents: per-device, kept separate
- **Claude Code** auto-memory lives in `~/.claude/projects/<project>/memory/`. Docs: "not shared across machines or cloud environments." claude.ai chat and Cowork share one memory since Aug 2026; Anthropic said Claude Code memory stays separate. Open feature requests ask for cross-machine sync.
- **GitHub Copilot** agentic memory (preview Jan 2026) is repository-scoped and stored on GitHub. It follows the repo, not the user.
- **Cursor / Windsurf** have project memories; no documented cross-device sync.
- No vendor syncs a *user's* memory across devices and tools. That's the gap.

### Hosted memory APIs: cloud-first
- **Mem0** (hosted MCP, free tier then ~$19/mo; Apache-2.0 SDK), **Zep** (temporal knowledge graph; only the Graphiti engine is OSS), **Supermemory** (aimed at coding agents), **Letta**.
- Sync works because the cloud is the only copy. No offline, the vendor reads your data, network latency on recall.
- Mem0 shut down **OpenMemory** (its local MCP server) and removed it from its repo on 2026-07-29. The local-first slot is open.

### DIY file sync of `~/.claude`
- Syncthing (`claude-code-sync`), git + hooks, rclone to S3/R2, NAS symlinks, `claude-sync` (encrypts files then pushes).
- Copies files without understanding them: OK for Markdown, breaks for live SQLite/vector DBs. Concurrent writes = last-write-wins on the whole file.

### Local-first with optional sync: closest competitors
| Project | Storage | Sync | Notes |
|---|---|---|---|
| memoir-cli | Markdown | E2EE cloud sync (user-held key) | Open format spec, cross-tool. Closest to this idea |
| Basic Memory | Markdown + SQLite index | Paid cloud, rclone bisync | Not E2EE |
| claude-mem (~46k stars) | Local SQLite (+ optional Chroma) | Hosted cmem.ai uploads full prompts | LAN fork exists because users objected |
| ai-memory, Memorix | Local, MCP | Optional cloud/workspace sync | Cross-agent |
| agent-memory (Elastic) | Elasticsearch | Shared index | Team/infra oriented |

### Session/transcript sync
- MemoryPlugin Sync (macOS) syncs conversation history, not distilled memory.

### Takeaways
- Demand is real, the space is fragmented, nobody has clearly won.
- Most tools treat sync as copying files. Few handle real sync semantics: op logs, conflicting facts across devices, propagated deletes, rebuilding embeddings locally.
- Differentiators: **trust** (E2EE, offline) and **portability** (works across agents).

---

## 3. Concepts

### 3.1 Vector databases

**Storing**
- An embedding model turns text into a fixed-length vector (e.g. 384 or 768 numbers). Similar meanings land close together.
- Each row: `id`, `vector`, payload (text + metadata). The vector is only for searching; the payload is what's returned.
- At scale, an approximate nearest neighbor (ANN) index avoids comparing against every row: HNSW (layered graph, most common), IVF (clusters), PQ (compression). Approximate = fast, may miss a true match.

**Querying**
1. Embed the query with the **same model** used for storage.
2. Measure distance, usually cosine similarity.
3. Return top-k rows.
4. Filter on metadata (type, scope, project).
5. Optionally re-rank: hybrid search (vector + BM25 keyword) and custom scoring (e.g. similarity blended with recency).

**Implications for sync**
- Vectors are derived data: regenerate from text. **Sync text + metadata, not vectors.**
- Vectors from different models aren't comparable. Record the model ID; a model change means re-embedding everything.
- ANN indexes can't be merged across machines. Each device builds its own.

**Options:** embedded (`sqlite-vec`, LanceDB, Chroma, DuckDB VSS) or server (pgvector, Qdrant, Weaviate, Pinecone, Milvus). At personal scale (thousands to tens of thousands of memories) embedded is plenty; even brute force is fast.

### 3.2 Embedding models: local vs API

| | Local | API |
|---|---|---|
| Privacy / E2EE | ✅ | ❌ provider sees text |
| Offline | ✅ | ❌ |
| Quality | Good | Best |
| Setup | Model download (100 MB–1 GB) | API key |
| Cost | Free | Cheap, recurring (~$0.02/M tokens for small models, check current pricing) |
| Same results on every device | Only if the model is pinned | Automatic |

- **API examples:** OpenAI `text-embedding-3-*`, Voyage, Cohere Embed, Google Gemini embeddings.
- **Local examples:** `all-MiniLM-L6-v2` (384d), `bge-small-en-v1.5` (384d), `nomic-embed-text` (768d), EmbeddingGemma (768d, on-device), `Qwen3-Embedding-0.6B`.
- **Runtimes:** sentence-transformers, ONNX/`fastembed`, Ollama, llama.cpp, transformers.js.
- Every device must use the same model + version. Tiny numeric differences across hardware don't matter.
- Matryoshka embeddings let vectors be truncated (768 → 256) with small quality loss.

**Leaning:** local model by default (consistent with local-first + E2EE), pinned version, API model as opt-in.

### 3.3 Sync

**Original idea:** push on a cadence (~5 min) or at session end; pull when the user arrives on a device. Assumes one device active at a time.

**Where "one device at a time" breaks:** offline gaps (writes never pushed), failed pushes at session end, genuinely parallel sessions (desktop + laptop, cloud agents), background maintenance jobs writing on each device. "Usually one device has unsynced changes" is the real assumption.

**Option A, sync the DB file:** whole-file last-write-wins silently erases memories; copying a live SQLite file can corrupt it; transfers derived data every time.

**Option B, sync an op log (chosen):**
- Every write is a small immutable op: `{op_id, device, ts, kind: upsert|delete, memory_id, data}`.
- Push = append new ops. Pull = fetch ops since a cursor, apply locally, re-embed.
- New memories use ULIDs, so concurrent creates never conflict.
- Edits to the same record: per-record last-write-wins with a hybrid logical clock (HLC).
- Deletes are tombstones, so offline devices don't resurrect them.
- Op IDs make pushes idempotent; failed pushes just retry.
- Server is an append-only log of encrypted blobs (plus periodic snapshots for bootstrap). It never reads plaintext.
- Full CRDTs are unnecessary unless collaborative editing of one memory's text is allowed.

**When to sync**
| Trigger | Action |
|---|---|
| Session start | Pull (short timeout, ~1–2 s, then continue) |
| Session end / Stop | Push |
| Every ~5 min while running | Push + pull |
| Daemon start / network restored | Push backlog + pull |

Local DB is always the source of truth for reads; sync is best-effort.

**Conflicting meaning** ("prefers tabs" on one device, "prefers spaces" on another) is not a sync conflict. It's a memory-quality problem handled by supersession (see 3.6).

### 3.4 High-level usage

**Components**
1. **Daemon** (each device): captures memories via agent hooks, stores locally (SQLite + vectors), answers recall locally, encrypts and syncs ops.
2. **SyncService** (remote, thin): authenticated push/pull of encrypted ops per user, plus device management. Not a "memory service": it can't read or search memories.
3. **Storage**: per-user append-only op log + snapshots (e.g. S3/R2 + small DB for cursors/devices).
4. **Client** (UI): view, edit, forget memories; manage devices.

**Local vs remote are different shapes:** remote = encrypted op log; local = SQLite with text, metadata, vectors, built from the log. No single master device; the log is the shared truth, each device's DB is a local copy.

**Auth (decided):** per-device credentials, not one shared token. `<cli> login` opens the browser and issues a credential for that device (like `gh auth login`). Dashboard lists devices; a lost laptop can be revoked individually.

**Encryption key (open):** E2EE needs a key separate from login that the server never sees. How a new device gets it:
- Recovery phrase / passphrase entered once per device (1Password/Bitwarden model)
- Approve from an existing device (Signal/WhatsApp model)

A web UI that shows memories must decrypt in the browser. Alternatively the UI is local (served by the daemon).

**Easy-to-miss pieces**
- Agent integration is part of install (hooks).
- **Project identity across devices:** `/Users/x/foo` and `/home/x/foo` are the same project. Key on git remote URL, path as fallback.
- New-device bootstrap: snapshot + newer ops → decrypt → embed locally, with visible progress.
- Forget/delete propagates as tombstones; account deletion wipes the server log.

**User flow**
1. Sign up on the website → create encryption passphrase / save recovery phrase.
2. On each device: install → login (browser, per-device credential) → passphrase once → hooks wired into the agent automatically.
3. New device bootstraps from snapshot + ops.
4. Use the agent normally. Recall is local; sync at session start, session end, every ~5 min.
5. Manage memories and devices in the client.

### 3.5 Agent integration

**Three mechanisms**
| Mechanism | Who decides when memory runs | Deterministic? |
|---|---|---|
| Instructions (`CLAUDE.md`, `AGENTS.md`) | The model | ❌ |
| MCP tools | The model | ❌ |
| Hooks | The harness | ✅ |

MCP gives the agent the *ability* to use memory; only hooks *guarantee* it runs.

**How others do it**
- **Mem0 Claude Code plugin:** hooks for capture (session start, compaction, task completion, session end); for recall, injects a decision rubric at session start and lets the agent decide when to call MCP search. Capture is deterministic, recall is not.
- **claude-mem:** hooks only. `PostToolUse` queues observations for a background worker; `SessionStart` injects a compressed index of recent work. Hooks are fire-and-forget.

**What "deterministic" means:** triggers are deterministic (recall on every prompt, capture on every turn). Content is probabilistic (similarity relevance, LLM judgment of what's worth keeping).

**Agent-agnostic shape: core + thin adapters.** Each adapter maps its harness's events to four core verbs:
| Verb | Fires on | Does |
|---|---|---|
| `session_start(project)` | SessionStart | Pull sync, inject short profile |
| `recall(prompt)` | UserPromptSubmit | Local search, inject top-k (~500-token cap, <100 ms) |
| `capture(turn)` | Stop / PostToolUse | Queue for background extraction, return immediately |
| `session_end()` | SessionEnd | Flush queue, push sync |

**Hook support (Oct 2026):** Claude Code ✅; Codex CLI ✅ (SessionStart, UserPromptSubmit since ~Mar 2026, can inject `additionalContext`); Cursor and Gemini CLI have hooks (unverified); anything else via MCP only.

**Decided:** P0 = Claude Code hooks only. MCP = P1.

### 3.6 Memory types

Semantic / episodic / procedural (from the CoALA paper, 2023) is still the standard vocabulary. Two shifts in practice: **scope** matters as much as type for coding agents, and **how a memory reaches the agent** (always injected vs retrieved) matters more than its label.

- **Semantic:** facts and preferences. Facts change, so use **temporal validity / supersession** (popularized by Zep/Graphiti): mark the old fact superseded instead of deleting it.
- **Episodic:** what happened. Store session summaries, not raw transcripts. Episodes are also source material for semantic facts; old episodes fade with recency.
- **Procedural:** how to do things. Has largely moved into instruction files and skills (`CLAUDE.md`, `AGENTS.md`, Claude Code skills), which agents follow more reliably than retrieved snippets. Best form may be *proposing* rules/skills from repeated workflows.
- **Working memory** is the context window; the harness owns it.

**Scope**
| Scope | Example | Recall |
|---|---|---|
| User / global | "Prefers concise answers" | Every project |
| Project | "This repo uses Alembic" | Only that repo (git remote URL) |
| Team / org (later) | "We deploy via ArgoCD" | Shared; different product |

Project facts leaking into other repos is the most common memory failure; scope filtering prevents it better than better embeddings.

**Delivery**
| | Profile (always injected) | Retrieved |
|---|---|---|
| What | Small set of key user + project facts | Everything else |
| When | SessionStart | UserPromptSubmit, top-k above threshold |
| Size | ~200–300 tokens | ~300–500 tokens |

**Decided:** P0 = semantic (user/project scope, with supersession) + episodic (one summary per session). Procedural = P1, as proposed rules/skills rather than a retrieval table. One table with a `type` column.

**Sketch of a memory record**
```
id, type (semantic|episodic), scope (user|project), project_id (git remote URL),
text, source (session_id + device_id), pinned, created_at/updated_at (HLC),
superseded_by, deleted (tombstone), embedding_model + vector (local only, never synced)
```

### 3.7 Extraction

```
Stop hook → disk queue → 1. Prepare → 2. Extract → 3. Reconcile → 4. Write (ops → log + embed)
SessionEnd or idle timeout → episodic summary
```
Hook only enqueues (milliseconds); the daemon does the rest.

**1. Prepare**
- Keep user messages (preferences are mostly *stated*), final assistant reply, condensed tool calls (commands, files edited, errors).
- Drop large tool outputs, file contents, diffs.
- **Redact secrets first** (regex + entropy, gitleaks-style). Required: anything stored syncs to every device and is permanent in the log.

**2. Extract (LLM, structured output)**
```json
{"text": "Prefers pnpm over npm", "scope": "user", "kind": "preference",
 "evidence": "explicit", "confidence": 0.95}
```
| Remember | Skip |
|---|---|
| Durable preferences | Temporary task state |
| Decisions + reasons | Anything visible in code or git history |
| Corrections ("no, don't mock the DB") | Generic knowledge |
| Non-obvious environment facts | Restating what the agent did |
| Gotchas learned the hard way | Pleasantries |

- Corrections are the strongest signal.
- `evidence`: explicit (user said it) beats observed (agent inferred it).
- Most turns should yield nothing (~70–80% empty). Cap ~3 candidates per turn.

**3. Reconcile** (Mem0 uses the same pattern). Embed candidate, fetch top ~5 similar in the same scope:
| Similarity | Action | LLM? |
|---|---|---|
| ≥ 0.95 | SKIP (optionally bump `last_seen`) | No |
| ≤ 0.75 | ADD | No |
| Between | LLM decides ADD / SUPERSEDE / MERGE / SKIP | Yes |
Thresholds are starting guesses to tune.

**4. Write:** ops appended to local log → embed → index → next sync pushes. Extraction and sync are decoupled.

**Episodic summary** at SessionEnd or after N minutes idle (don't rely on SessionEnd alone; verify how reliably it fires):
```json
{"goal": "...", "outcome": "...", "files": [...], "open_threads": [...], "project_id": "..."}
```
`open_threads` powers "where did we leave off?", on any device.

**Who pays for the LLM (open):**
- User's own Anthropic API key (many Claude Code users only have a subscription)
- Headless `claude -p` via the user's existing Claude Code login (check terms and rate limits)
- Own backend proxy (breaks E2EE: plaintext reaches our server)
- Local small LLM (opt-in, offline, lower quality)

Batching (every N turns / end of burst) reduces cost at the expense of a few minutes' freshness.

**Cross-device:** reconcile runs against a possibly stale local DB, so two devices can both add the same fact. Run a light dedup pass on newly pulled memories after each pull.

---

## 4. Decisions so far
- Sync an **op log**, not DB files. Per-record LWW with HLC, tombstones, ULIDs.
- Sync cadence: pull at session start; push at session end; push + pull every ~5 min; catch up on daemon start / reconnect.
- Sync text + metadata only; each device embeds locally.
- **Per-device credentials** via browser login; devices revocable individually.
- No "memory service": a thin **SyncService** that stores encrypted ops.
- **P0 integration: Claude Code hooks.** MCP in P1. Design as core + adapters for agent-agnosticism.
- **P0 memory types:** semantic (user/project scope, supersession) + episodic (session summaries). Procedural in P1.
- Leaning: local embedding model by default, pinned.

## 5. Open questions
- E2EE key distribution: recovery phrase vs approve-from-existing-device.
- Is a web UI that shows memories needed? (Determines browser-side decryption.)
- Who pays for / runs extraction LLM calls.
- Recall and injection details: thresholds, token budget, formatting.
- Project identity edge cases: no git remote, monorepos, forks.
- Retention: how long episodes and superseded facts live.

## 6. Next
User stories and requirements, before any solution or design.
