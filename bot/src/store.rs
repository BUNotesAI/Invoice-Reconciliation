//! Orchestrator state in SQLite. Every state change, its audit row and its outgoing messages commit together.
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Persisted batch states (design §8.1). Transient system steps run inside one call and are not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Standby,
    Collecting,
    NeedsDecision,
    AwaitingConfirm,
    Executing,
    OutputWait,
    ReadyToShare,
    Manual,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standby => "standby",
            Self::Collecting => "collecting",
            Self::NeedsDecision => "needs_decision",
            Self::AwaitingConfirm => "awaiting_confirm",
            Self::Executing => "executing",
            Self::OutputWait => "output_wait",
            Self::ReadyToShare => "ready_to_share",
            Self::Manual => "manual",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "standby" => Self::Standby,
            "collecting" => Self::Collecting,
            "needs_decision" => Self::NeedsDecision,
            "awaiting_confirm" => Self::AwaitingConfirm,
            "executing" => Self::Executing,
            "output_wait" => Self::OutputWait,
            "ready_to_share" => Self::ReadyToShare,
            "manual" => Self::Manual,
            _ => return None,
        })
    }

    /// Chinese label shown to people.
    pub fn label(self) -> &'static str {
        match self {
            Self::Standby => "待命",
            Self::Collecting => "收件",
            Self::NeedsDecision => "待判断",
            Self::AwaitingConfirm => "待确认",
            Self::Executing => "执行",
            Self::OutputWait => "执行等待",
            Self::ReadyToShare => "待分享",
            Self::Manual => "等人工",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Batch {
    pub id: String,
    pub applicant: String,
    pub room_id: String,
    pub period: String,
    pub state: State,
    pub revision: i64,
    pub snapshot_hash: Option<String>,
    pub published_revision: Option<i64>,
    pub verify_attempts: i64,
    pub resume_state: Option<State>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub id: i64,
    pub batch_id: String,
    pub room_id: String,
    pub txn_id: String,
    pub content: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeskSession {
    pub id_hash: String,
    pub csrf: String,
    pub batch_id: String,
    pub code_hash: Option<String>,
    pub code_expires_at: Option<i64>,
    pub paired_user: Option<String>,
    pub paired_until: Option<i64>,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// One audit row as written by a transition.
pub struct Audit<'a> {
    pub event: &'a str,
    pub actor: &'a str,
    pub payload: &'a Value,
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRow {
    pub seq: i64,
    pub revision: i64,
    pub from_state: String,
    pub to_state: String,
    pub event: String,
    pub actor: String,
}

pub struct Store {
    connection: Connection,
}

/// One unit of work: callers change state, add audit and outbox rows, then commit once.
pub struct Work<'a> {
    tx: Transaction<'a>,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self { connection })
    }

    pub fn begin(&mut self) -> rusqlite::Result<Work<'_>> {
        Ok(Work {
            tx: self
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?,
        })
    }

    pub fn batch(&self, id: &str) -> rusqlite::Result<Option<Batch>> {
        self.connection
            .query_row(&format!("{BATCH_COLUMNS} WHERE id = ?1"), [id], row_batch)
            .optional()
    }

    pub fn open_batch_for(&self, applicant: &str) -> rusqlite::Result<Option<Batch>> {
        self.connection
            .query_row(&format!("{BATCH_COLUMNS} WHERE applicant = ?1 AND closed = 0 ORDER BY created DESC LIMIT 1"), [applicant], row_batch)
            .optional()
    }

    pub fn batches(&self) -> rusqlite::Result<Vec<Batch>> {
        let mut statement = self
            .connection
            .prepare(&format!("{BATCH_COLUMNS} WHERE closed = 0"))?;
        statement.query_map([], row_batch)?.collect()
    }

    pub fn files(&self, batch: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT source_id, original_name FROM files WHERE batch_id = ?1 ORDER BY seq",
        )?;
        statement
            .query_map([batch], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    pub fn decisions(&self, batch: &str) -> rusqlite::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare("SELECT body FROM decisions WHERE batch_id = ?1 ORDER BY seq")?;
        statement
            .query_map([batch], |row| row.get::<_, String>(0))?
            .map(|r| r.map(|text| parse(&text)))
            .collect()
    }

    pub fn document(&self, batch: &str, kind: &str) -> rusqlite::Result<Option<Value>> {
        self.connection
            .query_row(
                "SELECT body FROM documents WHERE batch_id = ?1 AND kind = ?2",
                params![batch, kind],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|found| found.map(|text| parse(&text)))
    }

    pub fn pending_outbox(&self) -> rusqlite::Result<Vec<Outgoing>> {
        let mut statement =
            self.connection.prepare("SELECT id, batch_id, room_id, txn_id, content FROM outbox WHERE sent_event IS NULL ORDER BY id")?;
        statement
            .query_map([], |row| {
                Ok(Outgoing {
                    id: row.get(0)?,
                    batch_id: row.get(1)?,
                    room_id: row.get(2)?,
                    txn_id: row.get(3)?,
                    content: parse(&row.get::<_, String>(4)?),
                })
            })?
            .collect()
    }

    pub fn outbox(&self, batch: &str) -> rusqlite::Result<Vec<(String, Value, Option<String>)>> {
        let mut statement = self.connection.prepare(
            "SELECT txn_id, content, sent_event FROM outbox WHERE batch_id = ?1 ORDER BY id",
        )?;
        statement
            .query_map([batch], |row| {
                Ok((row.get(0)?, parse(&row.get::<_, String>(1)?), row.get(2)?))
            })?
            .collect()
    }

    pub fn mark_sent(&self, id: i64, event_id: &str) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE outbox SET sent_event = ?2 WHERE id = ?1 AND sent_event IS NULL",
            params![id, event_id],
        )?;
        Ok(())
    }

    pub fn audit(&self, batch: &str) -> rusqlite::Result<Vec<AuditRow>> {
        let mut statement =
            self.connection.prepare("SELECT seq, revision, from_state, to_state, event, actor FROM audit WHERE batch_id = ?1 ORDER BY seq")?;
        statement
            .query_map([batch], |row| {
                Ok(AuditRow {
                    seq: row.get(0)?,
                    revision: row.get(1)?,
                    from_state: row.get(2)?,
                    to_state: row.get(3)?,
                    event: row.get(4)?,
                    actor: row.get(5)?,
                })
            })?
            .collect()
    }

    pub fn setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
    }

    pub fn inbound_seen(&self, event_id: &str) -> rusqlite::Result<bool> {
        self.connection
            .query_row(
                "SELECT 1 FROM inbound WHERE event_id = ?1",
                [event_id],
                |_| Ok(()),
            )
            .optional()
            .map(|v| v.is_some())
    }

    pub fn desk_session(&self, id_hash: &str) -> rusqlite::Result<Option<DeskSession>> {
        self.connection
            .query_row(
                "SELECT id_hash, csrf, batch_id, code_hash, code_expires_at, paired_user, paired_until FROM desk_sessions WHERE id_hash = ?1",
                [id_hash],
                row_session,
            )
            .optional()
    }

    pub fn session_by_code(
        &self,
        code_hash: &str,
        now: i64,
    ) -> rusqlite::Result<Option<DeskSession>> {
        self.connection
            .query_row(
                "SELECT id_hash, csrf, batch_id, code_hash, code_expires_at, paired_user, paired_until FROM desk_sessions
                 WHERE code_hash = ?1 AND code_expires_at > ?2 AND paired_user IS NULL",
                params![code_hash, now],
                row_session,
            )
            .optional()
    }

    pub fn failures_since(&self, sender: &str, since: i64) -> rusqlite::Result<i64> {
        self.connection.query_row(
            "SELECT COUNT(*) FROM pairing_failures WHERE sender = ?1 AND at > ?2",
            params![sender, since],
            |row| row.get(0),
        )
    }
}

