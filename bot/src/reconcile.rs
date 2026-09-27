//! First reconciliation pass of a sealed batch: core facts, agent suggestions checked by code, one report.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agent::{AgentPort, AgentRequest, AgentTask},
    core_client::{CoreClient, CoreCommand, CoreFailure},
    report::{self, ItemView, Report},
    validate::{self, FactValue, Violations},
};

pub struct BatchInput {
    pub uploads: Vec<PathBuf>,
    pub history: Value,
    pub period: String,
    pub applicant: String,
}

#[derive(Debug)]
pub enum ReconcileError {
    Core(CoreFailure),
    Contract(String),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core(failure) => write!(f, "{failure}"),
            Self::Contract(what) => write!(f, "unexpected core result: {what}"),
        }
    }
}

impl std::error::Error for ReconcileError {}

impl From<CoreFailure> for ReconcileError {
    fn from(failure: CoreFailure) -> Self {
        Self::Core(failure)
    }
}

fn typed<T: for<'de> Deserialize<'de>>(value: Value, what: &str) -> Result<T, ReconcileError> {
    serde_json::from_value(value)
        .map_err(|error| ReconcileError::Contract(format!("{what}: {error}")))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SourceFile {
    pub id: String,
    pub detected_type: String,
    pub original_name: String,
}

#[derive(Deserialize)]
struct Ingested {
    source_file: SourceFile,
}

#[derive(Deserialize)]
struct Extracted {
    invoice: Option<Value>,
    #[serde(default)]
    vision_image: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Fact<T> {
    value: T,
}

/// The parts of a core invoice record the orchestrator reads; the record itself travels back to the core unchanged.
#[derive(Deserialize)]
struct InvoiceView {
    id: String,
    amount_cents: Fact<i64>,
    issue_date: Fact<String>,
    seller_name: Fact<String>,
    project: Fact<String>,
    service_period: Option<Stay>,
}

#[derive(Deserialize)]
struct Stay {
    check_in: Fact<String>,
    nights: Fact<i64>,
}

#[derive(Deserialize)]
struct GateRow {
    invoice_id: String,
    disposition: String,
    review_reasons: Vec<String>,
    #[serde(default)]
    replacement_candidates: Vec<Value>,
}

#[derive(Deserialize)]
struct Gates {
    items: Vec<GateRow>,
}

#[derive(Deserialize)]
struct History {
    validated_snapshot: Value,
}

#[derive(Deserialize)]
struct EvidenceSet {
    evidence: Vec<Value>,
}

#[derive(Deserialize, Clone)]
struct LinkRow {
    item_id: String,
    #[serde(default)]
    payment_candidates: Vec<String>,
    disposition: String,
    review_reasons: Vec<String>,
    notes: Vec<String>,
    category: Option<String>,
    service_date: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Bucket {
    pub count: u32,
    pub cents: i64,
}

#[derive(Deserialize)]
struct Linked {
    links: Vec<LinkRow>,
    occupancy: Value,
    summary: BTreeMap<String, Bucket>,
    policy_categories: Vec<String>,
    policy_limits: Limits,
    policy_short_names: BTreeMap<String, String>,
    #[serde(default)]
    policy_finance: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct MissingRow {
    pub merchant: String,
    pub amount_cents: i64,
    pub payment_date: String,
    pub likelihood: String,
}

#[derive(Deserialize)]
struct Missing {
    candidates: Vec<MissingRow>,
    not_included: Vec<Value>,
    ignored_merchants: Vec<String>,
}

/// Policy limits the explanation step may cite; read through the core, never parsed here.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub hotel_per_night_cents: i64,
    pub meal_per_day_cents: i64,
    pub local_taxi_per_day_cents: i64,
}

enum Outcome<T> {
    Valid(T),
    RulesMode(String),
}

/// One agent step: ask, validate, one repair with the violation list, then fall back to rules.
async fn ask<T>(
    agent: &mut AgentPort,
    request: AgentRequest,
    check: impl Fn(&str) -> Result<T, Violations>,
) -> Outcome<T> {
    let first = match agent.ask(&request).await {
        Ok(text) => text,
        Err(error) => return Outcome::RulesMode(error.to_string()),
    };
    let violations = match check(&first) {
        Ok(value) => return Outcome::Valid(value),
        Err(violations) => violations,
    };
    let second = match agent.ask(&request.repaired(violations)).await {
        Ok(text) => text,
        Err(error) => return Outcome::RulesMode(error.to_string()),
    };
    match check(&second) {
        Ok(value) => Outcome::Valid(value),
        Err(_) => Outcome::RulesMode("answer violated the contract twice".into()),
    }
}

/// A full ISO date written into the file name is the user's own declaration of the service date.
pub fn declared_date(file_name: &str) -> Option<String> {
    static DATE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?:^|[^0-9])(20[0-9]{2}-[01][0-9]-[0-3][0-9])(?:[^0-9]|$)").unwrap()
    });
    DATE.captures(file_name)
        .map(|capture| capture[1].to_string())
}

