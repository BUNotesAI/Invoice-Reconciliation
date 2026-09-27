//! Script steps 12-14: submission, finance approval, return of one item, supplement, rebuild and resubmission.
mod common;

use common::{
    FINANCE_ROOM, Harness, INTRUDER, LINYI, ROOM, ZHOUMIN, decide_everything, item_id, repo,
    to_needs_decision,
};
use reimb_bot::{
    service::{Service, ServiceError, frozen_items_unchanged, supplement_text},
    store::State,
};
use serde_json::{Map, Value, json};

async fn published(harness: &Harness) -> (Service, String) {
    let (service, batch) = to_needs_decision(harness).await;
    decide_everything(&service, &batch).await;
    service
        .confirm(&batch, LINYI, service.batch(&batch).unwrap().revision)
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::ReadyToShare);
    (service, batch)
}

fn revision(service: &Service, batch: &str) -> i64 {
    service.batch(batch).unwrap().revision
}

fn manifest(harness: &Harness, batch: &str, revision: i64) -> Value {
    let path = harness
        .root
        .join("batches")
        .join(batch)
        .join("published")
        .join(revision.to_string())
        .join("manifest.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn posted_to(service: &Service, batch: &str, room: &str) -> Vec<String> {
    service
        .with_store(|store| store.pending_outbox())
        .unwrap()
        .into_iter()
        .filter(|m| m.batch_id == batch && m.room_id == room)
        .filter_map(|m| m.content["body"].as_str().map(String::from))
        .collect()
}

#[tokio::test]
async fn submit_and_approve() {
    let harness = Harness::new();
    let (service, batch) = published(&harness).await;
    let rev = revision(&service, &batch);
    // Only the applicant submits, and only the published revision.
    assert!(matches!(
        service.submit(&batch, ZHOUMIN, rev).await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service.submit(&batch, LINYI, rev - 1).await,
        Err(ServiceError::StaleRevision(_))
    ));
    service.submit(&batch, LINYI, rev).await.unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::Submitted);
    let notices = posted_to(&service, &batch, FINANCE_ROOM);
    assert!(
        notices
            .iter()
            .any(|t| t.contains("林一提交了 2026-10 报销：10 张发票，合计 ¥4,901.40")),
        "{notices:?}"
    );
    assert!(notices.iter().any(|t| t.starts_with("[Mini app]")));
    let submitted = service
        .with_store(|store| store.history_events(None))
        .unwrap();
    assert_eq!(
        submitted
            .iter()
            .filter(|e| e["status"] == "submitted")
            .count(),
        10
    );
    // Only finance approves, at the submitted revision.
    assert!(matches!(
        service.approve(&batch, LINYI, rev).await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service.approve(&batch, INTRUDER, rev).await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service.approve(&batch, ZHOUMIN, rev + 1).await,
        Err(ServiceError::StaleRevision(_))
    ));
    service.approve(&batch, ZHOUMIN, rev).await.unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::Approved);
    let ledger = service
        .with_store(|store| store.history_events(None))
        .unwrap();
    assert_eq!(
        ledger.iter().filter(|e| e["status"] == "approved").count(),
        10
    );
    assert!(
        posted_to(&service, &batch, ROOM)
            .iter()
            .any(|t| t.contains("财务已审批通过"))
    );
}

#[tokio::test]
async fn an_approved_invoice_is_a_duplicate_in_the_next_batch() {
    let harness = Harness::new();
    let (service, batch) = published(&harness).await;
    service
        .submit(&batch, LINYI, revision(&service, &batch))
        .await
        .unwrap();
    service
        .approve(&batch, ZHOUMIN, revision(&service, &batch))
        .await
        .unwrap();
    // The approved batch is closed: a new upload opens a new batch whose history includes the ledger.
    let f01 = std::fs::read(repo().join("fixtures/demo/F01.pdf")).unwrap();
    service
        .receive_file(LINYI, ROOM, "$next-f01", "F01.pdf", &f01)
        .await
        .unwrap();
    service
        .receive_text(LINYI, ROOM, "$next-start", "开始对账")
        .await
        .unwrap();
    let next = service
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap();
    assert_ne!(next.id, batch);
    let report = service.assessment(&next.id).unwrap().unwrap().report;
    assert_eq!(report.rejected.count, 1);
    assert_eq!(report.items[0].reasons, ["DUPLICATE_INVOICE"]);
}

