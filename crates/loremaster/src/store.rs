//! SQLite store for memories, queued turns and session state.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use loremaster_core::{Kind, Record, RecordType, Scope};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, params};
use serde::de::DeserializeOwned;
use thiserror::Error;

const SCHEMA_VERSION: i64 = 1;
const SCHEMA_V1: &str = "
    CREATE TABLE memories (
        memory_id TEXT PRIMARY KEY,
        type TEXT,
        kind TEXT,
        scope TEXT,
        project_id TEXT,
        text TEXT,
        created_at INTEGER,
        hlc TEXT NOT NULL,
        writer TEXT NOT NULL,
        superseded_by TEXT,
        deleted INTEGER NOT NULL DEFAULT 0,
        seq INTEGER,
        vector BLOB,
        model_id TEXT
    );
    CREATE INDEX memories_scope_project ON memories (scope, project_id);
    CREATE TABLE turn_queue (
        id INTEGER PRIMARY KEY,
        session_id TEXT NOT NULL,
        agent TEXT NOT NULL,
        project_id TEXT NOT NULL,
        turn TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending'
    );
    CREATE TABLE sessions (
        session_id TEXT PRIMARY KEY,
        agent TEXT NOT NULL,
        project_id TEXT NOT NULL,
        transcript_path TEXT,
        transcript_offset INTEGER NOT NULL DEFAULT 0,
        last_turn_at INTEGER,
        ended INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE outbox (memory_id TEXT PRIMARY KEY);
    CREATE TABLE sync_state (key TEXT PRIMARY KEY, value TEXT);
    CREATE TABLE injected (
        session_id TEXT NOT NULL,
        memory_id TEXT NOT NULL,
        PRIMARY KEY (session_id, memory_id)
    );
";
const MEMORY_COLUMNS: &str = "memory_id, type, kind, scope, project_id, text, created_at, hlc, \
    writer, superseded_by, deleted, seq, vector, model_id";

/// Why the store could not be opened or used.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store.db is from a newer Loremaster (schema {found})")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("store.db is not a database")]
    NotADatabase,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot lock store.db for setup: {0}")]
    SetupLock(#[from] std::io::Error),
}

/// A memory as kept on the device, with its locally computed vector.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMemory {
    pub record: Record,
    pub vector: Option<Vec<f32>>,
    pub model_id: Option<String>,
}

/// Capture progress for one agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub session_id: String,
    pub agent: String,
    pub project_id: String,
    pub transcript_path: Option<String>,
    pub transcript_offset: u64,
    pub last_turn_at: Option<i64>,
    pub ended: bool,
}

/// One queued prepared turn and the session state that advances with it.
#[derive(Debug, Clone, Copy)]
pub struct QueuedTurn<'a> {
    pub session_id: &'a str,
    pub agent: &'a str,
    pub project_id: &'a str,
    pub turn: &'a str,
    pub next_offset: u64,
    pub now_ms: i64,
}