/// One invoice file as read: the core record (extracted, candidate or confirmed) and the A3 suggestion.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReadItem {
    pub id: String,
    pub file: String,
    pub source_id: String,
    pub invoice: Option<Value>,
    pub category: Option<String>,
    pub short_name: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
    /// Set by the applicant's reject decision: the item leaves the batch.
    #[serde(default)]
    pub rejected: bool,
}

/// A screenshot whose reading is still a candidate; it becomes evidence only after the user confirms it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScreenshotRead {
    pub source_id: String,
    pub file: String,
    pub fields: Option<Value>,
    pub vision_image: Option<PathBuf>,
}

/// Everything learnt from the sealed files. Stored per batch; decisions re-assess it without re-reading.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Reading {
    pub items: Vec<ReadItem>,
    pub evidence: Vec<Value>,
    pub screenshots: Vec<ScreenshotRead>,
    pub unsupported: Vec<String>,
    pub rules_steps: Vec<String>,
    pub history_snapshot: Value,
    pub categories: Vec<String>,
    pub limits: Limits,
    pub short_names: BTreeMap<String, String>,
    #[serde(default)]
    pub finance: Vec<String>,
    pub agent_available: bool,
}

/// The current judgement of a reading: report card, per-item link rows and the hashes a snapshot binds to.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Assessment {
    pub report: Report,
    pub links: Vec<Value>,
    /// History invoice numbers each item may re-issue, as the gates found them.
    pub replacements: BTreeMap<String, Vec<String>>,
    pub policy_hash: String,
    pub history_hash: String,
}

#[derive(Deserialize)]
struct Hashes {
    policy_hash: String,
    history_hash: String,
}

fn view_of(item: &ReadItem) -> Result<Option<InvoiceView>, ReconcileError> {
    match &item.invoice {
        Some(record) => Ok(Some(typed::<InvoiceView>(record.clone(), "invoice")?)),
        None => Ok(None),
    }
}

pub async fn ingest(
    core: &CoreClient,
    path: &std::path::Path,
    name: &str,
) -> Result<SourceFile, ReconcileError> {
    let ingested: Ingested = typed(
        core.call(
            CoreCommand::Ingest,
            &json!({"source_path": path, "original_name": name}),
        )
        .await?,
        "ingest",
    )?;
    Ok(ingested.source_file)
}

