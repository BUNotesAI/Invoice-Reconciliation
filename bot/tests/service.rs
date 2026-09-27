//! Script steps 1-11 through the orchestration service: real core subprocess, real SQLite, scripted model.
mod common;

use std::sync::atomic::Ordering;

use common::{Harness, INTRUDER, LINYI, ROOM, ZHOUMIN, item_id, repo, texts, to_needs_decision};
use reimb_bot::{
    service::{Service, ServiceError, Stage},
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
    // One reminder per configured applicant, each in that applicant's room, and only once.
    harness.clock.store(common::MONTH_END, Ordering::SeqCst);
    assert_eq!(service.remind().unwrap(), 2);
    service.remind().unwrap();
    let mut rooms: Vec<String> = service
        .with_store(|store| store.pending_outbox())
        .unwrap()
        .into_iter()
        .map(|message| message.room_id)
        .collect();
    rooms.sort();
    assert_eq!(
        rooms,
        [ROOM, common::SECOND_ROOM],
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

    // The process dies right after freezing, after packaging, or after publishing (before recording the result);
    // the stop is injected inside the production `execute`.
    for stop in [None, Some(Stage::Packaged), Some(Stage::Published)] {
        let harness = Harness::new();
        let (service, batch) = to_needs_decision(&harness).await;
        decide_everything(&service, &batch).await;
        let hash = service
            .freeze(&batch, LINYI, service.batch(&batch).unwrap().revision)
            .await
            .unwrap();
        if let Some(stage) = stop {
            service.set_fault(Some(std::sync::Arc::new(move |at| at == stage)));
            assert!(service.execute(&batch).await.is_err(), "{stage:?}");
            let staged = harness.root.join("batches").join(&batch);
            assert_eq!(
                staged.join("published").join("CURRENT").exists(),
                stage == Stage::Published,
                "{stage:?}"
            );
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

/// Decisions of an item as frozen into the snapshot.
fn snapshot_explanations(service: &Service, batch: &str, item: &str) -> Vec<Value> {
    let snapshot = service
        .with_store(|store| store.document(batch, "snapshot"))
        .unwrap()
        .unwrap();
    snapshot["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["item_id"] == item)
        .map(|d| d["payload"]["explanation"].clone())
        .collect()
}

fn needs_decision(service: &Service, batch: &str, file: &str) -> bool {
    let report = service.assessment(batch).unwrap().unwrap().report;
    report
        .items
        .iter()
        .any(|item| item.file == file && item.disposition == "needs_decision")
}

#[tokio::test]
async fn reopening_takes_earlier_decisions_out_of_force() {
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
            json!({"explanation": "旧说明：会展"}),
        )
        .await
        .unwrap();
    assert!(!needs_decision(&service, &batch, "F08.pdf"));
    // Within one reading an item is answered once: a second answer is refused, so no two stack up.
    let revision = service.batch(&batch).unwrap().revision;
    assert!(matches!(
        service
            .decide(
                &batch,
                LINYI,
                revision,
                &f08,
                "explain_over_limit",
                json!({"explanation": "再说一次"})
            )
            .await,
        Err(ServiceError::Invalid(_))
    ));
    let f09 = item_id(&service, &batch, "F09.pdf");
    service
        .confirm_visual(&batch, LINYI, revision, &f09, &Map::new())
        .await
        .unwrap();
    let revision = service.batch(&batch).unwrap().revision;
    assert!(matches!(
        service
            .confirm_visual(&batch, LINYI, revision, &f09, &Map::new())
            .await,
        Err(ServiceError::Invalid(_))
    ));
    assert_eq!(
        service
            .with_store(|store| store.decisions(&batch))
            .unwrap()
            .len(),
        2
    );
    service
        .receive_text(LINYI, ROOM, "$reopen", "重开收件")
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::Collecting);
    let reply = texts(&service, &batch).pop().unwrap();
    assert!(reply.contains("判断作废"), "{reply}");
    service
        .receive_text(LINYI, ROOM, "$start-again", "开始对账")
        .await
        .unwrap();
    // The new report asks again, and the stored decision is no longer in force: nothing is silently re-applied.
    assert!(needs_decision(&service, &batch, "F08.pdf"));
    assert!(
        service
            .with_store(|store| store.decisions(&batch))
            .unwrap()
            .is_empty()
    );
    // Deciding again (another item first, then F08) gives exactly the new decision in the snapshot.
    decide_everything(&service, &batch).await;
    let revision = service.batch(&batch).unwrap().revision;
    service.confirm(&batch, LINYI, revision).await.unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::ReadyToShare);
    assert_eq!(
        snapshot_explanations(&service, &batch, &f08),
        vec![json!("客户接待与会展期间")]
    );
}

#[tokio::test]
async fn declining_and_reading_again_asks_every_question_again() {
    let harness = Harness::new();
    let (service, batch) = to_needs_decision(&harness).await;
    decide_everything(&service, &batch).await;
    let revision = service.batch(&batch).unwrap().revision;
    service.decline(&batch, LINYI, revision).await.unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::Standby);
    // One more file and a new start: the batch is read again from all its files.
    let extra = std::fs::read(repo().join("fixtures/edge/E01.pdf")).unwrap();
    service
        .receive_file(LINYI, ROOM, "$again", "E01.pdf", &extra)
        .await
        .unwrap();
    service
        .receive_text(LINYI, ROOM, "$restart", "开始对账")
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::NeedsDecision);
    assert!(
        service
            .with_store(|store| store.decisions(&batch))
            .unwrap()
            .is_empty()
    );
    for file in ["F08.pdf", "F09.pdf", "F10.pdf"] {
        assert!(needs_decision(&service, &batch, file), "{file}");
    }
}