/// The device store.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the store, migrating it to the current schema. A store it cannot use is
    /// left untouched.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        // Setup must run one process at a time: SQLite reports busy at once, ignoring its timeout,
        // when two connections switch a new database to WAL together.
        let setup_lock = File::options()
            .create(true)
            .append(true)
            .open(path.with_added_extension("lock"))?;
        setup_lock.lock()?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| match e.sqlite_error_code() {
                Some(ErrorCode::NotADatabase) => StoreError::NotADatabase,
                _ => StoreError::Sqlite(e),
            })?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        if version < 1 {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA_V1} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?;
        }
        Ok(Self { conn })
    }

    /// Replaces a memory.
    pub fn put_memory(&self, memory: &StoredMemory) -> Result<(), StoreError> {
        self.put_memories(std::slice::from_ref(memory))
    }

    /// Replaces many memories in one transaction.
    pub fn put_memories(&self, memories: &[StoredMemory]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut insert = tx.prepare_cached(&format!(
                "INSERT OR REPLACE INTO memories ({MEMORY_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ))?;
            for StoredMemory {
                record,
                vector,
                model_id,
            } in memories
            {
                insert.execute(params![
                    record.memory_id,
                    record.record_type.map(RecordType::as_str),
                    record.kind.map(Kind::as_str),
                    record.scope.map(Scope::as_str),
                    record.project_id,
                    record.text,
                    record.created_at,
                    record.hlc,
                    record.writer,
                    record.superseded_by,
                    record.deleted,
                    record.seq,
                    vector.as_deref().map(vector_bytes),
                    model_id,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Reads one memory.
    pub fn get_memory(&self, memory_id: &str) -> Result<Option<StoredMemory>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE memory_id = ?1"),
                [memory_id],
                memory_from_row,
            )
            .optional()?)
    }

    /// Visits every memory.
    pub fn for_each_memory(&self, mut visit: impl FnMut(StoredMemory)) -> Result<(), StoreError> {
        let mut query = self
            .conn
            .prepare_cached(&format!("SELECT {MEMORY_COLUMNS} FROM memories"))?;
        for memory in query.query_map([], memory_from_row)? {
            // One unreadable row must not take the rest of memory down with it.
            match memory {
                Ok(memory) => visit(memory),
                Err(e) => tracing::error!(error = %e, "skipped unreadable memory"),
            }
        }
        Ok(())
    }

    /// Reads current facts eligible for a project, without their vectors.
    pub fn live_facts(&self, project_id: &str) -> Result<Vec<Record>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT memory_id, type, kind, scope, project_id, text, created_at, hlc, writer, \
                 superseded_by, deleted, seq, NULL, NULL
             FROM memories
             WHERE type = 'fact' AND deleted = 0 AND superseded_by IS NULL
                 AND (scope = 'user' OR (scope = 'project' AND project_id = ?1))",
        )?;
        let rows = query.query_map([project_id], memory_from_row)?;
        Ok(rows
            .filter_map(|row| match row {
                Ok(memory) => Some(memory.record),
                Err(e) => {
                    tracing::error!(error = %e, "skipped unreadable memory");
                    None
                }
            })
            .collect())
    }

    /// Creates a session, or points an existing one at its current transcript and reopens it.
    /// Capture progress is kept.
    pub fn upsert_session(
        &self,
        session_id: &str,
        agent: &str,
        project_id: &str,
        transcript_path: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO sessions (session_id, agent, project_id, transcript_path)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id) DO UPDATE SET
                 transcript_path = COALESCE(excluded.transcript_path, transcript_path),
                 ended = 0",
            params![session_id, agent, project_id, transcript_path],
        )?;
        Ok(())
    }

    /// Reads every session that has not ended.
    pub fn open_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT session_id, agent, project_id, transcript_path, transcript_offset,
                 last_turn_at, ended
             FROM sessions WHERE ended = 0",
        )?;
        let sessions = query.query_map([], session_from_row)?;
        Ok(sessions.collect::<Result<_, _>>()?)
    }

    /// Reads one session.
    pub fn get_session(&self, session_id: &str) -> Result<Option<Session>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_id, agent, project_id, transcript_path, transcript_offset,
                     last_turn_at, ended
                 FROM sessions WHERE session_id = ?1",
                [session_id],
                session_from_row,
            )
            .optional()?)
    }

    /// Moves a session's transcript position.
    pub fn set_offset(&self, session_id: &str, offset: u64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET transcript_offset = ?2 WHERE session_id = ?1",
            params![session_id, offset as i64],
        )?;
        Ok(())
    }

    /// Queues a prepared turn and advances the transcript position atomically, so a crash can
    /// neither lose the turn nor queue it twice.
    pub fn commit_turn(&self, turn: QueuedTurn<'_>) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO turn_queue (session_id, agent, project_id, turn, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                turn.session_id,
                turn.agent,
                turn.project_id,
                turn.turn,
                turn.now_ms
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET transcript_offset = ?2, last_turn_at = ?3 WHERE session_id = ?1",
            params![turn.session_id, turn.next_offset as i64, turn.now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Marks a session as ended.
    pub fn end_session(&self, session_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET ended = 1 WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    }

    /// Records that a memory was injected in a session.
    pub fn mark_injected(&self, session_id: &str, memory_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO injected (session_id, memory_id) VALUES (?1, ?2)",
            [session_id, memory_id],
        )?;
        Ok(())
    }

    /// Forgets what a session was given, after its context has lost those memories.
    pub fn clear_injected(&self, session_id: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM injected WHERE session_id = ?1", [session_id])?;
        Ok(())
    }

    /// True when a memory was already injected in a session.
    pub fn was_injected(&self, session_id: &str, memory_id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM injected WHERE session_id = ?1 AND memory_id = ?2)",
            [session_id, memory_id],
            |row| row.get(0),
        )?)
    }

    /// Counter that changes when another process commits to the database.
    pub fn data_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        session_id: row.get(0)?,
        agent: row.get(1)?,
        project_id: row.get(2)?,
        transcript_path: row.get(3)?,
        transcript_offset: row.get::<_, i64>(4)? as u64,
        last_turn_at: row.get(5)?,
        ended: row.get(6)?,
    })
}