#[tokio::test]
async fn return_one_item_supplement_and_resubmit_keeps_the_rest() {
    let harness = Harness::new();
    let (service, batch) = published(&harness).await;
    let first = revision(&service, &batch);
    service.submit(&batch, LINYI, first).await.unwrap();
    let f08 = item_id(&service, &batch, "F08.pdf");
    let f01 = item_id(&service, &batch, "F01.pdf");
    let mut returned = Map::new();
    returned.insert(f08.clone(), json!("请补充住宿的事由和人数"));
    // Returns are finance-only, bound to the revision, and name submitted items with a reason.
    assert!(matches!(
        service.return_items(&batch, LINYI, first, &returned).await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service
            .return_items(&batch, ZHOUMIN, first - 1, &returned)
            .await,
        Err(ServiceError::StaleRevision(_))
    ));
    let mut bad = Map::new();
    bad.insert("item-unknown".into(), json!("x"));
    assert!(matches!(
        service.return_items(&batch, ZHOUMIN, first, &bad).await,
        Err(ServiceError::Invalid(_))
    ));
    service
        .return_items(&batch, ZHOUMIN, first, &returned)
        .await
        .unwrap();
    let state = service.batch(&batch).unwrap();
    assert_eq!(
        (state.state, state.revision),
        (State::NeedsDecision, first + 1)
    );
    assert!(
        posted_to(&service, &batch, ROOM)
            .iter()
            .any(|t| t.contains("财务退回了 1 项") && t.contains("请补充住宿的事由和人数"))
    );
    // Only the returned item is unfrozen.
    let other = service
        .decide(
            &batch,
            LINYI,
            state.revision,
            &f01,
            "reject",
            json!({"reason": "x"}),
        )
        .await;
    assert!(matches!(other, Err(ServiceError::Invalid(_))), "{other:?}");
    assert!(matches!(
        service
            .supplement(&batch, LINYI, state.revision, &f01, 2, "接待")
            .await,
        Err(ServiceError::Invalid(_))
    ));
    assert!(matches!(
        service
            .supplement(&batch, LINYI, state.revision, &f08, 0, "会展")
            .await,
        Err(ServiceError::Invalid(_))
    ));
    let text = service
        .supplement(&batch, LINYI, state.revision, &f08, 3, "客户会展接待")
        .await
        .unwrap();
    assert_eq!(text, "客户会展接待，3 人");
    let ready = service.batch(&batch).unwrap();
    assert_eq!(ready.state, State::AwaitingConfirm);
    service
        .confirm(&batch, LINYI, ready.revision)
        .await
        .unwrap();
    let rebuilt = service.batch(&batch).unwrap();
    assert_eq!(
        rebuilt.state,
        State::ReadyToShare,
        "the rebuilt revision passes the final review (F6: own history excluded)"
    );
    // Everything but the returned item is unchanged, compared by stable item id, not row number or bytes.
    let (old, new) = (
        manifest(&harness, &batch, first),
        manifest(&harness, &batch, rebuilt.revision),
    );
    let rows = |m: &Value| -> std::collections::BTreeMap<String, Value> {
        m["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["item_id"].as_str().unwrap().to_string(), r.clone()))
            .collect()
    };
    let (old_rows, new_rows) = (rows(&old), rows(&new));
    assert_eq!(
        old_rows.keys().collect::<Vec<_>>(),
        new_rows.keys().collect::<Vec<_>>()
    );
    for (id, row) in &old_rows {
        let now = &new_rows[id];
        assert_eq!(row["attachment_hash"], now["attachment_hash"]);
        if id == &f08 {
            assert_ne!(row["business_hash"], now["business_hash"]);
            assert!(
                now["expense_detail"]
                    .as_str()
                    .unwrap()
                    .ends_with("（客户会展接待，3 人）")
            );
        } else {
            assert_eq!(row["business_hash"], now["business_hash"], "{id} changed");
        }
    }
    assert_eq!(new["total_cents"], old["total_cents"]);
    // Resubmission: the ledger keeps each invoice's first submission only.
    service
        .submit(&batch, LINYI, rebuilt.revision)
        .await
        .unwrap();
    let ledger = service
        .with_store(|store| store.history_events(None))
        .unwrap();
    assert_eq!(
        ledger.iter().filter(|e| e["status"] == "submitted").count(),
        10
    );
}

#[test]
fn frozen_items_may_not_change() {
    let previous = json!({"items": [{"id": "a", "expense_detail": "x", "decision_ids": []}, {"id": "b", "expense_detail": "y", "decision_ids": []}],
                          "decisions": []});
    let returned: Map<String, Value> = [("b".to_string(), json!("reason"))].into_iter().collect();
    let mut next = previous.clone();
    next["items"][1]["expense_detail"] = json!("y（补充）");
    assert!(frozen_items_unchanged(&previous, &next, &returned).is_ok());
    next["items"][0]["expense_detail"] = json!("changed");
    assert!(frozen_items_unchanged(&previous, &next, &returned).is_err());
    let mut dropped = previous.clone();
    dropped["items"].as_array_mut().unwrap().remove(0);
    assert!(frozen_items_unchanged(&previous, &dropped, &returned).is_err());
}

#[test]
fn supplement_text_is_bounded() {
    assert_eq!(supplement_text(2, " 客户接待 ").unwrap(), "客户接待，2 人");
    for (people, purpose) in [(0, "x"), (201, "x"), (2, ""), (2, "a\nb")] {
        assert!(supplement_text(people, purpose).is_err());
    }
}

#[tokio::test]
async fn reading_again_after_a_return_ignores_the_batch_own_submission() {
    // Finding F6: re-reading the batch must not treat its own submitted invoices as history duplicates.
    let harness = Harness::new();
    let (service, batch) = published(&harness).await;
    let first = revision(&service, &batch);
    service.submit(&batch, LINYI, first).await.unwrap();
    let mut returned = Map::new();
    returned.insert(item_id(&service, &batch, "F08.pdf"), json!("补充说明"));
    service
        .return_items(&batch, ZHOUMIN, first, &returned)
        .await
        .unwrap();
    service
        .receive_text(LINYI, ROOM, "$reopen-after-return", "重开收件")
        .await
        .unwrap();
    service
        .receive_text(LINYI, ROOM, "$restart-after-return", "开始对账")
        .await
        .unwrap();
    let report = service.assessment(&batch).unwrap().unwrap().report;
    assert_eq!(
        report.rejected.count, 2,
        "only F11 and F12, never the batch's own ten invoices"
    );
}

#[tokio::test]
async fn confirming_after_a_return_refuses_a_changed_frozen_item() {
    let harness = Harness::new();
    let (service, batch) = published(&harness).await;
    service
        .submit(&batch, LINYI, revision(&service, &batch))
        .await
        .unwrap();
    let f08 = item_id(&service, &batch, "F08.pdf");
    let mut returned = Map::new();
    returned.insert(f08.clone(), json!("请补充住宿的事由和人数"));
    service
        .return_items(&batch, ZHOUMIN, revision(&service, &batch), &returned)
        .await
        .unwrap();
    service
        .supplement(
            &batch,
            LINYI,
            revision(&service, &batch),
            &f08,
            3,
            "客户会展接待",
        )
        .await
        .unwrap();
    // A defect elsewhere changes an item finance did not return; confirmation must not publish it.
    let f01 = item_id(&service, &batch, "F01.pdf");
    let mut reading = service
        .with_store(|store| store.document(&batch, "reading"))
        .unwrap()
        .unwrap();
    for item in reading["items"].as_array_mut().unwrap() {
        if item["id"] == f01.as_str() {
            item["invoice"]["remark"]["value"] = json!("改过的备注");
        }
    }
    service
        .with_store(|store| {
            let work = store.begin()?;
            work.put_document(&batch, "reading", &reading)?;
            work.commit()
        })
        .unwrap();
    let before = service.batch(&batch).unwrap();
    let result = service.confirm(&batch, LINYI, before.revision).await;
    assert!(
        matches!(result, Err(ServiceError::Invalid(_))),
        "{result:?}"
    );
    let after = service.batch(&batch).unwrap();
    assert_eq!(
        (after.state, after.revision),
        (State::AwaitingConfirm, before.revision)
    );
}
