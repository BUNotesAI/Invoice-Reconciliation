//! Batch orchestration: the state machine of design §8 over the core, the agent port and the store.
//!
//! Every mutation runs under one lock (batches are serial), checks actor and revision first, and commits the state
//! change, its audit row and its outgoing messages in one transaction. Long core and model work happens between a
//! guarded read and that commit; a crash in between leaves the previous state, and `executing` is resumed on start.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde_json::{Map, Value, json};

use crate::{
    agent::{AgentPort, canonical_json},
    core_client::{CoreClient, CoreCommand, CoreFailure},
    reconcile::{self, Assessment, Reading, ReconcileError, SourceFile},
    report::{self, yuan},
    store::{Audit, Batch, State, Store, Work, sha256_hex},
    validate::yuan_to_cents,
};

pub const BUSINESS_OFFSET_SECONDS: i64 = 8 * 3600;
pub const VERIFY_ATTEMPTS: i64 = 3;

pub struct ServiceConfig {
    pub data_root: PathBuf,
    pub policy: PathBuf,
    pub python: PathBuf,
    pub core_dir: PathBuf,
    pub history: Value,
    pub period: String,
    pub desk_url: String,
    /// Applicants allowed to open batches, with the direct room the bot uses for reminders.
    pub applicants: BTreeMap<String, String>,
}

#[derive(Debug)]
pub enum ServiceError {
    Forbidden,
    NotFound,
    StaleRevision(i64),
    Invalid(String),
    Core(ReconcileError),
    Store(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden => write!(f, "forbidden"),
            Self::NotFound => write!(f, "not found"),
            Self::StaleRevision(current) => write!(f, "stale revision; current is {current}"),
            Self::Invalid(why) => write!(f, "{why}"),
            Self::Core(error) => write!(f, "{error}"),
            Self::Store(error) => write!(f, "store: {error}"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<rusqlite::Error> for ServiceError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.to_string())
    }
}

impl From<ReconcileError> for ServiceError {
    fn from(error: ReconcileError) -> Self {
        Self::Core(error)
    }
}

impl From<CoreFailure> for ServiceError {
    fn from(failure: CoreFailure) -> Self {
        Self::Core(ReconcileError::Core(failure))
    }
}

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Points inside `execute` where a test can stop the run as if the process died there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Packaged,
    Published,
}

pub type Fault = Arc<dyn Fn(Stage) -> bool + Send + Sync>;

pub struct Service {
    pub config: ServiceConfig,
    store: Mutex<Store>,
    agent: tokio::sync::Mutex<AgentPort>,
    serial: tokio::sync::Mutex<()>,
    clock: Clock,
    fault: Mutex<Option<Fault>>,
}

/// Marks an inbound event handled and queues its replies, inside the caller's unit of work, so the event, the
/// business change and the replies commit together: a crash before the commit replays the event, after it nothing
/// is lost and nothing is sent twice.
fn answer(
    work: &Work<'_>,
    batch: &str,
    room: &str,
    event_id: &str,
    contents: &[Value],
) -> rusqlite::Result<()> {
    work.mark_inbound(event_id)?;
    let key = sha256_hex(event_id.as_bytes());
    for (index, content) in contents.iter().enumerate() {
        work.enqueue(batch, room, &format!("r-{}-{index}", &key[..24]), content)?;
    }
    Ok(())
}

/// A chat reply produced by an inbound event; queued with a transaction id derived from that event.
fn notice(text: &str) -> Value {
    let html = report::escape_html(text).replace('\n', "<br>");
    json!({"msgtype": "m.notice", "body": text, "format": "org.matrix.custom.html", "formatted_body": html})
}

pub fn desk_card(desk_url: &str, batch: &str) -> Value {
    let url = format!("{desk_url}/desk/b/{batch}");
    json!({"msgtype": "rs.robius.robrix.mini_app", "body": format!("[Mini app] 报销对账台\n{url}"),
           "mini_app": {"version": 1, "title": "报销对账台", "url": url}})
}

