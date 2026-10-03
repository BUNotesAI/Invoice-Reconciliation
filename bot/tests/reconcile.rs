//! End-to-end first pass: real core subprocess, scripted agent answers, hand-written expected counts.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use reimb_bot::{
    agent::{AgentError, AgentPort, AgentRequest, AgentTask, ReplayAdapter},
    core_client::CoreClient,
    reconcile::{BatchInput, reconcile},
    report::Report,
};
use serde_json::{Value, json};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn expected() -> Value {
    serde_json::from_slice(&std::fs::read(repo().join("fixtures/expected/link.json")).unwrap())
        .unwrap()
}

struct Batch {
    _dir: tempfile::TempDir,
    core: CoreClient,
    input: BatchInput,
}

fn batch() -> Batch {
    batch_from("fixtures/demo")
}

fn batch_from(folder: &str) -> Batch {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("data");
    let uploads = root.join("uploads");
    std::fs::create_dir_all(&uploads).unwrap();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(repo().join(folder)).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() != "history.json" {
            let target = uploads.join(path.file_name().unwrap());
            std::fs::copy(&path, &target).unwrap();
            files.push(target);
        }
    }
    files.sort();
    let policy = root.join("policy.yaml");
    std::fs::copy(repo().join("policy/example.yaml"), &policy).unwrap();
    let core = CoreClient::new(
        repo().join(".venv/bin/python"),
        repo().join("core"),
        root.clone(),
        root.join("batch-2026-10"),
        policy,
    );
    let history =
        serde_json::from_slice(&std::fs::read(repo().join("fixtures/demo/history.json")).unwrap())
            .unwrap();
    let input = BatchInput {
        uploads: files,
        history,
        period: "2026-10".into(),
        applicant: "林一".into(),
    };
    Batch {
        _dir: dir,
        core,
        input,
    }
}

fn category(seller: &str) -> (&'static str, &'static str) {
    match seller {
        s if s.contains("瑞幸") => ("餐饮", "瑞幸"),
        s if s.contains("滴滴") => ("打车", "滴滴"),
        s if s.contains("航空") => ("机票", "国航"),
        s if s.contains("酒店") => ("住宿", "燕园会展酒店"),
        _ => ("餐饮", "某店"),
    }
}

/// A well-behaved model: correct readings, in-policy categories, explanations citing real facts.
fn honest(request: &AgentRequest) -> Result<String, AgentError> {
    Ok(match request.task {
        AgentTask::ReadInvoice => json!({"invoice_no": "26442000000500010191", "issue_date": "2026-10-19", "amount_cents": "386.00",
            "amount_upper": "叁佰捌拾陆圆整", "buyer_name": "示例科技有限公司", "buyer_tax_id": "91440300XXXXXXXX0A",
            "seller_name": "深圳潮海居酒楼有限公司", "project": "*餐饮服务*餐费", "remark": ""})
        .to_string(),
        AgentTask::ReadScreenshot => {
            json!({"merchant": "滴滴出行", "amount": "20.00", "service_date": "2026-10-17", "order_ref": "DD-20261017-6630"})
                .to_string()
        }
        AgentTask::Classify => {
            let (category, short) = category(request.data["seller_name"].as_str().unwrap());
            json!({"category": category, "short_name": short, "rationale": "按销售方与项目判断。"}).to_string()
        }
        AgentTask::Rank => {
            let first = request.data["candidates"][0]["id"].as_str().unwrap();
            json!({"choice": first, "reason": "日期最接近开票日。"}).to_string()
        }
        AgentTask::Explain => {
            let items: Vec<Value> = request.data["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    let id = item["item_id"].as_str().unwrap();
                    let amount = format!("{id}.amount");
                    match item["facts"].get(&amount).and_then(Value::as_str) {
                        Some(yuan) => json!({"item_id": id, "fact_refs": [amount], "explanation": format!("这张票 {yuan} 元，需要你看一下。")}),
                        None => json!({"item_id": id, "fact_refs": [], "explanation": "这张票需要你看一下。"}),
                    }
                })
                .collect();
            json!({"items": items}).to_string()
        }
    })
}

