//! Script steps 1-11 through the orchestration service: real core subprocess, real SQLite, scripted model.
mod common;

use std::sync::atomic::Ordering;

use common::{Harness, INTRUDER, LINYI, ROOM, ZHOUMIN, item_id, repo, texts, to_needs_decision};
use reimb_bot::{
    service::{Service, ServiceError},
    store::State,
};
use serde_json::{Map, Value, json};

/// Decisions a careful applicant makes for F06, F08, F09 and F10, all bound to the current revision.
async fn decide_everything(service: &Service, batch: &str) {
    let rev = |service: &Service| service.batch(batch).unwrap().revision;
    let f09 = item_id(service, batch, "F09.pdf");
    service
        .confirm_visual(batch, LINYI, rev(service), &f09, &Map::new())
        .await
        .unwrap();
    let reading = service
        .with_store(|store| store.document(batch, "reading"))
        .unwrap()
        .unwrap();
    let shot = reading["screenshots"][0]["source_id"]
        .as_str()
        .unwrap()
        .to_string();
    service
        .confirm_screenshot(batch, LINYI, rev(service), &shot, &Map::new())
        .await
        .unwrap();
    for file in ["F08.pdf", "F09.pdf"] {
        let id = item_id(service, batch, file);
        service
            .decide(
                batch,
                LINYI,
                rev(service),
                &id,
                "explain_over_limit",
                json!({"explanation": "客户接待与会展期间"}),
            )
            .await
            .unwrap();
    }
    let f10 = item_id(service, batch, "F10.pdf");
    service
        .decide(
            batch,
            LINYI,
            rev(service),
            &f10,
            "replace_unpaid_invoice",
            json!({"invoice_no": "26112000000300002208"}),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn month_end_reminder_is_sent_once() {
    let harness = Harness::new();
    let service = harness.service();
    harness
        .clock
        .store(common::MONTH_END - 3600, Ordering::SeqCst);
    assert_eq!(
        service.remind().unwrap(),
        0,
        "08:00 on the last day is too early"
    );
    harness.clock.store(common::MONTH_END, Ordering::SeqCst);
    assert_eq!(service.remind().unwrap(), 1);
    service.remind().unwrap();
    assert_eq!(
        service
            .with_store(|store| store.pending_outbox())
            .unwrap()
            .len(),
        1,
        "the same reminder is keyed once"
    );
}

#[tokio::test]
async fn files_report_and_permissions() {
    let harness = Harness::new();
    let (service, batch) = to_needs_decision(&harness).await;
    let state = service.batch(&batch).unwrap();
    assert_eq!(state.state, State::NeedsDecision);
    let report = service.assessment(&batch).unwrap().unwrap().report;
    let want = common::expected("link.json")["demo_first_pass"]["summary"].clone();
    assert_eq!(
        (report.automatic.count, report.automatic.cents),
        (6, want["automatic"]["cents"].as_i64().unwrap())
    );
    assert_eq!((report.needs_decision.count, report.rejected.count), (4, 2));
    let posted = texts(&service, &batch);
    assert!(
        posted
            .iter()
            .any(|t| t.contains("待你判断 4 张，合计 ¥3,426.00"))
    );
    assert!(
        posted
            .iter()
            .any(|t| t.starts_with("[Mini app] 报销对账台"))
    );

    // The same file again changes nothing; the same event twice is handled once.
    service
        .receive_file(
            LINYI,
            ROOM,
            "$again",
            "F01.pdf",
            &std::fs::read(repo().join("fixtures/demo/F01.pdf")).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        texts(&service, &batch)
            .iter()
            .any(|t| t.contains("新文件没有收进来"))
    );
    let before = service
        .with_store(|store| store.outbox(&batch))
        .unwrap()
        .len();
    service
        .receive_text(LINYI, ROOM, "$start", "开始对账")
        .await
        .unwrap();
    assert_eq!(
        service
            .with_store(|store| store.outbox(&batch))
            .unwrap()
            .len(),
        before
    );

    // Only the applicant changes the batch, and only at the current revision.
    let f08 = item_id(&service, &batch, "F08.pdf");
    let explain = json!({"explanation": "会展"});
    let revision = state.revision;
    assert!(matches!(
        service
            .decide(
                &batch,
                INTRUDER,
                revision,
                &f08,
                "explain_over_limit",
                explain.clone()
            )
            .await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service
            .decide(
                &batch,
                ZHOUMIN,
                revision,
                &f08,
                "explain_over_limit",
                explain.clone()
            )
            .await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service
            .decide(
                &batch,
                LINYI,
                revision - 1,
                &f08,
                "explain_over_limit",
                explain.clone()
            )
            .await,
        Err(ServiceError::StaleRevision(_))
    ));
    assert!(matches!(
        service
            .decide(
                &batch,
                LINYI,
                revision,
                &f08,
                "replace_unpaid_invoice",
                json!({"invoice_no": "1"})
            )
            .await,
        Err(ServiceError::Invalid(_))
    ));
    assert!(
        matches!(
            service.confirm(&batch, LINYI, revision).await,
            Err(ServiceError::Invalid(_))
        ),
        "pending items block confirmation"
    );
    service
        .decide(&batch, LINYI, revision, &f08, "explain_over_limit", explain)
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().revision, revision + 1);

    // Finance may view, never change; a stranger may do neither.
    let fresh = service.batch(&batch).unwrap();
    assert!(
        service.may_view(&fresh, ZHOUMIN).unwrap() && !service.may_view(&fresh, INTRUDER).unwrap()
    );
}

#[tokio::test]
async fn strangers_cannot_open_batches() {
    let harness = Harness::new();
    let service = harness.service();
    service
        .receive_file(INTRUDER, "!x:reimb.local", "$x", "F01.pdf", b"%PDF-1.4")
        .await
        .unwrap();
    assert!(
        service
            .with_store(|store| store.open_batch_for(INTRUDER))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn decisions_confirmation_and_publication() {
    let harness = Harness::new();
    let (service, batch) = to_needs_decision(&harness).await;
    decide_everything(&service, &batch).await;
    let state = service.batch(&batch).unwrap();
    assert_eq!(
        state.state,
        State::AwaitingConfirm,
        "{:?}",
        service.assessment(&batch).unwrap().unwrap().report.text
    );
    assert!(
        texts(&service, &batch)
            .iter()
            .any(|t| t.contains("所有待判断项都处理完了"))
    );
    assert!(matches!(
        service.confirm(&batch, LINYI, state.revision - 1).await,
        Err(ServiceError::StaleRevision(_))
    ));
    service
        .confirm(&batch, LINYI, state.revision)
        .await
        .unwrap();
    let done = service.batch(&batch).unwrap();
    assert_eq!(done.state, State::ReadyToShare);
    let posted = texts(&service, &batch);
    let result = posted.iter().find(|t| t.contains("终审全部通过")).unwrap();
    assert!(
        result.contains("（7/7）") && result.contains("10 张发票") && result.contains("¥4,901.40"),
        "{result}"
    );
    let published = harness.root.join("batches").join(&batch).join("published");
    assert_eq!(
        std::fs::read_to_string(published.join("CURRENT"))
            .unwrap()
            .trim(),
        done.revision.to_string()
    );
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(
            published
                .join(done.revision.to_string())
                .join("manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["total_cents"], 490140);
    let audit: Vec<String> = service
        .with_store(|store| store.audit(&batch))
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect();
    assert!(
        audit.ends_with(&["confirm".to_string(), "verify_passed".to_string()]),
        "{audit:?}"
    );
}

#[tokio::test]
async fn crash_after_freeze_or_after_publish_ends_like_no_crash() {
    // Reference run without a crash.
    let reference = Harness::new();
    let (service, batch) = to_needs_decision(&reference).await;
    decide_everything(&service, &batch).await;
    service
        .confirm(&batch, LINYI, service.batch(&batch).unwrap().revision)
        .await
        .unwrap();
    let revision = service.batch(&batch).unwrap().revision;
    let expected = std::fs::read(
        reference
            .root
            .join("batches")
            .join(&batch)
            .join("published")
            .join(revision.to_string())
            .join("manifest.json"),
    )
    .unwrap();

    for crash_after_publish in [false, true] {
        let harness = Harness::new();
        let (service, batch) = to_needs_decision(&harness).await;
        decide_everything(&service, &batch).await;
        let hash = service
            .freeze(&batch, LINYI, service.batch(&batch).unwrap().revision)
            .await
            .unwrap();
        if crash_after_publish {
            // The artefacts reached their final place, but the process died before recording it.
            service
                .execute_without_commit_for_test(&batch)
                .await
                .unwrap();
        }
        drop(service);
        let restarted = harness.service();
        assert_eq!(restarted.batch(&batch).unwrap().state, State::Executing);
        restarted.recover().await.unwrap();
        let finished = restarted.batch(&batch).unwrap();
        assert_eq!(
            (finished.state, finished.snapshot_hash.as_deref()),
            (State::ReadyToShare, Some(hash.as_str()))
        );
        let published = harness
            .root
            .join("batches")
            .join(&batch)
            .join("published")
            .join(finished.revision.to_string());
        let manifest = std::fs::read(published.join("manifest.json")).unwrap();
        // Same snapshot, same bytes: only the batch identity differs between the two runs.
        let normalise = |bytes: &[u8]| -> Value {
            let mut value: Value = serde_json::from_slice(bytes).unwrap();
            value["batch_id"] = json!("x");
            value["snapshot_hash"] = json!("x");
            value
        };
        assert_eq!(normalise(&manifest)["rows"], normalise(&expected)["rows"]);
        let posts = texts(&restarted, &batch)
            .into_iter()
            .filter(|t| t.contains("终审全部通过"))
            .count();
        assert_eq!(posts, 1, "the result is posted once");
    }
}

#[tokio::test]
async fn upload_during_processing_is_refused_and_reopen_keeps_decisions() {
    let harness = Harness::new();
    let (service, batch) = to_needs_decision(&harness).await;
    let f08 = item_id(&service, &batch, "F08.pdf");
    let revision = service.batch(&batch).unwrap().revision;
    service
        .decide(
            &batch,
            LINYI,
            revision,
            &f08,
            "explain_over_limit",
            json!({"explanation": "会展"}),
        )
        .await
        .unwrap();
    service
        .receive_text(LINYI, ROOM, "$reopen", "重开收件")
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::Collecting);
    assert_eq!(
        service
            .with_store(|store| store.decisions(&batch))
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn confirmation_works_when_the_model_cannot_classify() {
    // A3 down: rule categories and rule short names still let the applicant confirm.
    let harness = Harness::new();
    let service = harness.service_with(common::scripted(|request| match request.task {
        reimb_bot::agent::AgentTask::Classify => {
            Err(reimb_bot::agent::AgentError::Unavailable("down".into()))
        }
        _ => common::honest(request),
    }));
    common::upload_demo(&service).await;
    service
        .receive_text(LINYI, ROOM, "$start", "开始对账")
        .await
        .unwrap();
    let batch = service
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap()
        .id;
    decide_everything(&service, &batch).await;
    let revision = service.batch(&batch).unwrap().revision;
    service.confirm(&batch, LINYI, revision).await.unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::ReadyToShare);
    let report = service.assessment(&batch).unwrap().unwrap().report;
    assert!(report.rules_mode);
}