fn random_hex(bytes: usize) -> String {
    let mut out = String::new();
    while out.len() < bytes * 2 {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(bytes * 2);
    out
}

fn utc(now: i64) -> String {
    let days = now.div_euclid(86_400);
    let seconds = now.rem_euclid(86_400);
    let (year, month, day) = civil(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
pub fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Safe display name for an uploaded file: its last path segment, printable, bounded.
pub fn upload_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("file");
    let clean: String = base.chars().filter(|c| !c.is_control()).take(120).collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() {
        "file".into()
    } else {
        clean
    }
}

impl Service {
    pub fn new(config: ServiceConfig, store: Store, agent: AgentPort, clock: Clock) -> Self {
        Self {
            config,
            store: Mutex::new(store),
            agent: tokio::sync::Mutex::new(agent),
            serial: tokio::sync::Mutex::new(()),
            clock,
            fault: Mutex::new(None),
        }
    }

    /// Test hook: `execute` stops without recording anything at every stage where `fault` returns true.
    #[doc(hidden)]
    pub fn set_fault(&self, fault: Option<Fault>) {
        *self.fault.lock().expect("fault lock poisoned") = fault;
    }

    fn stopped_at(&self, stage: Stage) -> bool {
        self.fault
            .lock()
            .expect("fault lock poisoned")
            .as_ref()
            .is_some_and(|fault| fault(stage))
    }

    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    pub fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> T) -> T {
        f(&mut self.store.lock().expect("store lock poisoned"))
    }

    fn batch_dir(&self, batch: &str) -> PathBuf {
        self.config.data_root.join("batches").join(batch)
    }

    fn core(&self, batch: &str) -> CoreClient {
        CoreClient::new(
            self.config.python.clone(),
            self.config.core_dir.clone(),
            self.config.data_root.clone(),
            self.batch_dir(batch),
            self.config.policy.clone(),
        )
    }

    pub fn batch(&self, id: &str) -> Result<Batch, ServiceError> {
        self.with_store(|store| store.batch(id))?
            .ok_or(ServiceError::NotFound)
    }

    fn reading(&self, batch: &str) -> Result<Reading, ServiceError> {
        let value = self
            .with_store(|store| store.document(batch, "reading"))?
            .ok_or(ServiceError::NotFound)?;
        serde_json::from_value(value).map_err(|e| ServiceError::Invalid(e.to_string()))
    }

    pub fn assessment(&self, batch: &str) -> Result<Option<Assessment>, ServiceError> {
        Ok(self
            .with_store(|store| store.document(batch, "assessment"))?
            .and_then(|value| serde_json::from_value(value).ok()))
    }

    /// Answers one inbound event that changes nothing else: marks it handled and queues the replies together.
    fn reply(
        &self,
        batch: &str,
        room: &str,
        event_id: &str,
        contents: &[Value],
    ) -> Result<(), ServiceError> {
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            answer(&work, batch, room, event_id, contents)?;
            work.commit()
        })?;
        Ok(())
    }

    fn seen(&self, event_id: &str) -> Result<bool, ServiceError> {
        Ok(self.with_store(|store| store.inbound_seen(event_id))?)
    }

    /// Runs one inbound handler; if it fails before committing, the event is answered once with an error notice
    /// and marked handled, so the person knows to send it again.
    async fn inbound(
        &self,
        room: &str,
        event_id: &str,
        handler: impl std::future::Future<Output = Result<(), ServiceError>>,
    ) -> Result<(), ServiceError> {
        match handler.await {
            Ok(()) => Ok(()),
            Err(error) => {
                let _serial = self.serial.lock().await;
                if !self.seen(event_id)? {
                    self.reply(
                        "-",
                        room,
                        event_id,
                        &[notice(
                            "这一步出错了，已记录。请把刚才的消息或文件再发一次。",
                        )],
                    )?;
                }
                Err(error)
            }
        }
    }

    fn open_batch(&self, sender: &str, room: &str) -> Result<Batch, ServiceError> {
        if let Some(batch) = self.with_store(|store| store.open_batch_for(sender))? {
            return Ok(batch);
        }
        let batch = Batch {
            id: format!("b{}", random_hex(8)),
            applicant: sender.to_string(),
            room_id: room.to_string(),
            period: self.config.period.clone(),
            state: State::Standby,
            revision: 0,
            snapshot_hash: None,
            published_revision: None,
            verify_attempts: 0,
            resume_state: None,
        };
        let now = self.now();
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            work.create_batch(&batch, now)?;
            work.commit()
        })?;
        Ok(batch)
    }

    fn allowed_applicant(&self, sender: &str) -> bool {
        self.config.applicants.contains_key(sender)
    }

    /// Step 2: a file sent in chat. Content-addressed, so the same file twice changes nothing.
    pub async fn receive_file(
        &self,
        sender: &str,
        room: &str,
        event_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), ServiceError> {
        self.inbound(
            room,
            event_id,
            self.take_file(sender, room, event_id, name, bytes),
        )
        .await
    }

    async fn take_file(
        &self,
        sender: &str,
        room: &str,
        event_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        if self.seen(event_id)? {
            return Ok(());
        }
        if !self.allowed_applicant(sender) {
            return self.reply(
                "-",
                room,
                event_id,
                &[notice("你不在报销名单里，这个对话不能开报销批次。")],
            );
        }
        let batch = self.open_batch(sender, room)?;
        if !matches!(batch.state, State::Standby | State::Collecting) {
            return self.reply(&batch.id, room, event_id, &[notice(&format!(
                "本批次已开始对账（{}），新文件没有收进来。要重开收件，请回复「重开收件」，再重新发送文件。",
                batch.state.label()
            ))]);
        }
        let display = upload_name(name);
        let folder = self.config.data_root.join("uploads").join(&batch.id);
        std::fs::create_dir_all(&folder)
            .map_err(|e| ServiceError::Invalid(e.kind().to_string()))?;
        let path = folder.join(format!(
            "{}-{}",
            &sha256_hex(bytes)[..16],
            &sha256_hex(display.as_bytes())[..8]
        ));
        write_private(&path, bytes)?;
        let source = match reconcile::ingest(&self.core(&batch.id), &path, &display).await {
            Ok(source) => source,
            Err(ReconcileError::Core(CoreFailure::Rejected { message, .. })) => {
                return self.reply(
                    &batch.id,
                    room,
                    event_id,
                    &[notice(&format!(
                        "文件「{display}」没有收进来：{}。",
                        refusal(&message)
                    ))],
                );
            }
            Err(error) => return Err(error.into()),
        };
        let now = self.now();
        self.with_store(|store| -> rusqlite::Result<()> {
            let before = store.files(&batch.id)?.len();
            let work = store.begin()?;
            let added = work.add_file(&batch.id, &source.id, &display)?;
            if added {
                work.transition(
                    &batch,
                    State::Collecting,
                    batch.revision + 1,
                    Audit {
                        event: "upload",
                        actor: sender,
                        payload: &json!({"source": source.id}),
                        at: now,
                    },
                )?;
            }
            let text = if added {
                format!(
                    "收到「{display}」，本批次共 {} 个文件。继续发，或说「开始对账」。",
                    before + 1
                )
            } else {
                format!("「{display}」已处理过，未重复入账。")
            };
            answer(&work, &batch.id, room, event_id, &[notice(&text)])?;
            work.commit()
        })?;
        Ok(())
    }

    /// A file the bot could not take (too large, download failed): say so once, change nothing.
    pub async fn refuse_upload(
        &self,
        sender: &str,
        room: &str,
        event_id: &str,
        name: &str,
        reason: &str,
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        if self.seen(event_id)? {
            return Ok(());
        }
        let _ = sender;
        self.reply(
            "-",
            room,
            event_id,
            &[notice(&format!(
                "文件「{}」没有收进来：{reason}。",
                upload_name(name)
            ))],
        )
    }

    /// Chat text: commands, pairing codes, or a hint.
    pub async fn receive_text(
        &self,
        sender: &str,
        room: &str,
        event_id: &str,
        text: &str,
    ) -> Result<(), ServiceError> {
        let text = text.trim();
        if text.len() == 6 && text.bytes().all(|b| b.is_ascii_digit()) {
            return self
                .inbound(room, event_id, self.pair(sender, room, event_id, text))
                .await;
        }
        match text {
            "开始对账" => {
                self.inbound(room, event_id, self.start(sender, room, event_id))
                    .await
            }
            "重开收件" => {
                self.inbound(room, event_id, self.reopen(sender, room, event_id))
                    .await
            }
            "重试" => {
                self.inbound(room, event_id, self.retry(sender, room, event_id))
                    .await
            }
            _ => Ok(()),
        }
    }

    async fn start(&self, sender: &str, room: &str, event_id: &str) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        if self.seen(event_id)? || !self.allowed_applicant(sender) {
            return Ok(());
        }
        let Some(batch) = self.with_store(|store| store.open_batch_for(sender))? else {
            return self.reply(
                "-",
                room,
                event_id,
                &[notice(
                    "还没有收到文件。先把发票、账单和行程单发给我，再说「开始对账」。",
                )],
            );
        };
        if batch.state != State::Collecting {
            return self.reply(
                &batch.id,
                room,
                event_id,
                &[notice(&format!(
                    "本批次当前是「{}」，不能开始对账。",
                    batch.state.label()
                ))],
            );
        }
        self.run_reconciliation(batch, sender, event_id).await
    }

    async fn run_reconciliation(
        &self,
        batch: Batch,
        actor: &str,
        event_id: &str,
    ) -> Result<(), ServiceError> {
        let files = self.with_store(|store| store.files(&batch.id))?;
        let core = self.core(&batch.id);
        let mut sources = Vec::new();
        for (source_id, name) in &files {
            let record: Value = serde_json::from_slice(
                &std::fs::read(
                    self.batch_dir(&batch.id)
                        .join("objects")
                        .join(format!("{source_id}.json")),
                )
                .map_err(|e| ServiceError::Invalid(e.kind().to_string()))?,
            )
            .map_err(|e| ServiceError::Invalid(e.to_string()))?;
            sources.push(SourceFile {
                id: source_id.clone(),
                detected_type: record["detected_type"]
                    .as_str()
                    .unwrap_or("unsupported")
                    .to_string(),
                original_name: name.clone(),
            });
        }
        let outcome = {
            let mut agent = self.agent.lock().await;
            match reconcile::read(
                &core,
                &mut agent,
                &sources,
                &self.config.history,
                &batch.period,
            )
            .await
            {
                Ok(mut reading) => reconcile::assess(
                    &core,
                    &mut agent,
                    &mut reading,
                    &[],
                    &batch.period,
                    &applicant_name(&batch.applicant),
                    true,
                )
                .await
                .map(|assessment| (reading, assessment)),
                Err(error) => Err(error),
            }
        };
        let now = self.now();
        match outcome {
            Ok((reading, assessment)) => {
                let to = if assessment.report.needs_decision.count > 0 {
                    State::NeedsDecision
                } else {
                    State::AwaitingConfirm
                };
                let text = assessment.report.text.clone();
                self.with_store(|store| -> rusqlite::Result<()> {
                    let work = store.begin()?;
                    work.put_document(
                        &batch.id,
                        "reading",
                        &serde_json::to_value(&reading).unwrap_or_default(),
                    )?;
                    work.put_document(
                        &batch.id,
                        "assessment",
                        &serde_json::to_value(&assessment).unwrap_or_default(),
                    )?;
                    work.transition(
                        &batch,
                        to,
                        batch.revision + 1,
                        Audit {
                            event: "start_reconciliation",
                            actor,
                            payload: &json!({}),
                            at: now,
                        },
                    )?;
                    // A fresh reading asks every question again; earlier decisions stay only as record.
                    work.supersede_decisions(&batch.id, batch.revision + 1)?;
                    work.set_fields(
                        &batch.id,
                        batch.snapshot_hash.as_deref(),
                        batch.published_revision,
                        batch.verify_attempts,
                        None,
                    )?;
                    answer(
                        &work,
                        &batch.id,
                        &batch.room_id,
                        event_id,
                        &[notice(&text), desk_card(&self.config.desk_url, &batch.id)],
                    )?;
                    work.commit()
                })?;
                Ok(())
            }
            Err(error) => {
                // A retry that fails again keeps the step it was retrying.
                let resume = if batch.state == State::Manual {
                    batch.resume_state.unwrap_or(State::Collecting)
                } else {
                    batch.state
                };
                self.with_store(|store| -> rusqlite::Result<()> {
                    let work = store.begin()?;
                    work.transition(
                        &batch,
                        State::Manual,
                        batch.revision,
                        Audit {
                            event: "core_failed",
                            actor: "system",
                            payload: &json!({"error": error.to_string()}),
                            at: now,
                        },
                    )?;
                    work.set_fields(
                        &batch.id,
                        batch.snapshot_hash.as_deref(),
                        batch.published_revision,
                        batch.verify_attempts,
                        Some(resume),
                    )?;
                    answer(
                        &work,
                        &batch.id,
                        &batch.room_id,
                        event_id,
                        &[notice("这一步出错了，已记录，可以回复「重试」。")],
                    )?;
                    work.commit()
                })?;
                Ok(())
            }
        }
    }

    /// Reopening collection leads to a fresh reading, which takes every earlier decision out of force.
    async fn reopen(&self, sender: &str, room: &str, event_id: &str) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        if self.seen(event_id)? {
            return Ok(());
        }
        let Some(batch) = self.with_store(|store| store.open_batch_for(sender))? else {
            return Ok(());
        };
        if !matches!(batch.state, State::NeedsDecision | State::AwaitingConfirm) {
            return self.reply(
                &batch.id,
                room,
                event_id,
                &[notice(&format!(
                    "本批次当前是「{}」，不能重开收件。",
                    batch.state.label()
                ))],
            );
        }
        let now = self.now();
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            work.transition(
                &batch,
                State::Collecting,
                batch.revision + 1,
                Audit {
                    event: "reopen_collection",
                    actor: sender,
                    payload: &json!({}),
                    at: now,
                },
            )?;
            answer(
                &work,
                &batch.id,
                room,
                event_id,
                &[notice(
                    "已重开收件，之前的文件都保留；之前在对账台做的判断作废，重新对账后请再判断一次。继续发文件，发完说「开始对账」。",
                )],
            )?;
            work.commit()
        })?;
        Ok(())
    }

    async fn retry(&self, sender: &str, room: &str, event_id: &str) -> Result<(), ServiceError> {
        let batch = {
            let _serial = self.serial.lock().await;
            if self.seen(event_id)? {
                return Ok(());
            }
            let Some(batch) = self.with_store(|store| store.open_batch_for(sender))? else {
                return Ok(());
            };
            if batch.state != State::Manual {
                return self.reply(
                    &batch.id,
                    room,
                    event_id,
                    &[notice("当前没有需要重试的步骤。")],
                );
            }
            let resume = batch.resume_state.unwrap_or(State::Collecting);
            if resume == State::Collecting {
                // Reading again moves the batch straight from manual to its result and answers this event.
                return self.run_reconciliation(batch, sender, event_id).await;
            }
            let now = self.now();
            self.with_store(|store| -> rusqlite::Result<()> {
                let work = store.begin()?;
                work.transition(
                    &batch,
                    resume,
                    batch.revision,
                    Audit {
                        event: "retry",
                        actor: sender,
                        payload: &json!({}),
                        at: now,
                    },
                )?;
                work.set_fields(
                    &batch.id,
                    batch.snapshot_hash.as_deref(),
                    batch.published_revision,
                    batch.verify_attempts,
                    None,
                )?;
                let contents = if resume == State::Executing {
                    Vec::new()
                } else {
                    vec![notice(&format!("已回到「{}」。", resume.label()))]
                };
                answer(&work, &batch.id, room, event_id, &contents)?;
                work.commit()
            })?;
            self.batch(&batch.id)?
        };
        if batch.state == State::Executing {
            self.execute(&batch.id).await?;
        }
        Ok(())
    }

    // --- Pairing ---------------------------------------------------------------------------------------------

    /// A code typed in chat binds the waiting desk session to the sender the homeserver authenticated.
    /// The code is consumed atomically; every outcome is written to the pairing audit with the chat reply.
    async fn pair(
        &self,
        sender: &str,
        room: &str,
        event_id: &str,
        code: &str,
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        if self.seen(event_id)? {
            return Ok(());
        }
        let now = self.now();
        let recent = self.with_store(|store| store.failures_since(sender, now - 600))?;
        if recent >= 5 {
            return self.reply(
                "-",
                room,
                event_id,
                &[notice("配对码错误次数过多，请 10 分钟后再试。")],
            );
        }
        let code_hash = sha256_hex(code.as_bytes());
        let found = self.with_store(|store| store.session_by_code(&code_hash, now))?;
        let refuse = |batch: &str, session: &str, result: &str, text: &str| {
            self.with_store(|store| -> rusqlite::Result<()> {
                let work = store.begin()?;
                work.record_failure(sender, now)?;
                work.spend_code_attempt(now)?;
                work.audit_pairing(batch, session, sender, result, now)?;
                answer(&work, "-", room, event_id, &[notice(text)])?;
                work.commit()
            })
        };
        const INVALID: &str = "配对码无效或已过期。请在对账台页面刷新后使用新的配对码。";
        let Some(session) = found else {
            refuse("-", "-", "unknown_code", INVALID)?;
            return Ok(());
        };
        let batch = self.batch(&session.batch_id)?;
        if !self.may_view(&batch, sender)? {
            refuse(
                &batch.id,
                &session.id_hash,
                "not_allowed",
                "这个批次不属于你，不能配对。",
            )?;
            return Ok(());
        }
        let paired = self.with_store(|store| -> rusqlite::Result<bool> {
            let work = store.begin()?;
            if !work.claim_code(&session.id_hash, &code_hash, sender, now + 3600, now)? {
                return Ok(false);
            }
            work.audit_pairing(&batch.id, &session.id_hash, sender, "paired", now)?;
            answer(
                &work,
                &batch.id,
                room,
                event_id,
                &[notice("配对成功。回到对账台页面即可继续，1 小时内有效。")],
            )?;
            work.commit()?;
            Ok(true)
        })?;
        if !paired {
            refuse(&batch.id, &session.id_hash, "code_gone", INVALID)?;
        }
        Ok(())
    }

    pub fn may_view(&self, batch: &Batch, user: &str) -> Result<bool, ServiceError> {
        if batch.applicant == user {
            return Ok(true);
        }
        let finance = self
            .reading(&batch.id)
            .map(|reading| reading.finance)
            .unwrap_or_default();
        Ok(finance.iter().any(|member| member == user))
    }

    fn applicant_only(&self, batch: &Batch, user: &str, expected: i64) -> Result<(), ServiceError> {
        if batch.applicant != user {
            return Err(ServiceError::Forbidden);
        }
        if batch.revision != expected {
            return Err(ServiceError::StaleRevision(batch.revision));
        }
        Ok(())
    }

    // --- Desk mutations ----------------------------------------------------------------------------------------

    /// Records one applicant decision and re-assesses the batch (step 9).
    pub async fn decide(
        &self,
        batch_id: &str,
        user: &str,
        expected: i64,
        item_id: &str,
        kind: &str,
        payload: Value,
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        let batch = self.batch(batch_id)?;
        self.applicant_only(&batch, user, expected)?;
        if !matches!(batch.state, State::NeedsDecision | State::AwaitingConfirm) {
            return Err(ServiceError::Invalid(
                "batch is not waiting for decisions".into(),
            ));
        }
        let mut reading = self.reading(batch_id)?;
        let assessment = self.assessment(batch_id)?.ok_or(ServiceError::NotFound)?;
        let view = assessment
            .report
            .items
            .iter()
            .find(|view| view.item_id == item_id)
            .ok_or(ServiceError::NotFound)?;
        allowed_kind(kind, &view.reasons, &view.disposition)?;
        let now = self.now();
        let decision_id = format!("d-{}", random_hex(8));
        let decision = json!({"id": decision_id, "item_id": item_id, "kind": kind, "payload": payload, "actor": user,
                              "at": utc(now), "expected_revision": expected, "source_event_id": format!("desk-{}", random_hex(8))});
        if kind == "reject"
            && let Some(item) = reading.items.iter_mut().find(|item| item.id == item_id)
        {
            item.rejected = true;
        }
        let mut decisions = self.with_store(|store| store.decisions(batch_id))?;
        decisions.push(decision.clone());
        self.reassess_and_commit(
            &batch,
            &mut reading,
            &decisions,
            Some((&decision_id, &decision)),
            "decide_item",
            user,
        )
        .await
    }

    /// The applicant checked an image invoice against its original and confirmed or corrected every field.
    pub async fn confirm_visual(
        &self,
        batch_id: &str,
        user: &str,
        expected: i64,
        item_id: &str,
        corrections: &Map<String, Value>,
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        let batch = self.batch(batch_id)?;
        self.applicant_only(&batch, user, expected)?;
        if batch.state != State::NeedsDecision {
            return Err(ServiceError::Invalid(
                "batch is not waiting for decisions".into(),
            ));
        }
        let mut reading = self.reading(batch_id)?;
        let now = self.now();
        let event = format!("desk-{}", random_hex(8));
        let item = reading
            .items
            .iter_mut()
            .find(|item| item.id == item_id)
            .ok_or(ServiceError::NotFound)?;
        let candidate = item
            .invoice
            .clone()
            .ok_or_else(|| ServiceError::Invalid("no reading to confirm".into()))?;
        // Each item is confirmed once; its facts stay bound to that one confirmation decision.
        if candidate["invoice_no"]["level"] != "candidate" {
            return Err(ServiceError::Invalid("reading already confirmed".into()));
        }
        let confirmed = confirm_invoice(&candidate, corrections, &event, user, &utc(now))?;
        // The core re-validates the corrected facts; a format error is returned to the page, nothing is stored.
        let core = self.core(batch_id);
        core.call(
            CoreCommand::Gates,
            &json!({"invoices": [confirmed], "history_snapshot": reading.history_snapshot}),
        )
        .await
        .map_err(|failure| {
            ServiceError::Invalid(format!("corrected reading is not valid: {failure}"))
        })?;
        item.invoice = Some(confirmed.clone());
        item.category = None;
        item.short_name = None;
        let fact_ids = confirmed_ids(&confirmed);
        let decision_id = format!("d-{}", random_hex(8));
        let decision = json!({"id": decision_id, "item_id": item_id, "kind": "confirm_visual", "payload": {"fact_ids": fact_ids},
                              "actor": user, "at": utc(now), "expected_revision": expected, "source_event_id": event});
        {
            let mut agent = self.agent.lock().await;
            let mut steps = Vec::new();
            reconcile::classify(&core, &mut agent, &mut reading, &mut steps).await?;
            reading.rules_steps.extend(steps);
        }
        let mut decisions = self.with_store(|store| store.decisions(batch_id))?;
        decisions.push(decision.clone());
        self.reassess_and_commit(
            &batch,
            &mut reading,
            &decisions,
            Some((&decision_id, &decision)),
            "confirm_visual",
            user,
        )
        .await
    }

    /// The applicant confirmed or corrected a screenshot reading; it becomes evidence that may be linked.
    pub async fn confirm_screenshot(
        &self,
        batch_id: &str,
        user: &str,
        expected: i64,
        source_id: &str,
        corrections: &Map<String, Value>,
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        let batch = self.batch(batch_id)?;
        self.applicant_only(&batch, user, expected)?;
        if batch.state != State::NeedsDecision {
            return Err(ServiceError::Invalid(
                "batch is not waiting for decisions".into(),
            ));
        }
        let mut reading = self.reading(batch_id)?;
        let shot = reading
            .screenshots
            .iter()
            .find(|shot| shot.source_id == source_id)
            .ok_or(ServiceError::NotFound)?;
        let mut fields = shot.fields.clone().unwrap_or_else(
            || json!({"merchant": "", "amount_cents": 0, "service_date": "", "order_ref": ""}),
        );
        for (key, value) in corrections {
            match (key.as_str(), value.as_str()) {
                ("amount", Some(text)) => {
                    fields["amount_cents"] = json!(
                        yuan_to_cents(text)
                            .ok_or_else(|| ServiceError::Invalid("amount".into()))?
                    )
                }
                ("merchant" | "service_date" | "order_ref", Some(text)) => {
                    fields[key] = json!(text.trim())
                }
                _ => return Err(ServiceError::Invalid(format!("unknown field {key}"))),
            }
        }
        let now = self.now();
        let confirmation = json!({"confirmed_by": user, "confirmation_event": format!("desk-{}", random_hex(8)), "confirmed_at": utc(now)});
        let result = self
            .core(batch_id)
            .call(CoreCommand::Evidence, &json!({"source_file_id": source_id, "confirmed_visual_facts": {"fields": fields, "confirmation": confirmation}}))
            .await
            .map_err(|failure| ServiceError::Invalid(format!("screenshot reading is not valid: {failure}")))?;
        let confirmed: Vec<Value> = result["evidence"].as_array().cloned().unwrap_or_default();
        let ids: BTreeSet<&str> = confirmed.iter().filter_map(|e| e["id"].as_str()).collect();
        reading
            .evidence
            .retain(|entry| entry["id"].as_str().is_none_or(|id| !ids.contains(id)));
        reading.evidence.extend(confirmed);
        if let Some(shot) = reading
            .screenshots
            .iter_mut()
            .find(|shot| shot.source_id == source_id)
        {
            shot.fields = None;
        }
        let decisions = self.with_store(|store| store.decisions(batch_id))?;
        self.reassess_and_commit(
            &batch,
            &mut reading,
            &decisions,
            None,
            "evidence_confirmed",
            user,
        )
        .await
    }

    async fn reassess_and_commit(
        &self,
        batch: &Batch,
        reading: &mut Reading,
        decisions: &[Value],
        new_decision: Option<(&str, &Value)>,
        event: &str,
        actor: &str,
    ) -> Result<(), ServiceError> {
        let core = self.core(&batch.id);
        // Only decisions for items still in the batch are core decisions; a reject removes its item instead.
        let active: Vec<Value> = decisions
            .iter()
            .filter(|d| d["kind"] != "reject")
            .filter(|d| {
                reading.items.iter().any(|item| {
                    !item.rejected && item.id == d["item_id"].as_str().unwrap_or_default()
                })
            })
            .cloned()
            .collect();
        let assessment = {
            let mut agent = self.agent.lock().await;
            reconcile::assess(
                &core,
                &mut agent,
                reading,
                &active,
                &batch.period,
                &applicant_name(&batch.applicant),
                false,
            )
            .await?
        };
        let to = if assessment.report.needs_decision.count > 0 {
            State::NeedsDecision
        } else {
            State::AwaitingConfirm
        };
        let now = self.now();
        let finished = to == State::AwaitingConfirm && batch.state == State::NeedsDecision;
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            if let Some((id, decision)) = new_decision {
                work.add_decision(&batch.id, id, decision)?;
            }
            work.put_document(
                &batch.id,
                "reading",
                &serde_json::to_value(&*reading).unwrap_or_default(),
            )?;
            work.put_document(
                &batch.id,
                "assessment",
                &serde_json::to_value(&assessment).unwrap_or_default(),
            )?;
            work.transition(
                batch,
                to,
                batch.revision + 1,
                Audit {
                    event,
                    actor,
                    payload: &new_decision.map(|(_, d)| d.clone()).unwrap_or_default(),
                    at: now,
                },
            )?;
            if finished {
                work.enqueue(
                    &batch.id,
                    &batch.room_id,
                    &format!("{}-{}-all-decided", batch.id, batch.revision + 1),
                    &notice("所有待判断项都处理完了。请在对账台核对后点「确认生成」。"),
                )?;
            }
            work.commit()
        })?;
        Ok(())
    }

    /// Step 10: confirmation bound to a revision freezes a snapshot, then executes it.
    pub async fn confirm(
        &self,
        batch_id: &str,
        user: &str,
        expected: i64,
    ) -> Result<(), ServiceError> {
        self.freeze(batch_id, user, expected).await?;
        self.execute(batch_id).await
    }

    /// Stores the immutable snapshot and moves to `executing`; a crash after this is finished by `recover`.
    pub async fn freeze(
        &self,
        batch_id: &str,
        user: &str,
        expected: i64,
    ) -> Result<String, ServiceError> {
        {
            let _serial = self.serial.lock().await;
            let batch = self.batch(batch_id)?;
            self.applicant_only(&batch, user, expected)?;
            if batch.state != State::AwaitingConfirm {
                return Err(ServiceError::Invalid(format!(
                    "batch is {}",
                    batch.state.as_str()
                )));
            }
            let reading = self.reading(batch_id)?;
            let assessment = self.assessment(batch_id)?.ok_or(ServiceError::NotFound)?;
            let decisions = self.with_store(|store| store.decisions(batch_id))?;
            let snapshot = build_snapshot(&batch, &reading, &assessment, &decisions)?;
            let hash = sha256_hex(canonical_json(&snapshot).as_bytes());
            let now = self.now();
            self.with_store(|store| -> rusqlite::Result<()> {
                let work = store.begin()?;
                work.put_document(batch_id, "snapshot", &snapshot)?;
                work.transition(
                    &batch,
                    State::Executing,
                    batch.revision,
                    Audit {
                        event: "confirm",
                        actor: user,
                        payload: &json!({"snapshot": hash}),
                        at: now,
                    },
                )?;
                work.set_fields(batch_id, Some(&hash), batch.published_revision, 0, None)?;
                work.commit()
            })?;
            Ok(hash)
        }
    }

    pub async fn decline(
        &self,
        batch_id: &str,
        user: &str,
        expected: i64,
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        let batch = self.batch(batch_id)?;
        self.applicant_only(&batch, user, expected)?;
        if batch.state != State::AwaitingConfirm {
            return Err(ServiceError::Invalid(
                "batch is not awaiting confirmation".into(),
            ));
        }
        let now = self.now();
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            work.transition(
                &batch,
                State::Standby,
                batch.revision + 1,
                Audit {
                    event: "decline",
                    actor: user,
                    payload: &json!({}),
                    at: now,
                },
            )?;
            work.commit()
        })?;
        Ok(())
    }

    /// Package, verify, publish and queue the result post. Idempotent: rerunning after a crash reaches the same end.
    pub async fn execute(&self, batch_id: &str) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        let batch = self.batch(batch_id)?;
        if batch.state != State::Executing {
            return Ok(());
        }
        let snapshot = self
            .with_store(|store| store.document(batch_id, "snapshot"))?
            .ok_or(ServiceError::NotFound)?;
        let hash = batch.snapshot_hash.clone().ok_or(ServiceError::NotFound)?;
        let reading = self.reading(batch_id)?;
        let core = self.core(batch_id);
        let packaged = core
            .call(
                CoreCommand::Package,
                &json!({"confirmed_snapshot": snapshot, "expected_snapshot_hash": hash}),
            )
            .await;
        let now = self.now();
        let packaged = match packaged {
            Ok(value) => value,
            Err(CoreFailure::Rejected { code, .. }) if code == "OUTPUT_BUSY" => {
                return self.finish(&batch, State::OutputWait, "output_busy", None, now);
            }
            Err(failure) => return self.fail(&batch, &failure.to_string(), now),
        };
        if self.stopped_at(Stage::Packaged) {
            return Err(ServiceError::Invalid("stopped after packaging".into()));
        }
        let verified = match core
            .call(CoreCommand::Verify, &json!({"snapshot_hash": hash, "manifest_object_id": hash, "history_snapshot": reading.history_snapshot}))
            .await
        {
            Ok(value) => value,
            Err(failure) => return self.fail(&batch, &failure.to_string(), now),
        };
        let checks = verified["checks"].as_array().map(Vec::len).unwrap_or(0);
        if verified["passed"] != json!(true) {
            let attempts = batch.verify_attempts + 1;
            let to = if attempts >= VERIFY_ATTEMPTS {
                State::Manual
            } else {
                State::NeedsDecision
            };
            self.with_store(|store| -> rusqlite::Result<()> {
                let work = store.begin()?;
                work.transition(
                    &batch,
                    to,
                    batch.revision + 1,
                    Audit {
                        event: "verify_failed",
                        actor: "system",
                        payload: &verified,
                        at: now,
                    },
                )?;
                work.set_fields(
                    batch_id,
                    batch.snapshot_hash.as_deref(),
                    batch.published_revision,
                    attempts,
                    Some(State::AwaitingConfirm),
                )?;
                work.enqueue(
                    batch_id,
                    &batch.room_id,
                    &format!("{batch_id}-{hash}-verify-{attempts}"),
                    &notice("终审没有通过，报销包未发布。已退回待判断，请在对账台查看原因。"),
                )?;
                work.commit()
            })?;
            return Ok(());
        }
        publish(&self.batch_dir(batch_id), &hash, batch.revision)
            .map_err(|e| ServiceError::Invalid(e.to_string()))?;
        if self.stopped_at(Stage::Published) {
            return Err(ServiceError::Invalid("stopped after publishing".into()));
        }
        let manifest = &packaged["staging_manifest"];
        let rows = manifest["rows"].as_array().map(Vec::len).unwrap_or(0);
        let total = manifest["total_cents"].as_i64().unwrap_or(0);
        let text = format!(
            "终审全部通过（{checks}/{checks}）。报销包已生成：{rows} 张发票，合计 {}，交接清单 {}。\n在对账台可以下载；确认无误后再分享给财务。",
            yuan(total),
            manifest["ledger_name"].as_str().unwrap_or("")
        );
        self.finish(
            &batch,
            State::ReadyToShare,
            "verify_passed",
            Some((&hash, &text)),
            now,
        )
    }

    fn finish(
        &self,
        batch: &Batch,
        to: State,
        event: &str,
        post: Option<(&str, &str)>,
        now: i64,
    ) -> Result<(), ServiceError> {
        let desk = desk_card(&self.config.desk_url, &batch.id);
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            work.transition(
                batch,
                to,
                batch.revision,
                Audit {
                    event,
                    actor: "system",
                    payload: &json!({}),
                    at: now,
                },
            )?;
            if let Some((hash, text)) = post {
                work.set_fields(
                    &batch.id,
                    Some(hash),
                    Some(batch.revision),
                    batch.verify_attempts,
                    None,
                )?;
                work.enqueue(
                    &batch.id,
                    &batch.room_id,
                    &format!("{}-{hash}-published", batch.id),
                    &notice(text),
                )?;
                work.enqueue(
                    &batch.id,
                    &batch.room_id,
                    &format!("{}-{hash}-published-card", batch.id),
                    &desk,
                )?;
            }
            work.commit()
        })?;
        Ok(())
    }

    fn fail(&self, batch: &Batch, error: &str, now: i64) -> Result<(), ServiceError> {
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            work.transition(
                batch,
                State::Manual,
                batch.revision,
                Audit {
                    event: "core_failed",
                    actor: "system",
                    payload: &json!({"error": error}),
                    at: now,
                },
            )?;
            work.set_fields(
                &batch.id,
                batch.snapshot_hash.as_deref(),
                batch.published_revision,
                batch.verify_attempts,
                Some(State::Executing),
            )?;
            work.enqueue(
                &batch.id,
                &batch.room_id,
                &format!("{}-{}-failed-{now}", batch.id, batch.revision),
                &notice("这一步出错了，已记录，可以回复「重试」。"),
            )?;
            work.commit()
        })?;
        Ok(())
    }

    /// Restart recovery: finish any batch that was executing when the process stopped.
    pub async fn recover(&self) -> Result<(), ServiceError> {
        let executing: Vec<String> = self
            .with_store(|store| store.batches())?
            .into_iter()
            .filter(|batch| matches!(batch.state, State::Executing | State::OutputWait))
            .map(|batch| batch.id)
            .collect();
        for id in executing {
            let batch = self.batch(&id)?;
            if batch.state == State::OutputWait {
                let now = self.now();
                self.with_store(|store| -> rusqlite::Result<()> {
                    let work = store.begin()?;
                    work.transition(
                        &batch,
                        State::Executing,
                        batch.revision,
                        Audit {
                            event: "retry",
                            actor: "system",
                            payload: &json!({}),
                            at: now,
                        },
                    )?;
                    work.commit()
                })?;
            }
            self.execute(&id).await?;
        }
        Ok(())
    }

    /// Step 1: the month-end reminder, at most once per applicant and period (the transaction id is the key).
    pub fn remind(&self) -> Result<usize, ServiceError> {
        let now = self.now() + BUSINESS_OFFSET_SECONDS;
        let (year, month, day) = civil(now.div_euclid(86_400));
        let (_, next_month, _) = civil(now.div_euclid(86_400) + 1);
        let last_day = next_month != month;
        if !last_day || now.rem_euclid(86_400) < 9 * 3600 {
            return Ok(0);
        }
        let period = format!("{year:04}-{month:02}");
        let mut queued = 0;
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            for (applicant, room) in &self.config.applicants {
                let text = format!("今天是 {period} 最后一天（{day} 日）。把本月的发票、微信账单和滴滴行程单发给我，发完说「开始对账」。");
                work.enqueue("-", room, &format!("remind-{}-{period}", &sha256_hex(applicant.as_bytes())[..16]), &notice(&text))?;
                queued += 1;
            }
            work.commit()
        })?;
        Ok(queued)
    }
}