fn scripted(
    mut answer: impl FnMut(&AgentRequest) -> Result<String, AgentError> + Send + 'static,
) -> AgentPort {
    AgentPort::Scripted(Box::new(move |request| answer(request)))
}

fn counts(report: &Report) -> [(u32, i64); 3] {
    [
        (report.automatic.count, report.automatic.cents),
        (report.needs_decision.count, report.needs_decision.cents),
        (report.rejected.count, report.rejected.cents),
    ]
}

fn expected_counts() -> [(u32, i64); 3] {
    let summary = &expected()["demo_first_pass"]["summary"];
    let bucket = |key: &str| {
        (
            summary[key]["count"].as_u64().unwrap() as u32,
            summary[key]["cents"].as_i64().unwrap(),
        )
    };
    [
        bucket("automatic"),
        bucket("needs_decision"),
        bucket("rejected"),
    ]
}

#[tokio::test]
async fn first_pass_report_is_six_four_two_with_two_missing() {
    let batch = batch();
    let mut agent = scripted(honest);
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    assert_eq!(counts(&report), expected_counts());
    let want = &expected()["demo_first_pass"];
    let merchants: Vec<&str> = report
        .missing_candidates
        .iter()
        .map(|row| row.merchant.as_str())
        .collect();
    let expected_merchants: Vec<&str> = want["missing_candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["merchant"].as_str().unwrap())
        .collect();
    assert_eq!(merchants, expected_merchants);
    assert_eq!(
        report.not_included_count as u64,
        want["not_included_count"].as_u64().unwrap()
    );
    assert!(!report.rules_mode, "{:?}", report.rules_steps);
    let pending: Vec<&str> = report
        .items
        .iter()
        .filter(|i| i.disposition == "needs_decision")
        .map(|i| i.file.as_str())
        .collect();
    assert_eq!(pending, ["F06.pdf", "F08.pdf", "F09.pdf", "F10.pdf"]);
    assert!(
        report
            .items
            .iter()
            .filter(|i| i.disposition == "needs_decision")
            .all(|i| i.explanation.is_some())
    );
    assert!(
        report.text.contains("待你判断 4 张，合计 ¥3,426.00")
            && report.text.contains("Agent 说明：")
    );
    assert!(!report.text.contains("通过"));
}

#[tokio::test]
async fn without_an_agent_the_batch_runs_in_rules_mode() {
    let batch = batch();
    let mut agent = AgentPort::Unavailable;
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    let [automatic, needs, rejected] = expected_counts();
    // The image-only invoice cannot be read, so its amount is unknown; the count is unchanged.
    assert_eq!(
        counts(&report),
        [automatic, (needs.0, needs.1 - 38600), rejected]
    );
    assert!(report.rules_mode && report.text.contains("规则模式"));
    assert!(report.items.iter().all(|item| item.explanation.is_none()));
    // The unread invoice's payment cannot be reserved; the card says so instead of hiding it.
    assert_eq!(report.missing_candidates.len(), 3);
    assert!(report.text.contains("有 1 张图片票未读出"));
}

#[tokio::test]
async fn one_invalid_explanation_is_repaired() {
    let batch = batch();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    let mut agent = scripted(move |request| {
        log.lock()
            .unwrap()
            .push((request.task, request.repair.is_some()));
        if request.task == AgentTask::Explain && request.repair.is_none() {
            // Fabricated amount and a status claim.
            let first = request.data["items"][0]["item_id"]
                .as_str()
                .unwrap()
                .to_string();
            return Ok(json!({"items": [{"item_id": first, "fact_refs": [], "explanation": "金额 999 元，终审已通过。"}]}).to_string());
        }
        honest(request)
    });
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    assert!(!report.rules_mode, "{:?}", report.rules_steps);
    let explains: Vec<bool> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(task, _)| *task == AgentTask::Explain)
        .map(|(_, r)| *r)
        .collect();
    assert_eq!(explains, [false, true]);
    assert!(!report.text.contains("999") && !report.text.contains("终审"));
}

