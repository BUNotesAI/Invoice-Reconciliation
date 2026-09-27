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

#[derive(Deserialize, Clone)]
struct SourceFile {
    id: String,
    detected_type: String,
    original_name: String,
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
#[derive(Deserialize, Default, Clone, Copy)]
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

struct Item {
    id: String,
    file: String,
    invoice: Option<Value>,
    view: Option<InvoiceView>,
    gate: Option<(String, Vec<String>)>,
    category: Option<String>,
    short_name: Option<String>,
}

pub async fn reconcile(
    core: &CoreClient,
    agent: &mut AgentPort,
    batch: &BatchInput,
) -> Result<Report, ReconcileError> {
    let mut rules_steps: Vec<String> = Vec::new();

    // Receive: content-addressed ingest; the same file twice is a no-op.
    let mut sources = Vec::new();
    for path in &batch.uploads {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let ingested: Ingested = typed(
            core.call(
                CoreCommand::Ingest,
                &json!({"source_path": path, "original_name": name}),
            )
            .await?,
            "ingest",
        )?;
        sources.push(ingested.source_file);
    }
    let history: History = typed(
        core.call(
            CoreCommand::History,
            &json!({"action": "validate_import", "entries": batch.history}),
        )
        .await?,
        "history",
    )?;
    let policy: Linked = typed(
        core.call(
            CoreCommand::Link,
            &json!({"items": [], "evidence": [], "history_snapshot": history.validated_snapshot, "decisions": [], "period": batch.period}),
        )
        .await?,
        "policy",
    )?;
    let categories: BTreeSet<String> = policy.policy_categories.iter().cloned().collect();
    let limits = policy.policy_limits;

    // Read: text layers are facts; images go to A1 and stay candidates.
    let mut items: Vec<Item> = Vec::new();
    let mut evidence: Vec<Value> = Vec::new();
    let mut unsupported = Vec::new();
    for source in &sources {
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
                let view = match &invoice {
                    Some(record) => Some(typed::<InvoiceView>(record.clone(), "invoice")?),
                    None => None,
                };
                let id = format!("item-{}", &source.id[..12]);
                items.push(Item {
                    id,
                    file: source.original_name.clone(),
                    invoice,
                    view,
                    gate: None,
                    category: None,
                    short_name: None,
                });
            }
            "wechat_bill" | "didi_trip_pdf" => {
                let set: EvidenceSet = typed(
                    core.call(CoreCommand::Evidence, &json!({"source_file_id": source.id}))
                        .await?,
                    "evidence",
                )?;
                evidence.extend(set.evidence);
            }
            "image" => {
                if let Some(found) = read_screenshot(core, agent, source, &mut rules_steps).await? {
                    evidence.extend(found);
                }
            }
            _ => unsupported.push(source.original_name.clone()),
        }
    }

    // Gates over every reading, candidates included (they come back as needing confirmation).
    let readings: Vec<Value> = items
        .iter()
        .filter_map(|item| item.invoice.clone())
        .collect();
    let gates: Gates = typed(
        core.call(
            CoreCommand::Gates,
            &json!({"invoices": readings, "history_snapshot": history.validated_snapshot}),
        )
        .await?,
        "gates",
    )?;
    let gate_by_invoice: BTreeMap<String, &GateRow> = gates
        .items
        .iter()
        .map(|row| (row.invoice_id.clone(), row))
        .collect();
    for item in &mut items {
        if let Some(view) = &item.view {
            let row = gate_by_invoice
                .get(&view.id)
                .ok_or_else(|| ReconcileError::Contract("gate row missing".into()))?;
            item.gate = Some((row.disposition.clone(), row.review_reasons.clone()));
        }
    }

    // A3: category and short-name suggestions for trusted, not rejected invoices.
    let known_short = &policy.policy_short_names;
    for item in &mut items {
        let (Some(view), Some((disposition, reasons))) = (&item.view, &item.gate) else {
            continue;
        };
        if disposition == "rejected" || reasons.iter().any(|r| r == "FACT_UNCONFIRMED") {
            continue;
        }
        let data = json!({"seller_name": view.seller_name.value, "project": view.project.value,
                          "categories": categories, "known_short_names": known_short});
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
                item.short_name = Some(
                    known_short
                        .get(&view.seller_name.value)
                        .cloned()
                        .unwrap_or(value.short_name),
                );
            }
            Outcome::RulesMode(why) => {
                rules_steps.push(format!("A3: {why}"));
                item.short_name = known_short.get(&view.seller_name.value).cloned();
            }
        }
    }

    // Link and occupancy over the whole batch, then missing-invoice detection.
    let link_items: Vec<Value> = items
        .iter()
        .filter_map(|item| {
            let (disposition, reasons) = item.gate.as_ref()?;
            let mut value = json!({"id": item.id, "invoice": item.invoice, "category": item.category,
                                   "gate": {"disposition": disposition, "review_reasons": reasons}});
            if let Some(day) = declared_date(&item.file) {
                value["declared_service_date"] = json!(day);
            }
            Some(value)
        })
        .collect();
    let link_input = |choices: &BTreeMap<String, String>| {
        json!({"items": link_items, "evidence": evidence, "history_snapshot": history.validated_snapshot,
               "decisions": [], "period": batch.period, "agent_choices": choices})
    };
    let mut linked: Linked = typed(
        core.call(CoreCommand::Link, &link_input(&BTreeMap::new()))
            .await?,
        "link",
    )?;
    // A2 only where several genuine payments fit; the core re-checks each choice before using it.
    let choices = rank(agent, &linked, &evidence, &mut rules_steps).await;
    if !choices.is_empty() {
        linked = typed(
            core.call(CoreCommand::Link, &link_input(&choices)).await?,
            "link",
        )?;
    }
    let missing: Missing = typed(
        core.call(
            CoreCommand::Missing,
            &json!({"evidence": evidence, "occupancy": linked.occupancy, "ignored_transactions": [],
                    "period": batch.period, "history_snapshot": history.validated_snapshot}),
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
    for item in &items {
        let row = rows.get(&item.id);
        let (disposition, reasons, notes) = match (row, &item.view) {
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
            seller: item.view.as_ref().map(|v| v.seller_name.value.clone()),
            short_name: item.short_name.clone(),
            amount_cents: item.view.as_ref().map(|v| v.amount_cents.value),
            issue_date: item.view.as_ref().map(|v| v.issue_date.value.clone()),
            service_date: row.and_then(|r| r.service_date.clone()),
            category: row.and_then(|r| r.category.clone()),
            nights: item
                .view
                .as_ref()
                .and_then(|v| v.service_period.as_ref().map(|s| s.nights.value)),
            check_in: item
                .view
                .as_ref()
                .and_then(|v| v.service_period.as_ref().map(|s| s.check_in.value.clone())),
            disposition,
            reasons,
            notes,
            explanation: None,
        });
    }

    // A4: explanations only for items that need the applicant; facts are code-built and cited by id.
    explain(agent, &mut views, limits, &mut rules_steps).await;

    let mut summary = linked.summary;
    let unread = views
        .iter()
        .filter(|view| view.amount_cents.is_none())
        .count() as u32;
    summary.entry("needs_decision".into()).or_default().count += unread;
    Ok(report::build(report::Input {
        period: batch.period.clone(),
        applicant: batch.applicant.clone(),
        agent_available: agent.is_available(),
        rules_steps,
        summary,
        items: views,
        missing: missing.candidates,
        not_included: missing.not_included.len() as u32,
        ignored_merchants: missing.ignored_merchants,
        unsupported,
    }))
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
) -> Result<Option<Vec<Value>>, ReconcileError> {
    let probe: Extracted = typed(
        core.call(CoreCommand::Evidence, &json!({"source_file_id": source.id}))
            .await?,
        "evidence",
    )?;
    let Some(image) = probe.vision_image else {
        rules_steps.push("A1 screenshot: no image for the vision step".into());
        return Ok(None);
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
            return Ok(None);
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
        Ok(result) => Ok(Some(typed::<EvidenceSet>(result, "evidence")?.evidence)),
        Err(CoreFailure::Rejected { code, .. }) if code == "FIELD_CONFLICT" => {
            rules_steps.push("A1 screenshot: reading failed format checks".into());
            Ok(None)
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