#[tokio::test]
async fn a_late_upload_and_a_reopen_make_the_shown_revision_stale() {
    // Design §8.2 must-test: confirmation racing an upload.
    let harness = Harness::new();
    let (service, batch) = to_needs_decision(&harness).await;
    decide_everything(&service, &batch).await;
    let shown = service.batch(&batch).unwrap();
    assert_eq!(shown.state, State::AwaitingConfirm);
    let late = std::fs::read(repo().join("fixtures/demo/F01.pdf")).unwrap();
    service
        .receive_file(LINYI, ROOM, "$late", "F01-late.pdf", &late)
        .await
        .unwrap();
    let after = service.batch(&batch).unwrap();
    assert_eq!(
        (after.state, after.revision),
        (State::AwaitingConfirm, shown.revision)
    );
    assert!(
        texts(&service, &batch)
            .pop()
            .unwrap()
            .contains("新文件没有收进来")
    );
    service
        .receive_text(LINYI, ROOM, "$reopen", "重开收件")
        .await
        .unwrap();
    assert!(matches!(
        service.confirm(&batch, LINYI, shown.revision).await,
        Err(ServiceError::StaleRevision(_))
    ));
    assert_eq!(service.batch(&batch).unwrap().state, State::Collecting);
}

fn replies_to(service: &Service, batch: &str, needle: &str) -> usize {
    texts(service, batch)
        .iter()
        .filter(|t| t.contains(needle))
        .count()
}