/// Read phase: text layers are facts, images go to A1 and stay candidates, A3 suggests categories.
pub async fn read(
    core: &CoreClient,
    agent: &mut AgentPort,
    sources: &[SourceFile],
    history_entries: &Value,
    period: &str,
) -> Result<Reading, ReconcileError> {
    let mut rules_steps: Vec<String> = Vec::new();
    let history: History = typed(
        core.call(
            CoreCommand::History,
            &json!({"action": "validate_import", "entries": history_entries}),
        )
        .await?,
        "history",
    )?;
    let policy: Linked = typed(
        core.call(
            CoreCommand::Link,
            &json!({"items": [], "evidence": [], "history_snapshot": history.validated_snapshot, "decisions": [], "period": period}),
        )
        .await?,
        "policy",
    )?;
    let mut reading = Reading {
        items: Vec::new(),
        evidence: Vec::new(),
        screenshots: Vec::new(),
        unsupported: Vec::new(),
        rules_steps: Vec::new(),
        history_snapshot: history.validated_snapshot,
        categories: policy.policy_categories,
        limits: policy.policy_limits,
        short_names: policy.policy_short_names,
        finance: policy.policy_finance,
        agent_available: agent.is_available(),
    };
    for source in sources {
        match source.detected_type.as_str() {
            "invoice_pdf" | "image_invoice_pdf" => {
                let extracted: Extracted = typed(
                    core.call(CoreCommand::Extract, &json!({"source_file_id": source.id}))
                        .await?,
                    "extract",
                )?;
                let mut invoice = extracted.invoice;
                if invoice.is_none() {
                    invoice = read_image_invoice(
                        core,
                        agent,
                        source,
                        extracted.vision_image,
                        &mut rules_steps,
                    )
                    .await?;
                }
                reading.items.push(ReadItem {
                    id: format!("item-{}", &source.id[..12]),
                    file: source.original_name.clone(),
                    source_id: source.id.clone(),
                    invoice,
                    category: None,
                    short_name: None,
                    explanation: None,
                    rejected: false,
                });
            }
            "wechat_bill" | "didi_trip_pdf" => {
                let set: EvidenceSet = typed(
                    core.call(CoreCommand::Evidence, &json!({"source_file_id": source.id}))
                        .await?,
                    "evidence",
                )?;
                reading.evidence.extend(set.evidence);
            }
            "image" => {
                let (found, shot) = read_screenshot(core, agent, source, &mut rules_steps).await?;
                reading.evidence.extend(found);
                reading.screenshots.push(shot);
            }
            _ => reading.unsupported.push(source.original_name.clone()),
        }
    }
    classify(core, agent, &mut reading, &mut rules_steps).await?;
    reading.rules_steps = rules_steps;
    Ok(reading)
}

/// A3 for trusted, not rejected invoices that have no suggestion yet (also after a visual confirmation).
pub async fn classify(
    core: &CoreClient,
    agent: &mut AgentPort,
    reading: &mut Reading,
    rules_steps: &mut Vec<String>,
) -> Result<(), ReconcileError> {
    let invoices: Vec<Value> = reading
        .items
        .iter()
        .filter_map(|item| item.invoice.clone())
        .collect();
    let gates: Gates = typed(
        core.call(
            CoreCommand::Gates,
            &json!({"invoices": invoices, "history_snapshot": reading.history_snapshot}),
        )
        .await?,
        "gates",
    )?;
    let gate_by_invoice: BTreeMap<String, &GateRow> = gates
        .items
        .iter()
        .map(|row| (row.invoice_id.clone(), row))
        .collect();
    let categories: BTreeSet<String> = reading.categories.iter().cloned().collect();
    for item in &mut reading.items {
        let Some(view) = view_of(item)? else { continue };
        if item.short_name.is_some() && item.category.is_some() {
            continue;
        }
        let row = gate_by_invoice
            .get(&view.id)
            .ok_or_else(|| ReconcileError::Contract("gate row missing".into()))?;
        if row.disposition == "rejected"
            || row.review_reasons.iter().any(|r| r == "FACT_UNCONFIRMED")
        {
            continue;
        }
        let known = reading.short_names.get(&view.seller_name.value).cloned();
        let data = json!({"seller_name": view.seller_name.value, "project": view.project.value,
                          "categories": categories, "known_short_names": reading.short_names});
        let request = AgentRequest::new(AgentTask::Classify, data, None)
            .map_err(|e| ReconcileError::Contract(e.to_string()))?;
        match ask(agent, request, |text| {
            validate::classification(text, &categories)
        })
        .await
        {
            Outcome::Valid(value) => {
                item.category = Some(value.category);
                // The policy table wins; a new short name is only a suggestion the user confirms before packaging.
                item.short_name = Some(known.unwrap_or(value.short_name));
            }
            Outcome::RulesMode(why) => {
                rules_steps.push(format!("A3: {why}"));
                item.short_name = known;
            }
        }
    }
    Ok(())
}