fn refusal(message: &str) -> &'static str {
    match message {
        m if m.contains("size limit") => "文件超过 20 MB",
        m if m.contains("page limit") => "PDF 超过 20 页",
        m if m.contains("pixel limit") => "图片超过 4000 万像素",
        m if m.contains("file limit") => "本批次已满 100 个文件",
        _ => "文件读取失败",
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| ServiceError::Invalid(e.kind().to_string()))?;
    std::io::Write::write_all(&mut file, bytes)
        .map_err(|e| ServiceError::Invalid(e.kind().to_string()))
}

/// Decision kinds each review reason admits; anything else is refused before it reaches the core.
fn allowed_kind(kind: &str, reasons: &[String], disposition: &str) -> Result<(), ServiceError> {
    if disposition == "rejected" {
        return Err(ServiceError::Invalid(
            "rejected items take no decision".into(),
        ));
    }
    let admits = |reason: &str| -> &'static [&'static str] {
        match reason {
            "OVER_LIMIT" => &["explain_over_limit"],
            "REPLACEMENT_REQUIRES_DECISION" => &["replace_unpaid_invoice"],
            "HISTORY_ALREADY_PAID" => &["receipt_only"],
            "EVIDENCE_MISSING"
            | "DATE_CONFLICT"
            | "SERVICE_DATE_MISSING"
            | "STAY_PERIOD_MISSING" => &["manual_evidence"],
            "MULTIPLE_CANDIDATES" | "LINK_CONFLICT" => &["choose_evidence"],
            _ => &[],
        }
    };
    if kind == "reject" || reasons.iter().any(|reason| admits(reason).contains(&kind)) {
        Ok(())
    } else {
        Err(ServiceError::Invalid(format!(
            "decision {kind} does not answer this item"
        )))
    }
}