fn enum_from_text<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = row.get(index)?;
    text.map(|t| {
        serde_json::from_value(serde_json::Value::String(t)).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })
    })
    .transpose()
}

fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn vector_from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

fn memory_from_row(row: &Row<'_>) -> rusqlite::Result<StoredMemory> {
    let record_type: Option<RecordType> = enum_from_text(row, 1)?;
    let kind: Option<Kind> = enum_from_text(row, 2)?;
    let scope: Option<Scope> = enum_from_text(row, 3)?;
    let vector: Option<Vec<u8>> = row.get(12)?;
    Ok(StoredMemory {
        record: Record {
            user_id: None,
            memory_id: row.get(0)?,
            seq: row.get(11)?,
            hlc: row.get(7)?,
            writer: row.get(8)?,
            record_type,
            kind,
            scope,
            project_id: row.get(4)?,
            text: row.get(5)?,
            created_at: row.get(6)?,
            superseded_by: row.get(9)?,
            deleted: row.get(10)?,
            evict_key: None,
        },
        vector: vector.as_deref().map(vector_from_bytes),
        model_id: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store.db")).unwrap();
        (dir, store)
    }

    fn fact(id: &str) -> StoredMemory {
        StoredMemory {
            record: Record {
                user_id: None,
                memory_id: id.into(),
                seq: Some(4),
                hlc: "1761750000123-00002-w".into(),
                writer: "w".into(),
                record_type: Some(RecordType::Fact),
                kind: Some(Kind::Preference),
                scope: Some(Scope::User),
                project_id: None,
                text: Some("Prefers pnpm over npm".into()),
                created_at: Some(1761750000123),
                superseded_by: None,
                deleted: false,
                evict_key: None,
            },
            vector: Some(vec![0.25, -1.5, 3.0]),
            model_id: Some("m".into()),
        }
    }

    #[test]
    fn fresh_store_has_the_schema() {
        let (_dir, store) = open_temp();
        let conn = &store.conn;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        for table in [
            "memories",
            "turn_queue",
            "sessions",
            "outbox",
            "sync_state",
            "injected",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "{table}");
        }
    }

    #[test]
    fn reopening_keeps_the_schema_and_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        Store::open(&path).unwrap().put_memory(&fact("m1")).unwrap();
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        assert!(store.get_memory("m1").unwrap().is_some());
    }

    #[test]
    fn memory_round_trips_with_its_vector() {
        let (_dir, store) = open_temp();
        let memory = fact("m1");
        store.put_memory(&memory).unwrap();
        assert_eq!(store.get_memory("m1").unwrap().unwrap(), memory);
    }

    #[test]
    fn tombstone_round_trips_with_no_content() {
        let (_dir, store) = open_temp();
        let tombstone = StoredMemory {
            record: Record {
                user_id: None,
                memory_id: "m2".into(),
                seq: None,
                hlc: "1761750000123-00002-w".into(),
                writer: "w".into(),
                record_type: None,
                kind: None,
                scope: None,
                project_id: None,
                text: None,
                created_at: None,
                superseded_by: None,
                deleted: true,
                evict_key: None,
            },
            vector: None,
            model_id: None,
        };
        store.put_memory(&tombstone).unwrap();
        assert_eq!(store.get_memory("m2").unwrap().unwrap(), tombstone);
    }

    #[test]
    fn unknown_ids_read_as_none() {
        let (_dir, store) = open_temp();
        assert!(store.get_memory("unknown").unwrap().is_none());
        assert!(store.get_session("unknown").unwrap().is_none());
    }

    #[test]
    fn offset_persists_on_the_session() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", Some("/t"))
            .unwrap();
        store.set_offset("s1", 4096).unwrap();
        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.transcript_offset, 4096);
        assert_eq!(session.transcript_path.as_deref(), Some("/t"));
        assert_eq!(session.agent, "claude-code");
        assert_eq!(session.project_id, "p");
    }

    #[test]
    fn upserting_keeps_progress_and_reopens_the_session() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", Some("/t"))
            .unwrap();
        store.set_offset("s1", 10).unwrap();
        store.end_session("s1").unwrap();
        assert!(store.get_session("s1").unwrap().unwrap().ended);
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.transcript_offset, 10);
        assert_eq!(session.transcript_path.as_deref(), Some("/t"));
        assert!(!session.ended);
    }

    #[test]
    fn injected_is_recorded_once_per_session() {
        let (_dir, store) = open_temp();
        assert!(!store.was_injected("s1", "m1").unwrap());
        store.mark_injected("s1", "m1").unwrap();
        store.mark_injected("s1", "m1").unwrap();
        assert!(store.was_injected("s1", "m1").unwrap());
        assert!(!store.was_injected("s2", "m1").unwrap());
        let rows: i64 = store
            .conn
            .query_row("SELECT count(*) FROM injected", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        store.clear_injected("s1").unwrap();
        assert!(!store.was_injected("s1", "m1").unwrap());
    }

    #[test]
    fn newer_schema_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE t (a); PRAGMA user_version = 2;")
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::SchemaTooNew {
                found: 2,
                supported: 1
            })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn non_database_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        std::fs::write(&path, "not a database").unwrap();
        assert!(matches!(Store::open(&path), Err(StoreError::NotADatabase)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a database");
    }

    #[test]
    fn setup_lock_that_cannot_be_created_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        std::fs::create_dir(path.with_added_extension("lock")).unwrap();
        assert!(matches!(Store::open(&path), Err(StoreError::SetupLock(_))));
    }

    #[test]
    fn opening_an_unusable_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Store::open(dir.path()),
            Err(StoreError::Sqlite(_))
        ));
    }

    #[test]
    fn commit_turn_queues_and_advances_together() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        let turn = QueuedTurn {
            session_id: "s1",
            agent: "claude-code",
            project_id: "p",
            turn: "{}",
            next_offset: 99,
            now_ms: 5,
        };
        store.commit_turn(turn).unwrap();
        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.transcript_offset, 99);
        assert_eq!(session.last_turn_at, Some(5));
        let status: String = store
            .conn
            .query_row("SELECT status FROM turn_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn failed_commit_rolls_back_the_queue_insert() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_update BEFORE UPDATE ON sessions
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        let turn = QueuedTurn {
            session_id: "s1",
            agent: "claude-code",
            project_id: "p",
            turn: "{}",
            next_offset: 99,
            now_ms: 5,
        };
        assert!(store.commit_turn(turn).is_err());
        let rows: i64 = store
            .conn
            .query_row("SELECT count(*) FROM turn_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        store
            .conn
            .execute_batch("DROP TRIGGER fail_update")
            .unwrap();
        assert_eq!(
            store.get_session("s1").unwrap().unwrap().transcript_offset,
            0
        );
    }

    #[test]
    fn live_facts_skip_other_projects_episodes_and_dead_rows() {
        let (_dir, store) = open_temp();
        let mut project = fact("project");
        project.record.scope = Some(Scope::Project);
        project.record.project_id = Some("x".into());
        let mut other = project.clone();
        other.record.memory_id = "other".into();
        other.record.project_id = Some("y".into());
        let mut episode = fact("episode");
        episode.record.record_type = Some(RecordType::Episode);
        let mut superseded = fact("superseded");
        superseded.record.superseded_by = Some("user".into());
        let mut deleted = fact("deleted");
        deleted.record.deleted = true;
        store
            .put_memories(&[fact("user"), project, other, episode, superseded, deleted])
            .unwrap();
        let mut ids: Vec<String> = store
            .live_facts("x")
            .unwrap()
            .into_iter()
            .map(|r| r.memory_id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["project", "user"]);
    }

    #[test]
    fn for_each_visits_all_memories() {
        let (_dir, store) = open_temp();
        store.put_memories(&[fact("a"), fact("b")]).unwrap();
        let mut seen = 0;
        store.for_each_memory(|_| seen += 1).unwrap();
        assert_eq!(seen, 2);
    }

    #[test]
    fn listing_skips_an_unreadable_row() {
        let (_dir, store) = open_temp();
        store.put_memories(&[fact("a"), fact("b")]).unwrap();
        store
            .conn
            .execute_batch("UPDATE memories SET kind = 'nonsense' WHERE memory_id = 'a'")
            .unwrap();
        let mut seen = Vec::new();
        store
            .for_each_memory(|m| seen.push(m.record.memory_id))
            .unwrap();
        assert_eq!(seen, ["b"]);
    }

    #[test]
    fn live_facts_skip_an_unreadable_row() {
        let (_dir, store) = open_temp();
        store.put_memories(&[fact("a"), fact("b")]).unwrap();
        store
            .conn
            .execute_batch("UPDATE memories SET kind = 'nonsense' WHERE memory_id = 'a'")
            .unwrap();
        let facts = store.live_facts("p").unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].memory_id, "b");
    }

    #[test]
    fn open_sessions_leave_out_ended_ones() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("open", "claude-code", "p", None)
            .unwrap();
        store
            .upsert_session("done", "claude-code", "p", None)
            .unwrap();
        store.end_session("done").unwrap();
        let open: Vec<String> = store
            .open_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(open, ["open"]);
    }

    #[test]
    fn concurrent_opens_migrate_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Store::open(&path).map(|_| ()))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    }

    #[test]
    fn corrupt_enum_text_is_a_read_error() {
        let (_dir, store) = open_temp();
        store.put_memory(&fact("m1")).unwrap();
        store
            .conn
            .execute_batch("UPDATE memories SET kind = 'nonsense'")
            .unwrap();
        assert!(store.get_memory("m1").is_err());
    }

    #[test]
    fn operations_report_sql_errors() {
        let (_dir, store) = open_temp();
        store
            .conn
            .execute_batch("DROP TABLE memories; DROP TABLE sessions; DROP TABLE injected; DROP TABLE turn_queue;")
            .unwrap();
        assert!(store.put_memory(&fact("m1")).is_err());
        assert!(store.get_memory("m1").is_err());
        assert!(store.for_each_memory(|_| ()).is_err());
        assert!(store.live_facts("p").is_err());
        assert!(store.upsert_session("s", "a", "p", None).is_err());
        assert!(store.get_session("s").is_err());
        assert!(store.open_sessions().is_err());
        assert!(store.set_offset("s", 1).is_err());
        assert!(store.end_session("s").is_err());
        assert!(store.mark_injected("s", "m").is_err());
        assert!(store.was_injected("s", "m").is_err());
        assert!(store.clear_injected("s").is_err());
        let turn = QueuedTurn {
            session_id: "s",
            agent: "a",
            project_id: "p",
            turn: "{}",
            next_offset: 1,
            now_ms: 1,
        };
        assert!(store.commit_turn(turn).is_err());
    }

    #[test]
    fn database_locked_by_another_process_is_a_sqlite_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let holder = Connection::open(&path).unwrap();
        holder
            .execute_batch("CREATE TABLE t (a); BEGIN EXCLUSIVE;")
            .unwrap();
        assert!(matches!(Store::open(&path), Err(StoreError::Sqlite(_))));
    }

    #[test]
    fn data_version_changes_after_another_connection_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let store = Store::open(&path).unwrap();
        let before = store.data_version().unwrap();
        Store::open(&path).unwrap().put_memory(&fact("m1")).unwrap();
        assert_ne!(store.data_version().unwrap(), before);
    }
}