/// Assess phase: gates, whole-batch linking (A2 where several payments fit), missing invoices, optional A4.
pub async fn assess(
    core: &CoreClient,
    agent: &mut AgentPort,
    reading: &mut Reading,
    decisions: &[Value],
    period: &str,
    applicant: &str,
    explain_items: bool,
) -> Result<Assessment, ReconcileError> {
    let mut rules_steps = reading.rules_steps.clone();
    let invoices: Vec<Value> = reading
        .items
        .iter()
        .filter_map(|item| item.invoice.clone())
        .collect();
    let gates: Gates = typed(
        core.call(
            CoreCommand::Gates,
            &json!({"invoices": invoices, "history_snapshot": reading.history_snapshot}),
        )
        .await?,
        "gates",
    )?;
    let gate_value = core
        .call(
            CoreCommand::Gates,
            &json!({"invoices": [], "history_snapshot": reading.history_snapshot}),
        )
        .await?;
    let hashes: Hashes = typed(gate_value, "gates")?;
    let gate_by_invoice: BTreeMap<String, &GateRow> = gates
        .items
        .iter()
        .map(|row| (row.invoice_id.clone(), row))
        .collect();
    let mut link_items = Vec::new();
    let mut replacements = BTreeMap::new();
    for item in reading.items.iter().filter(|item| !item.rejected) {
        let Some(view) = view_of(item)? else { continue };
        let row = gate_by_invoice
            .get(&view.id)
            .ok_or_else(|| ReconcileError::Contract("gate row missing".into()))?;
        let mut value = json!({"id": item.id, "invoice": item.invoice, "category": item.category,
                               "gate": {"disposition": row.disposition, "review_reasons": row.review_reasons}});
        if let Some(day) = declared_date(&item.file) {
            value["declared_service_date"] = json!(day);
        }
        let numbers: Vec<String> = row
            .replacement_candidates
            .iter()
            .filter_map(|c| c["invoice_no"].as_str().map(String::from))
            .collect();
        if !numbers.is_empty() {
            replacements.insert(item.id.clone(), numbers);
        }
        link_items.push(value);
    }
    let link_input = |choices: &BTreeMap<String, String>| {
        json!({"items": link_items, "evidence": reading.evidence, "history_snapshot": reading.history_snapshot,
               "decisions": decisions, "period": period, "agent_choices": choices})
    };
    let raw = core
        .call(CoreCommand::Link, &link_input(&BTreeMap::new()))
        .await?;
    let mut linked: Linked = typed(raw.clone(), "link")?;
    let mut links = raw["links"].as_array().cloned().unwrap_or_default();
    let choices = rank(agent, &linked, &reading.evidence, &mut rules_steps).await;
    if !choices.is_empty() {
        let raw = core.call(CoreCommand::Link, &link_input(&choices)).await?;
        linked = typed(raw.clone(), "link")?;
        links = raw["links"].as_array().cloned().unwrap_or_default();
    }
    let missing: Missing = typed(
        core.call(
            CoreCommand::Missing,
            &json!({"evidence": reading.evidence, "occupancy": linked.occupancy, "ignored_transactions": [],
                    "period": period, "history_snapshot": reading.history_snapshot}),
        )
        .await?,
        "missing",
    )?;
    let rows: BTreeMap<String, LinkRow> = linked
        .links
        .iter()
        .map(|row| (row.item_id.clone(), row.clone()))
        .collect();
    let mut views = Vec::new();
    for item in reading.items.iter().filter(|item| !item.rejected) {
        let view = view_of(item)?;
        let row = rows.get(&item.id);
        let (disposition, reasons, notes) = match (row, &view) {
            (Some(row), _) => (
                row.disposition.clone(),
                row.review_reasons.clone(),
                row.notes.clone(),
            ),
            (None, None) => (
                "needs_decision".to_string(),
                vec!["VISION_REQUIRED".to_string()],
                vec![],
            ),
            (None, Some(_)) => {
                return Err(ReconcileError::Contract(
                    "item missing from link result".into(),
                ));
            }
        };
        views.push(ItemView {
            item_id: item.id.clone(),
            file: item.file.clone(),
            seller: view.as_ref().map(|v| v.seller_name.value.clone()),
            short_name: item.short_name.clone(),
            amount_cents: view.as_ref().map(|v| v.amount_cents.value),
            issue_date: view.as_ref().map(|v| v.issue_date.value.clone()),
            service_date: row.and_then(|r| r.service_date.clone()),
            category: row.and_then(|r| r.category.clone()),
            nights: view
                .as_ref()
                .and_then(|v| v.service_period.as_ref().map(|s| s.nights.value)),
            check_in: view
                .as_ref()
                .and_then(|v| v.service_period.as_ref().map(|s| s.check_in.value.clone())),
            disposition,
            reasons,
            notes,
            explanation: item.explanation.clone(),
        });
    }
    if explain_items {
        // A4: explanations only for items that need the applicant; facts are code-built and cited by id.
        explain(agent, &mut views, reading.limits, &mut rules_steps).await;
        for view in &views {
            if let Some(item) = reading
                .items
                .iter_mut()
                .find(|item| item.id == view.item_id)
            {
                item.explanation = view.explanation.clone();
            }
        }
    } else {
        // An explanation written for other review reasons would be stale; keep it only while reasons are unchanged.
        for view in &mut views {
            if view.disposition != "needs_decision" {
                view.explanation = None;
            }
        }
    }
    let mut summary = linked.summary;
    let unread = views
        .iter()
        .filter(|view| view.amount_cents.is_none())
        .count() as u32;
    summary.entry("needs_decision".into()).or_default().count += unread;
    let report = report::build(report::Input {
        period: period.to_string(),
        applicant: applicant.to_string(),
        agent_available: reading.agent_available,
        rules_steps,
        summary,
        items: views,
        missing: missing.candidates,
        not_included: missing.not_included.len() as u32,
        ignored_merchants: missing.ignored_merchants,
        unsupported: reading.unsupported.clone(),
    });
    Ok(Assessment {
        report,
        links,
        replacements,
        policy_hash: hashes.policy_hash,
        history_hash: hashes.history_hash,
    })
}