/// The applicant's check of a vision reading: every fact becomes confirmed, corrected values replace read ones.
pub fn confirm_invoice(
    candidate: &Value,
    corrections: &Map<String, Value>,
    event: &str,
    actor: &str,
    at: &str,
) -> Result<Value, ServiceError> {
    let mut invoice = candidate.clone();
    let confirm = |fact: &Value, value: Value| {
        let id = fact["id"].as_str().unwrap_or_default();
        json!({"id": format!("{}.confirmed", id.trim_end_matches(".candidate")), "value": value, "level": "confirmed",
               "source": {"file_sha256": fact["source"]["file_sha256"], "method": "user",
                          "locator": {"type": "user_confirmation", "event_id": event, "actor": actor, "confirmed_at": at,
                                      "original_fact_id": id}},
               "validation_results": ["user_checked"], "candidate_id": id, "confirmed_by": actor,
               "confirmation_event": event, "confirmed_at": at})
    };
    let corrected = |name: &str, fact: &Value| -> Result<Value, ServiceError> {
        match corrections.get(name) {
            None => Ok(fact["value"].clone()),
            Some(Value::String(text)) if name == "amount_cents" => Ok(json!(
                yuan_to_cents(text).ok_or_else(|| ServiceError::Invalid("amount".into()))?
            )),
            Some(Value::String(text)) => Ok(json!(text.trim())),
            Some(Value::Number(n)) if name == "nights" => Ok(json!(n)),
            Some(_) => Err(ServiceError::Invalid(format!("invalid value for {name}"))),
        }
    };
    let object = invoice
        .as_object_mut()
        .ok_or_else(|| ServiceError::Invalid("reading".into()))?;
    for (name, fact) in object.iter_mut() {
        if fact.get("level").and_then(Value::as_str) == Some("candidate") {
            *fact = confirm(fact, corrected(name, fact)?);
        }
    }
    if let Some(period) = object
        .get_mut("service_period")
        .and_then(Value::as_object_mut)
    {
        for (name, fact) in period.iter_mut() {
            if fact.get("level").and_then(Value::as_str) == Some("candidate") {
                *fact = confirm(fact, corrected(name, fact)?);
            }
        }
    }
    for name in corrections.keys() {
        if !object.contains_key(name)
            && !matches!(name.as_str(), "check_in" | "check_out" | "nights")
        {
            return Err(ServiceError::Invalid(format!("unknown field {name}")));
        }
    }
    Ok(invoice)
}