#[tokio::test]
async fn an_inbound_event_that_fails_is_answered_once() {
    let harness = Harness::new();
    let broken = harness.service_using(
        common::scripted(common::honest),
        harness.root.join("no-python"),
    );
    let file = std::fs::read(repo().join("fixtures/demo/F01.pdf")).unwrap();
    assert!(
        broken
            .receive_file(LINYI, ROOM, "$f01", "F01.pdf", &file)
            .await
            .is_err()
    );
    assert_eq!(replies_to(&broken, "-", "这一步出错了"), 1);
    drop(broken);
    // The same event delivered again (e.g. after a restart) is not answered a second time.
    let service = harness.service();
    service
        .receive_file(LINYI, ROOM, "$f01", "F01.pdf", &file)
        .await
        .unwrap();
    assert_eq!(replies_to(&service, "-", "这一步出错了"), 1);
    // A new send of the same file is taken.
    service
        .receive_file(LINYI, ROOM, "$f01-again", "F01.pdf", &file)
        .await
        .unwrap();
    let batch = service
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap();
    assert_eq!(
        service
            .with_store(|store| store.files(&batch.id))
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_handler_cut_off_before_its_commit_is_replayed_and_answered_once() {
    let harness = Harness::new();
    let slow = harness.service_using(
        common::scripted(common::honest),
        common::slow_python(&harness.root, 3),
    );
    let file = std::fs::read(repo().join("fixtures/demo/F01.pdf")).unwrap();
    // The process dies while the core is still reading the file: nothing of this event is committed.
    let cut = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        slow.receive_file(LINYI, ROOM, "$f01", "F01.pdf", &file),
    )
    .await;
    assert!(
        cut.is_err(),
        "the handler must still be running when cut off"
    );
    assert!(!slow.with_store(|store| store.inbound_seen("$f01")).unwrap());
    drop(slow);
    // Matrix delivers the event again after the restart; it is handled and answered exactly once.
    let service = harness.service();
    for _ in 0..2 {
        service
            .receive_file(LINYI, ROOM, "$f01", "F01.pdf", &file)
            .await
            .unwrap();
    }
    let batch = service
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap();
    assert_eq!(
        service
            .with_store(|store| store.files(&batch.id))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(replies_to(&service, &batch.id, "收到「F01.pdf」"), 1);
}

/// What one command does in one state: `Some(text)` = refused with that chat reply or error, state and revision
/// unchanged. Allowed edges are taken by the walk itself, each after its actor and revision checks.
#[tokio::test]
async fn transitions_follow_the_table() {
    let harness = Harness::new();
    let service = harness.service();
    let file = |name: &str| std::fs::read(repo().join("fixtures/demo").join(name)).unwrap();
    let batch_of = |service: &Service| {
        service
            .with_store(|store| store.open_batch_for(LINYI))
            .unwrap()
            .unwrap()
    };
    let mut step = 0;
    // Refused chat commands and desk calls for the state the batch is in now.
    async fn refused(
        service: &Service,
        batch: &str,
        step: &mut usize,
        chat: &[(&str, &str)],
        desk: &[&str],
    ) {
        let before = service.batch(batch).unwrap();
        for (command, reply) in chat {
            *step += 1;
            service
                .receive_text(LINYI, ROOM, &format!("$t{step}"), command)
                .await
                .unwrap();
            let last = texts(service, batch).pop().unwrap_or_default();
            assert!(last.contains(reply), "{:?} {command}: {last}", before.state);
        }
        for call in desk {
            let result = match *call {
                "decide" => {
                    service
                        .decide(batch, LINYI, before.revision, "x", "reject", json!({}))
                        .await
                }
                "confirm_visual" => {
                    service
                        .confirm_visual(batch, LINYI, before.revision, "x", &Map::new())
                        .await
                }
                "confirm" => service.confirm(batch, LINYI, before.revision).await,
                "decline" => service.decline(batch, LINYI, before.revision).await,
                other => panic!("{other}"),
            };
            assert!(
                matches!(
                    result,
                    Err(ServiceError::Invalid(_)) | Err(ServiceError::NotFound)
                ),
                "{:?} {call}: {result:?}",
                before.state
            );
        }
        let after = service.batch(batch).unwrap();
        assert_eq!(
            (after.state, after.revision),
            (before.state, before.revision)
        );
    }
    // Standby -> Collecting: the applicant sends a file (a stranger cannot open a batch at all: see strangers test).
    service
        .receive_file(LINYI, ROOM, "$f", "F01.pdf", &file("F01.pdf"))
        .await
        .unwrap();
    let batch = batch_of(&service).id;
    assert_eq!(service.batch(&batch).unwrap().state, State::Collecting);
    refused(
        &service,
        &batch,
        &mut step,
        &[("重开收件", "不能重开收件"), ("重试", "没有需要重试")],
        &["decide", "confirm_visual", "confirm", "decline"],
    )
    .await;
    common::upload_demo(&service).await;
    // Collecting -> NeedsDecision: only the applicant's command counts.
    service
        .receive_text(INTRUDER, ROOM, "$x-start", "开始对账")
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::Collecting);
    service
        .receive_text(LINYI, ROOM, "$start", "开始对账")
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::NeedsDecision);
    refused(
        &service,
        &batch,
        &mut step,
        &[("开始对账", "不能开始对账"), ("重试", "没有需要重试")],
        &["confirm", "decline"],
    )
    .await;
    let refused_upload = service.batch(&batch).unwrap();
    service
        .receive_file(LINYI, ROOM, "$f-late", "F02-late.pdf", &file("F01.pdf"))
        .await
        .unwrap();
    assert!(
        texts(&service, &batch)
            .pop()
            .unwrap()
            .contains("新文件没有收进来")
    );
    assert_eq!(
        service.batch(&batch).unwrap().revision,
        refused_upload.revision
    );
    // NeedsDecision -> NeedsDecision / AwaitingConfirm: decisions need the applicant and the current revision.
    let f08 = item_id(&service, &batch, "F08.pdf");
    let revision = service.batch(&batch).unwrap().revision;
    let explain = json!({"explanation": "会展"});
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
    decide_everything(&service, &batch).await;
    assert_eq!(service.batch(&batch).unwrap().state, State::AwaitingConfirm);
    refused(
        &service,
        &batch,
        &mut step,
        &[("开始对账", "不能开始对账"), ("重试", "没有需要重试")],
        &["confirm_visual"],
    )
    .await;
    // AwaitingConfirm -> Executing -> ReadyToShare: confirmation needs the applicant and the current revision.
    let revision = service.batch(&batch).unwrap().revision;
    assert!(matches!(
        service.confirm(&batch, ZHOUMIN, revision).await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service.decline(&batch, INTRUDER, revision).await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service.decline(&batch, LINYI, revision - 1).await,
        Err(ServiceError::StaleRevision(_))
    ));
    service.confirm(&batch, LINYI, revision).await.unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::ReadyToShare);
    refused(
        &service,
        &batch,
        &mut step,
        &[
            ("开始对账", "不能开始对账"),
            ("重开收件", "不能重开收件"),
            ("重试", "没有需要重试"),
        ],
        &["decide", "confirm_visual", "confirm", "decline"],
    )
    .await;
    let audit: Vec<String> = service
        .with_store(|store| store.audit(&batch))
        .unwrap()
        .into_iter()
        .map(|row| format!("{}>{}", row.from_state, row.to_state))
        .collect();
    for edge in [
        "standby>collecting",
        "collecting>needs_decision",
        "needs_decision>awaiting_confirm",
        "awaiting_confirm>executing",
        "executing>ready_to_share",
    ] {
        assert!(audit.contains(&edge.to_string()), "{edge}: {audit:?}");
    }
}