/// Command-line pass: ingest the upload folder, read, and assess once with explanations.
pub async fn reconcile(
    core: &CoreClient,
    agent: &mut AgentPort,
    batch: &BatchInput,
) -> Result<Report, ReconcileError> {
    let mut sources = Vec::new();
    for path in &batch.uploads {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        sources.push(ingest(core, path, &name).await?);
    }
    let mut reading = read(core, agent, &sources, &batch.history, &batch.period).await?;
    Ok(assess(
        core,
        agent,
        &mut reading,
        &[],
        &batch.period,
        &batch.applicant,
        true,
    )
    .await?
    .report)
}

async fn read_image_invoice(
    core: &CoreClient,
    agent: &mut AgentPort,
    source: &SourceFile,
    image: Option<PathBuf>,
    rules_steps: &mut Vec<String>,
) -> Result<Option<Value>, ReconcileError> {
    let Some(image) = image else {
        rules_steps.push("A1: no renderable image for this invoice".into());
        return Ok(None);
    };
    let request = AgentRequest::new(
        AgentTask::ReadInvoice,
        json!({"file": source.original_name}),
        Some(image),
    )
    .map_err(|e| ReconcileError::Contract(e.to_string()))?;
    let reading = match ask(agent, request, validate::invoice_reading).await {
        Outcome::Valid(reading) => reading,
        Outcome::RulesMode(why) => {
            rules_steps.push(format!("A1: {why}"));
            return Ok(None);
        }
    };
    let mut candidate = json!({
        "invoice_no": reading.invoice_no, "issue_date": reading.issue_date, "amount_cents": reading.amount_cents,
        "amount_upper": reading.amount_upper, "buyer_name": reading.buyer_name, "buyer_tax_id": reading.buyer_tax_id,
        "seller_name": reading.seller_name, "project": reading.project, "remark": reading.remark});
    if let Some(order) = reading.order_ref.filter(|value| !value.is_empty()) {
        candidate["order_ref"] = json!(order);
    }
    match core
        .call(
            CoreCommand::Extract,
            &json!({"source_file_id": source.id, "vision_candidate": candidate}),
        )
        .await
    {
        Ok(result) => Ok(typed::<Extracted>(result, "extract")?.invoice),
        // A reading that fails the core's format checks is shown for manual checking, not used.
        Err(CoreFailure::Rejected { code, .. }) if code == "FIELD_CONFLICT" => {
            rules_steps.push("A1: reading failed format checks".into());
            Ok(None)
        }
        Err(failure) => Err(failure.into()),
    }
}