fn confirmed_ids(invoice: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut collect = |value: &Value| {
        if value.get("level").and_then(Value::as_str) == Some("confirmed")
            && let Some(id) = value["id"].as_str()
        {
            ids.push(id.to_string());
        }
    };
    if let Some(object) = invoice.as_object() {
        object.values().for_each(&mut collect);
        if let Some(period) = object.get("service_period").and_then(Value::as_object) {
            period.values().for_each(&mut collect);
        }
    }
    ids.sort();
    ids
}

/// The frozen snapshot: accepted items with their link facts and every decision bound to them, fully spelled out.
pub fn build_snapshot(
    batch: &Batch,
    reading: &Reading,
    assessment: &Assessment,
    decisions: &[Value],
) -> Result<Value, ServiceError> {
    let links: BTreeMap<&str, &Value> = assessment
        .links
        .iter()
        .filter_map(|link| Some((link["item_id"].as_str()?, link)))
        .collect();
    if assessment.report.needs_decision.count > 0 {
        return Err(ServiceError::Invalid("items still need a decision".into()));
    }
    let mut items = Vec::new();
    let mut used = BTreeSet::new();
    for item in reading.items.iter().filter(|item| !item.rejected) {
        let Some(link) = links.get(item.id.as_str()) else {
            continue;
        };
        if link["disposition"] != "accepted" {
            continue;
        }
        // A reading stored without a suggestion (A3 unavailable) falls back to the rule name from the seller.
        let seller = item
            .invoice
            .as_ref()
            .and_then(|invoice| invoice["seller_name"]["value"].as_str())
            .unwrap_or_default();
        let short_name = item
            .short_name
            .clone()
            .or_else(|| reconcile::rule_short_name(seller))
            .ok_or_else(|| ServiceError::Invalid(format!("{} needs a short name", item.file)))?;
        let category = link["category"]
            .as_str()
            .ok_or_else(|| ServiceError::Invalid(format!("{} needs a category", item.file)))?;
        let own: Vec<&Value> = decisions
            .iter()
            .filter(|d| d["item_id"] == item.id.as_str() && d["kind"] != "reject")
            .collect();
        let replaces = own
            .iter()
            .find(|d| d["kind"] == "replace_unpaid_invoice")
            .and_then(|d| d["payload"]["invoice_no"].as_str().map(String::from));
        used.extend(
            own.iter()
                .filter_map(|d| d["id"].as_str().map(String::from)),
        );
        items.push(json!({
            "id": item.id, "invoice": item.invoice, "service_date": link["service_date"], "category": category,
            "short_name": short_name, "expense_detail": format!("{category}：{short_name}"),
            "decision_ids": own.iter().filter_map(|d| d["id"].as_str()).collect::<Vec<_>>(),
            "replaces_invoice_no": replaces,
        }));
    }
    if items.is_empty() {
        return Err(ServiceError::Invalid(
            "no accepted invoices to package".into(),
        ));
    }
    let decisions: Vec<&Value> = decisions
        .iter()
        .filter(|d| d["id"].as_str().is_some_and(|id| used.contains(id)))
        .collect();
    Ok(
        json!({"batch_id": batch.id, "revision": batch.revision, "applicant": applicant_name(&batch.applicant),
              "period": batch.period, "policy_hash": assessment.policy_hash, "history_hash": assessment.history_hash,
              "items": items, "decisions": decisions}),
    )
}