#[tokio::test]
async fn two_invalid_answers_fall_back_to_rules_for_that_step() {
    let batch = batch();
    let mut agent = scripted(|request| match request.task {
        AgentTask::Explain => Ok("好的，我来解释：F08 超标 60 元".into()),
        _ => honest(request),
    });
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    assert_eq!(counts(&report), expected_counts());
    assert!(report.rules_mode && report.rules_steps.iter().any(|step| step.starts_with("A4")));
    assert!(report.items.iter().all(|item| item.explanation.is_none()));
    assert!(!report.text.contains("60 元"));
}

#[tokio::test]
async fn category_outside_policy_is_never_applied() {
    let batch = batch();
    let mut agent = scripted(|request| match request.task {
        AgentTask::Classify => {
            Ok(json!({"category": "娱乐", "short_name": "x", "rationale": "猜的。"}).to_string())
        }
        _ => honest(request),
    });
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    // Rule categories take over; the counts do not move.
    assert_eq!(counts(&report), expected_counts());
    assert!(
        report
            .items
            .iter()
            .all(|item| item.category.as_deref() != Some("娱乐"))
    );
    assert!(report.rules_steps.iter().any(|step| step.starts_with("A3")));
}

#[tokio::test]
async fn a_misread_image_stays_a_candidate() {
    let batch = batch();
    let mut agent = scripted(|request| {
        match request.task {
        AgentTask::ReadInvoice => Ok(json!({"invoice_no": "26442000000500010191", "issue_date": "2026-10-19", "amount_cents": "38.60",
            "amount_upper": "叁拾捌圆陆角", "buyer_name": "示例科技有限公司", "buyer_tax_id": "91440300XXXXXXXX0A",
            "seller_name": "深圳潮海居酒楼有限公司", "project": "*餐饮服务*餐费", "remark": ""})
        .to_string()),
        _ => honest(request),
    }
    });
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    let f09 = report
        .items
        .iter()
        .find(|item| item.file == "F09.pdf")
        .unwrap();
    assert_eq!(
        (f09.disposition.as_str(), f09.reasons.as_slice()),
        (
            "needs_decision",
            ["FACT_UNCONFIRMED".to_string()].as_slice()
        )
    );
    assert_eq!(report.automatic.count, expected_counts()[0].0);
}

#[tokio::test]
async fn recorded_real_answers_replay_without_rules_mode() {
    // Answers recorded from one real octos run (A1 invoice and screenshot, A3, A4); keys bind them to the requests.
    let batch = batch();
    let mut agent =
        AgentPort::Replay(ReplayAdapter::load(&repo().join("fixtures/agent-replay/demo")).unwrap());
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    assert_eq!(counts(&report), expected_counts());
    assert!(!report.rules_mode, "{:?}", report.rules_steps);
    assert_eq!(report.missing_candidates.len(), 2);
    assert!(
        report
            .items
            .iter()
            .filter(|i| i.disposition == "needs_decision")
            .all(|i| i.explanation.is_some())
    );
}

#[tokio::test]
async fn ranking_resolves_several_candidates_only_with_a_valid_choice() {
    let file = |report: &Report, name: &str| {
        report
            .items
            .iter()
            .find(|item| item.file == name)
            .cloned()
            .unwrap()
    };
    let batch = batch_from("fixtures/edge/link");
    let mut agent = scripted(honest);
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    let r10 = file(&report, "R10.pdf");
    assert_eq!(
        (r10.disposition.as_str(), r10.notes.as_slice()),
        ("accepted", ["AGENT_RANKED".to_string()].as_slice())
    );
    // The file-name date is the user's own declaration and conflicts with the trip list.
    assert_eq!(
        file(&report, "R06_2026-10-12.pdf").reasons,
        ["DATE_CONFLICT"]
    );

    let batch = batch_from("fixtures/edge/link");
    let mut agent = scripted(|request| match request.task {
        AgentTask::Rank => Ok(json!({"choice": "ev-not-offered", "reason": "猜的。"}).to_string()),
        _ => honest(request),
    });
    let report = reconcile(&batch.core, &mut agent, &batch.input)
        .await
        .unwrap();
    assert_eq!(file(&report, "R10.pdf").reasons, ["MULTIPLE_CANDIDATES"]);
    assert!(report.rules_steps.iter().any(|step| step.starts_with("A2")));
}