async fn read_screenshot(
    core: &CoreClient,
    agent: &mut AgentPort,
    source: &SourceFile,
    rules_steps: &mut Vec<String>,
) -> Result<(Vec<Value>, ScreenshotRead), ReconcileError> {
    let probe: Extracted = typed(
        core.call(CoreCommand::Evidence, &json!({"source_file_id": source.id}))
            .await?,
        "evidence",
    )?;
    let mut shot = ScreenshotRead {
        source_id: source.id.clone(),
        file: source.original_name.clone(),
        fields: None,
        vision_image: probe.vision_image.clone(),
    };
    let Some(image) = probe.vision_image else {
        rules_steps.push("A1 screenshot: no image for the vision step".into());
        return Ok((Vec::new(), shot));
    };
    let request = AgentRequest::new(
        AgentTask::ReadScreenshot,
        json!({"file": source.original_name}),
        Some(image),
    )
    .map_err(|e| ReconcileError::Contract(e.to_string()))?;
    let fields = match ask(agent, request, validate::screenshot_reading).await {
        Outcome::Valid(fields) => fields,
        Outcome::RulesMode(why) => {
            rules_steps.push(format!("A1 screenshot: {why}"));
            return Ok((Vec::new(), shot));
        }
    };
    let candidate = json!({"merchant": fields.merchant, "amount_cents": fields.amount_cents,
                           "service_date": fields.service_date, "order_ref": fields.order_ref});
    match core
        .call(
            CoreCommand::Evidence,
            &json!({"source_file_id": source.id, "vision_candidate": candidate}),
        )
        .await
    {
        Ok(result) => {
            shot.fields = Some(candidate);
            Ok((typed::<EvidenceSet>(result, "evidence")?.evidence, shot))
        }
        Err(CoreFailure::Rejected { code, .. }) if code == "FIELD_CONFLICT" => {
            rules_steps.push("A1 screenshot: reading failed format checks".into());
            Ok((Vec::new(), shot))
        }
        Err(failure) => Err(failure.into()),
    }
}

async fn rank(
    agent: &mut AgentPort,
    linked: &Linked,
    evidence: &[Value],
    rules_steps: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let by_id: BTreeMap<&str, &Value> = evidence
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect();
    let mut choices = BTreeMap::new();
    for row in linked.links.iter().filter(|row| {
        row.review_reasons
            .iter()
            .any(|r| r == "MULTIPLE_CANDIDATES")
    }) {
        let offered: Vec<Value> = row
            .payment_candidates
            .iter()
            .filter_map(|id| by_id.get(id.as_str()))
            .map(|entry| {
                json!({"id": entry["id"], "merchant": entry["merchant"], "goods": entry["goods"],
                                "payment_date": entry["payment_date"]})
            })
            .collect();
        let ids: BTreeSet<String> = row.payment_candidates.iter().cloned().collect();
        let Ok(request) = AgentRequest::new(
            AgentTask::Rank,
            json!({"item_id": row.item_id, "candidates": offered}),
            None,
        ) else {
            continue;
        };
        match ask(agent, request, |text| validate::ranking(text, &ids)).await {
            Outcome::Valid(ranking) => {
                choices.insert(row.item_id.clone(), ranking.choice);
            }
            Outcome::RulesMode(why) => rules_steps.push(format!("A2: {why}")),
        }
    }
    choices
}