/// The ledger shows a person's name, never a Matrix id; the demo accounts map to the fictional names.
fn applicant_name(mxid: &str) -> String {
    let local = mxid
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or("applicant");
    match local {
        "reimb-linyi" => "林一".into(),
        "reimb-zhoumin" => "周敏".into(),
        other => other
            .chars()
            .filter(|c| c.is_alphabetic())
            .take(20)
            .collect::<String>(),
    }
}

/// Atomic publish (spec §快照 step 3-4): rename staging into published/<revision>, then point CURRENT at it.
pub fn publish(batch_dir: &Path, hash: &str, revision: i64) -> std::io::Result<()> {
    let staging = batch_dir.join("staging").join(hash);
    let published = batch_dir.join("published");
    std::fs::create_dir_all(&published)?;
    let target = published.join(revision.to_string());
    if target.exists() {
        // A rerun after a crash re-stages identical bytes; anything else must not replace what was published.
        if staging.exists()
            && std::fs::read(target.join("manifest.json"))?
                != std::fs::read(staging.join("manifest.json"))?
        {
            return Err(std::io::Error::other("published revision differs"));
        }
    } else {
        std::fs::rename(&staging, &target)?;
    }
    let pointer = published.join(".CURRENT.tmp");
    std::fs::write(&pointer, format!("{revision}\n"))?;
    std::fs::rename(&pointer, published.join("CURRENT"))?;
    Ok(())
}