impl Work<'_> {
    pub fn create_batch(&self, batch: &Batch, now: i64) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO batches (id, applicant, room_id, period, state, revision, verify_attempts, closed, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7)",
            params![batch.id, batch.applicant, batch.room_id, batch.period, batch.state.as_str(), batch.revision, now],
        )?;
        Ok(())
    }

    /// Moves a batch and writes its audit row. `expected` guards against a concurrent change of revision.
    pub fn transition(
        &self,
        batch: &Batch,
        to: State,
        revision: i64,
        audit: Audit<'_>,
    ) -> rusqlite::Result<()> {
        let changed = self.tx.execute(
            "UPDATE batches SET state = ?2, revision = ?3 WHERE id = ?1 AND revision = ?4 AND state = ?5",
            params![batch.id, to.as_str(), revision, batch.revision, batch.state.as_str()],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        self.tx.execute(
            "INSERT INTO audit (batch_id, revision, from_state, to_state, event, actor, payload_sha256, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                batch.id,
                revision,
                batch.state.as_str(),
                to.as_str(),
                audit.event,
                audit.actor,
                sha256_hex(audit.payload.to_string().as_bytes()),
                audit.at
            ],
        )?;
        Ok(())
    }

    pub fn set_fields(
        &self,
        batch: &str,
        snapshot: Option<&str>,
        published: Option<i64>,
        attempts: i64,
        resume: Option<State>,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "UPDATE batches SET snapshot_hash = ?2, published_revision = ?3, verify_attempts = ?4, resume_state = ?5 WHERE id = ?1",
            params![batch, snapshot, published, attempts, resume.map(State::as_str)],
        )?;
        Ok(())
    }

    pub fn add_file(
        &self,
        batch: &str,
        source_id: &str,
        original_name: &str,
    ) -> rusqlite::Result<bool> {
        let added = self.tx.execute(
            "INSERT OR IGNORE INTO files (batch_id, source_id, original_name) VALUES (?1, ?2, ?3)",
            params![batch, source_id, original_name],
        )?;
        Ok(added == 1)
    }

    pub fn add_decision(&self, batch: &str, id: &str, body: &Value) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO decisions (batch_id, id, body) VALUES (?1, ?2, ?3)",
            params![batch, id, body.to_string()],
        )?;
        Ok(())
    }

    pub fn put_document(&self, batch: &str, kind: &str, body: &Value) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO documents (batch_id, kind, body) VALUES (?1, ?2, ?3) ON CONFLICT(batch_id, kind) DO UPDATE SET body = excluded.body",
            params![batch, kind, body.to_string()],
        )?;
        Ok(())
    }

    /// Queues one message with a transaction id fixed now, so every resend is the same Matrix transaction.
    pub fn enqueue(
        &self,
        batch: &str,
        room_id: &str,
        txn_id: &str,
        content: &Value,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT OR IGNORE INTO outbox (batch_id, room_id, txn_id, content) VALUES (?1, ?2, ?3, ?4)",
            params![batch, room_id, txn_id, content.to_string()],
        )?;
        Ok(())
    }

    pub fn put_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn mark_inbound(&self, event_id: &str) -> rusqlite::Result<bool> {
        Ok(self.tx.execute(
            "INSERT OR IGNORE INTO inbound (event_id) VALUES (?1)",
            [event_id],
        )? == 1)
    }

    pub fn put_session(&self, session: &DeskSession) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO desk_sessions (id_hash, csrf, batch_id, code_hash, code_expires_at, paired_user, paired_until)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id_hash) DO UPDATE SET code_hash = excluded.code_hash, code_expires_at = excluded.code_expires_at,
               paired_user = excluded.paired_user, paired_until = excluded.paired_until",
            params![session.id_hash, session.csrf, session.batch_id, session.code_hash, session.code_expires_at, session.paired_user, session.paired_until],
        )?;
        Ok(())
    }

    pub fn record_failure(&self, sender: &str, now: i64) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO pairing_failures (sender, at) VALUES (?1, ?2)",
            params![sender, now],
        )?;
        Ok(())
    }

    pub fn commit(self) -> rusqlite::Result<()> {
        self.tx.commit()
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

const BATCH_COLUMNS: &str = "SELECT id, applicant, room_id, period, state, revision, snapshot_hash, published_revision, verify_attempts, resume_state FROM batches";

fn row_batch(row: &rusqlite::Row<'_>) -> rusqlite::Result<Batch> {
    let state: String = row.get(4)?;
    let resume: Option<String> = row.get(9)?;
    Ok(Batch {
        id: row.get(0)?,
        applicant: row.get(1)?,
        room_id: row.get(2)?,
        period: row.get(3)?,
        state: State::parse(&state).ok_or(rusqlite::Error::InvalidQuery)?,
        revision: row.get(5)?,
        snapshot_hash: row.get(6)?,
        published_revision: row.get(7)?,
        verify_attempts: row.get(8)?,
        resume_state: resume.as_deref().and_then(State::parse),
    })
}

fn row_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeskSession> {
    Ok(DeskSession {
        id_hash: row.get(0)?,
        csrf: row.get(1)?,
        batch_id: row.get(2)?,
        code_hash: row.get(3)?,
        code_expires_at: row.get(4)?,
        paired_user: row.get(5)?,
        paired_until: row.get(6)?,
    })
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS batches (
  id TEXT PRIMARY KEY, applicant TEXT NOT NULL, room_id TEXT NOT NULL, period TEXT NOT NULL, state TEXT NOT NULL,
  revision INTEGER NOT NULL, snapshot_hash TEXT, published_revision INTEGER, verify_attempts INTEGER NOT NULL,
  resume_state TEXT, closed INTEGER NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS audit (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, batch_id TEXT NOT NULL REFERENCES batches(id), revision INTEGER NOT NULL,
  from_state TEXT NOT NULL, to_state TEXT NOT NULL, event TEXT NOT NULL, actor TEXT NOT NULL, payload_sha256 TEXT NOT NULL,
  ts INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS files (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, batch_id TEXT NOT NULL REFERENCES batches(id), source_id TEXT NOT NULL,
  original_name TEXT NOT NULL, UNIQUE (batch_id, source_id));
CREATE TABLE IF NOT EXISTS decisions (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, batch_id TEXT NOT NULL REFERENCES batches(id), id TEXT NOT NULL, body TEXT NOT NULL,
  UNIQUE (batch_id, id));
CREATE TABLE IF NOT EXISTS documents (
  batch_id TEXT NOT NULL REFERENCES batches(id), kind TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY (batch_id, kind));
CREATE TABLE IF NOT EXISTS outbox (
  id INTEGER PRIMARY KEY AUTOINCREMENT, batch_id TEXT NOT NULL, room_id TEXT NOT NULL, txn_id TEXT NOT NULL UNIQUE,
  content TEXT NOT NULL, sent_event TEXT);
CREATE TABLE IF NOT EXISTS inbound (event_id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS desk_sessions (
  id_hash TEXT PRIMARY KEY, csrf TEXT NOT NULL, batch_id TEXT NOT NULL, code_hash TEXT, code_expires_at INTEGER,
  paired_user TEXT, paired_until INTEGER);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS pairing_failures (sender TEXT NOT NULL, at INTEGER NOT NULL);
";