#[tokio::test]
async fn a_failed_reading_waits_for_a_person_and_retry_resumes_it() {
    let harness = Harness::new();
    let service = harness.service();
    common::upload_demo(&service).await;
    drop(service);
    let broken = harness.service_using(
        common::scripted(common::honest),
        harness.root.join("no-python"),
    );
    broken
        .receive_text(LINYI, ROOM, "$start", "开始对账")
        .await
        .unwrap();
    let batch = broken
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap();
    assert_eq!(
        (batch.state, batch.resume_state),
        (State::Manual, Some(State::Collecting))
    );
    // Retrying while the core is still down keeps the step to retry.
    broken
        .receive_text(LINYI, ROOM, "$retry1", "重试")
        .await
        .unwrap();
    let batch = broken.batch(&batch.id).unwrap();
    assert_eq!(
        (batch.state, batch.resume_state),
        (State::Manual, Some(State::Collecting))
    );
    assert_eq!(replies_to(&broken, &batch.id, "这一步出错了"), 2);
    drop(broken);
    let service = harness.service();
    service
        .receive_text(INTRUDER, ROOM, "$x-retry", "重试")
        .await
        .unwrap();
    assert_eq!(service.batch(&batch.id).unwrap().state, State::Manual);
    service
        .receive_text(LINYI, ROOM, "$retry2", "重试")
        .await
        .unwrap();
    let batch = service.batch(&batch.id).unwrap();
    assert_eq!(
        (batch.state, batch.resume_state),
        (State::NeedsDecision, None)
    );
    assert!(
        texts(&service, &batch.id)
            .iter()
            .any(|t| t.contains("本批次"))
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
