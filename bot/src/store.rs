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
    Submitted,
    Approved,
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
            Self::Submitted => "submitted",
            Self::Approved => "approved",
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
            "submitted" => Self::Submitted,
            "approved" => Self::Approved,
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
            Self::Submitted => "已交财务",
            Self::Approved => "已通过",
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
    /// The current pairing code, kept so every poll shows the same code until it expires.
    pub code: Option<String>,
    pub code_hash: Option<String>,
    pub code_expires_at: Option<i64>,
    pub paired_user: Option<String>,
    pub paired_until: Option<i64>,
}

/// Wrong attempts a live pairing code survives (design §9.3).
pub const CODE_ATTEMPTS: i64 = 5;

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
        // Databases created before a column existed gain it here.
        for (table, column, definition) in [
            ("desk_sessions", "code", "TEXT"),
            (
                "desk_sessions",
                "code_failures",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("desk_sessions", "created", "INTEGER NOT NULL DEFAULT 0"),
            ("decisions", "superseded_revision", "INTEGER"),
        ] {
            let present: bool = connection
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
                ))?
                .exists([column])?;
            if !present {
                connection.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        // A live code names exactly one session. Older databases may hold duplicates: their codes are withdrawn and
        // the pages draw new ones on the next poll.
        let indexed: bool = connection
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'desk_sessions_live_code'")?
            .exists([])?;
        if !indexed {
            connection.execute_batch(
                "UPDATE desk_sessions SET code = NULL, code_hash = NULL, code_expires_at = NULL;
                 CREATE UNIQUE INDEX desk_sessions_live_code ON desk_sessions(code_hash) WHERE code_hash IS NOT NULL;",
            )?;
        }
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

    /// Decisions still in force; superseded ones stay in the table for the record.
    pub fn decisions(&self, batch: &str) -> rusqlite::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare("SELECT body FROM decisions WHERE batch_id = ?1 AND superseded_revision IS NULL ORDER BY seq")?;
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

    /// The appended history ledger in order, optionally without one batch's own events (finding F6).
    pub fn history_events(&self, excluding: Option<&str>) -> rusqlite::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare("SELECT body FROM history_events WHERE batch_id != ?1 ORDER BY seq")?;
        statement
            .query_map([excluding.unwrap_or("")], |row| row.get::<_, String>(0))?
            .map(|r| r.map(|text| parse(&text)))
            .collect()
    }

    pub fn has_history_event(&self, invoice_no: &str, status: &str) -> rusqlite::Result<bool> {
        self.connection
            .query_row(
                "SELECT 1 FROM history_events WHERE invoice_no = ?1 AND status = ?2",
                params![invoice_no, status],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
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
                "SELECT id_hash, csrf, batch_id, code, code_hash, code_expires_at, paired_user, paired_until FROM desk_sessions WHERE id_hash = ?1",
                [id_hash],
                row_session,
            )
            .optional()
    }

    /// The session holding a live code; the unique index guarantees there is at most one.
    pub fn session_by_code(
        &self,
        code_hash: &str,
        now: i64,
    ) -> rusqlite::Result<Option<DeskSession>> {
        self.connection
            .query_row(
                "SELECT id_hash, csrf, batch_id, code, code_hash, code_expires_at, paired_user, paired_until FROM desk_sessions
                 WHERE code_hash = ?1 AND code_expires_at > ?2 AND paired_user IS NULL",
                params![code_hash, now],
                row_session,
            )
            .optional()
    }

    /// Missing-invoice follow-ups (one per payment, design §8.3); `body` is the full record.
    pub fn spends(&self, batch: &str) -> rusqlite::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare("SELECT body FROM missing_spends WHERE batch_id = ?1 ORDER BY json_extract(body, '$.payment_date'), id")?;
        statement
            .query_map([batch], |row| row.get::<_, String>(0))?
            .map(|r| r.map(|text| parse(&text)))
            .collect()
    }

    /// Follow-ups still waiting on the applicant, across batches; `applicant` narrows to one person.
    pub fn open_spends(&self, applicant: Option<&str>) -> rusqlite::Result<Vec<Value>> {
        let mut statement = self.connection.prepare(
            "SELECT body FROM missing_spends WHERE status IN ('discovered', 'business', 'waiting')
               AND (?1 IS NULL OR applicant = ?1) ORDER BY json_extract(body, '$.payment_date'), id",
        )?;
        statement
            .query_map([applicant], |row| row.get::<_, String>(0))?
            .map(|r| r.map(|text| parse(&text)))
            .collect()
    }

    pub fn spend(&self, id: &str) -> rusqlite::Result<Option<Value>> {
        self.connection
            .query_row(
                "SELECT body FROM missing_spends WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|found| found.map(|text| parse(&text)))
    }

    pub fn reminded(&self, spend: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT kind, slot FROM reminders WHERE spend_id = ?1 ORDER BY at, kind")?;
        statement
            .query_map([spend], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    pub fn muted_merchants(&self, applicant: &str) -> rusqlite::Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT merchant FROM muted_merchants WHERE applicant = ?1 ORDER BY merchant",
        )?;
        statement
            .query_map([applicant], |row| row.get(0))?
            .collect()
    }

    pub fn pairing_audit(&self, batch: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT sender, result FROM pairing_audit WHERE batch_id = ?1 ORDER BY seq")?;
        statement
            .query_map([batch], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    /// Unpaired sessions of a batch opened since `since`; bounds how many live codes one batch can hold.
    pub fn unpaired_sessions_since(&self, batch: &str, since: i64) -> rusqlite::Result<i64> {
        self.connection.query_row(
            "SELECT COUNT(*) FROM desk_sessions WHERE batch_id = ?1 AND paired_user IS NULL AND created > ?2",
            params![batch, since],
            |row| row.get(0),
        )
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

    pub fn has_history_event(&self, invoice_no: &str, status: &str) -> rusqlite::Result<bool> {
        self.tx
            .query_row(
                "SELECT 1 FROM history_events WHERE invoice_no = ?1 AND status = ?2",
                params![invoice_no, status],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
    }

    /// Appends one history event; the same invoice, status and batch is recorded once.
    pub fn add_history_event(&self, batch: &str, body: &Value) -> rusqlite::Result<bool> {
        let added = self.tx.execute(
            "INSERT OR IGNORE INTO history_events (batch_id, invoice_no, status, body) VALUES (?1, ?2, ?3, ?4)",
            params![batch, body["invoice_no"].as_str().unwrap_or_default(), body["status"].as_str().unwrap_or_default(), body.to_string()],
        )?;
        Ok(added == 1)
    }

    /// A finished batch stops being the applicant's open batch; its records stay.
    pub fn close_batch(&self, batch: &str) -> rusqlite::Result<()> {
        self.tx
            .execute("UPDATE batches SET closed = 1 WHERE id = ?1", [batch])?;
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

    /// A new browser session: no code and no pairing yet.
    pub fn create_session(
        &self,
        id_hash: &str,
        csrf: &str,
        batch: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO desk_sessions (id_hash, csrf, batch_id, created) VALUES (?1, ?2, ?3, ?4)",
            params![id_hash, csrf, batch, now],
        )?;
        Ok(())
    }

    /// Gives a session a new code. False when another live session holds the same code; the caller draws again.
    pub fn issue_code(
        &self,
        session: &str,
        code: &str,
        code_hash: &str,
        expires_at: i64,
        now: i64,
    ) -> rusqlite::Result<bool> {
        self.tx.execute(
            "UPDATE desk_sessions SET code = NULL, code_hash = NULL, code_expires_at = NULL, code_failures = 0
             WHERE code_expires_at <= ?1",
            [now],
        )?;
        match self.tx.execute(
            "UPDATE desk_sessions SET code = ?2, code_hash = ?3, code_expires_at = ?4, code_failures = 0
             WHERE id_hash = ?1 AND paired_user IS NULL",
            params![session, code, code_hash, expires_at],
        ) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Consumes a live code once: binds its session to `user` and withdraws the code. False if it was already gone.
    pub fn claim_code(
        &self,
        session: &str,
        code_hash: &str,
        user: &str,
        until: i64,
        now: i64,
    ) -> rusqlite::Result<bool> {
        Ok(self.tx.execute(
            "UPDATE desk_sessions SET paired_user = ?3, paired_until = ?4, code = NULL, code_hash = NULL,
               code_expires_at = NULL, code_failures = 0
             WHERE id_hash = ?1 AND code_hash = ?2 AND code_expires_at > ?5 AND paired_user IS NULL",
            params![session, code_hash, user, until, now],
        )? == 1)
    }

    /// A wrong code could have been aimed at any live code, so each one spends one of its five attempts;
    /// a code with none left is withdrawn and its page draws a new one.
    pub fn spend_code_attempt(&self, now: i64) -> rusqlite::Result<()> {
        self.tx.execute(
            "UPDATE desk_sessions SET code_failures = code_failures + 1 WHERE code_hash IS NOT NULL AND code_expires_at > ?1",
            [now],
        )?;
        self.tx.execute(
            "UPDATE desk_sessions SET code = NULL, code_hash = NULL, code_expires_at = NULL, code_failures = 0
             WHERE code_failures >= ?1",
            [CODE_ATTEMPTS],
        )?;
        Ok(())
    }

    pub fn audit_pairing(
        &self,
        batch: &str,
        session: &str,
        sender: &str,
        result: &str,
        at: i64,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO pairing_audit (batch_id, session, sender, result, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![batch, &session[..session.len().min(12)], sender, result, at],
        )?;
        Ok(())
    }

    /// Writes a follow-up record; `status` is kept in its own column for the open-follow-up query.
    pub fn put_spend(&self, spend: &Value) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO missing_spends (id, batch_id, applicant, status, body) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET status = excluded.status, body = excluded.body",
            params![
                spend["id"].as_str(),
                spend["batch_id"].as_str(),
                spend["applicant"].as_str(),
                spend["status"].as_str(),
                spend.to_string()
            ],
        )?;
        Ok(())
    }

    /// Records one reminder; false if this (follow-up, kind, slot) was already sent, so it never goes out twice.
    pub fn record_reminder(
        &self,
        spend: &str,
        kind: &str,
        slot: &str,
        at: i64,
    ) -> rusqlite::Result<bool> {
        Ok(self.tx.execute(
            "INSERT OR IGNORE INTO reminders (spend_id, kind, slot, at) VALUES (?1, ?2, ?3, ?4)",
            params![spend, kind, slot, at],
        )? == 1)
    }

    pub fn mute_merchant(&self, applicant: &str, merchant: &str, at: i64) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT OR IGNORE INTO muted_merchants (applicant, merchant, at) VALUES (?1, ?2, ?3)",
            params![applicant, merchant, at],
        )?;
        Ok(())
    }

    pub fn unmute_merchant(&self, applicant: &str, merchant: &str) -> rusqlite::Result<bool> {
        Ok(self.tx.execute(
            "DELETE FROM muted_merchants WHERE applicant = ?1 AND merchant = ?2",
            params![applicant, merchant],
        )? == 1)
    }

    /// Drops unpaired sessions older than their cookie; their codes go with them.
    pub fn drop_stale_sessions(&self, before: i64) -> rusqlite::Result<()> {
        self.tx.execute(
            "DELETE FROM desk_sessions WHERE paired_user IS NULL AND created < ?1",
            [before],
        )?;
        Ok(())
    }

    /// Takes every decision of a batch out of force, e.g. when collection reopens and the batch is read again.
    pub fn supersede_decisions(&self, batch: &str, revision: i64) -> rusqlite::Result<()> {
        self.tx.execute(
            "UPDATE decisions SET superseded_revision = ?2 WHERE batch_id = ?1 AND superseded_revision IS NULL",
            params![batch, revision],
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
        code: row.get(3)?,
        code_hash: row.get(4)?,
        code_expires_at: row.get(5)?,
        paired_user: row.get(6)?,
        paired_until: row.get(7)?,
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
  superseded_revision INTEGER, UNIQUE (batch_id, id));
CREATE TABLE IF NOT EXISTS documents (
  batch_id TEXT NOT NULL REFERENCES batches(id), kind TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY (batch_id, kind));
CREATE TABLE IF NOT EXISTS outbox (
  id INTEGER PRIMARY KEY AUTOINCREMENT, batch_id TEXT NOT NULL, room_id TEXT NOT NULL, txn_id TEXT NOT NULL UNIQUE,
  content TEXT NOT NULL, sent_event TEXT);
CREATE TABLE IF NOT EXISTS inbound (event_id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS desk_sessions (
  id_hash TEXT PRIMARY KEY, csrf TEXT NOT NULL, batch_id TEXT NOT NULL, code_hash TEXT, code_expires_at INTEGER,
  paired_user TEXT, paired_until INTEGER, code TEXT, code_failures INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS history_events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, batch_id TEXT NOT NULL, invoice_no TEXT NOT NULL, status TEXT NOT NULL,
  body TEXT NOT NULL, UNIQUE (invoice_no, status, batch_id));
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS pairing_failures (sender TEXT NOT NULL, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS missing_spends (
  id TEXT PRIMARY KEY, batch_id TEXT NOT NULL, applicant TEXT NOT NULL, status TEXT NOT NULL, body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS reminders (
  spend_id TEXT NOT NULL, kind TEXT NOT NULL, slot TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (spend_id, kind, slot));
CREATE TABLE IF NOT EXISTS muted_merchants (
  applicant TEXT NOT NULL, merchant TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (applicant, merchant));
CREATE TABLE IF NOT EXISTS pairing_audit (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, batch_id TEXT NOT NULL, session TEXT NOT NULL, sender TEXT NOT NULL,
  result TEXT NOT NULL, at INTEGER NOT NULL);
";