/// Code builds every fact the explanation may cite; the model only chooses which to cite.
pub fn item_facts(view: &ItemView, limits: Limits) -> BTreeMap<String, FactValue> {
    let mut facts = BTreeMap::new();
    let id = &view.item_id;
    if let Some(amount) = view.amount_cents {
        facts.insert(format!("{id}.amount"), FactValue::Amount(amount));
    }
    if let Some(seller) = &view.seller {
        facts.insert(format!("{id}.seller"), FactValue::Text(seller.clone()));
    }
    if let Some(day) = &view.service_date {
        facts.insert(format!("{id}.service_date"), FactValue::Date(day.clone()));
    }
    if let Some(day) = &view.issue_date {
        facts.insert(format!("{id}.issue_date"), FactValue::Date(day.clone()));
    }
    if let Some(nights) = view.nights {
        facts.insert(format!("{id}.nights"), FactValue::Count(nights));
        facts.insert(
            format!("{id}.hotel_limit_per_night"),
            FactValue::Amount(limits.hotel_per_night_cents),
        );
        facts.insert(
            format!("{id}.hotel_limit_total"),
            FactValue::Amount(limits.hotel_per_night_cents * nights),
        );
    }
    if let Some(day) = &view.check_in {
        facts.insert(format!("{id}.check_in"), FactValue::Date(day.clone()));
    }
    match view.category.as_deref() {
        Some("餐饮") => {
            facts.insert(
                format!("{id}.daily_limit"),
                FactValue::Amount(limits.meal_per_day_cents),
            );
        }
        Some("打车") => {
            facts.insert(
                format!("{id}.daily_limit"),
                FactValue::Amount(limits.local_taxi_per_day_cents),
            );
        }
        _ => {}
    }
    facts
}

fn rendered(value: &FactValue) -> Value {
    match value {
        FactValue::Amount(cents) => json!(format!("{}.{:02}", cents / 100, cents % 100)),
        FactValue::Date(day) => json!(day),
        FactValue::Count(count) => json!(count),
        FactValue::Text(text) => json!(text),
    }
}

async fn explain(
    agent: &mut AgentPort,
    views: &mut [ItemView],
    limits: Limits,
    rules_steps: &mut Vec<String>,
) {
    let mut facts = BTreeMap::new();
    let mut allowed = BTreeMap::new();
    let mut requested = Vec::new();
    for view in views
        .iter()
        .filter(|view| view.disposition == "needs_decision")
    {
        let own = item_facts(view, limits);
        allowed.insert(
            view.item_id.clone(),
            own.keys().cloned().collect::<BTreeSet<_>>(),
        );
        requested.push(json!({"item_id": view.item_id, "reasons": view.reasons,
                              "facts": own.iter().map(|(k, v)| (k.clone(), rendered(v))).collect::<BTreeMap<_, _>>()}));
        facts.extend(own);
    }
    if requested.is_empty() {
        return;
    }
    let Ok(request) = AgentRequest::new(AgentTask::Explain, json!({"items": requested}), None)
    else {
        return;
    };
    match ask(agent, request, |text| {
        validate::explanations(text, &allowed, &facts)
    })
    .await
    {
        Outcome::Valid(result) => {
            for entry in result.items {
                if let Some(view) = views.iter_mut().find(|view| view.item_id == entry.item_id) {
                    view.explanation = Some(entry.explanation);
                }
            }
        }
        Outcome::RulesMode(why) => rules_steps.push(format!("A4: {why}")),
    }
}
