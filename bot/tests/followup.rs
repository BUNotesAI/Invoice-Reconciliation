//! Script step 15: missing-invoice follow-up after approval — candidates, ways to get the invoice, billing details,
//! reminders on a virtual clock, and invoices sent into chat claimed automatically.
mod common;

use std::sync::atomic::Ordering;

use common::{
    Harness, LINYI, MONTH_END, ROOM, ZHOUMIN, decide_everything, repo, texts, to_needs_decision,
};
use reimb_bot::{
    service::{Service, ServiceError},
    store::State,
};
use serde_json::{Value, json};

const DAY: i64 = 86_400;

async fn approved(harness: &Harness) -> (Service, String) {
    let (service, batch) = to_needs_decision(harness).await;
    decide_everything(&service, &batch).await;
    let revision = |s: &Service| s.batch(&batch).unwrap().revision;
    service
        .confirm(&batch, LINYI, revision(&service))
        .await
        .unwrap();
    service
        .submit(&batch, LINYI, revision(&service))
        .await
        .unwrap();
    service
        .approve(&batch, ZHOUMIN, revision(&service))
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::Approved);
    (service, batch)
}

fn spend(service: &Service, batch: &str, merchant: &str) -> Value {
    service
        .follow_ups(batch)
        .unwrap()
        .into_iter()
        .find(|f| f["merchant"] == merchant)
        .unwrap()
}

fn status(service: &Service, batch: &str, merchant: &str) -> String {
    spend(service, batch, merchant)["status"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn act(
    service: &Service,
    batch: &str,
    merchant: &str,
    action: &str,
    payload: Value,
) -> Result<(), ServiceError> {
    let id = spend(service, batch, merchant)["id"]
        .as_str()
        .unwrap()
        .to_string();
    let revision = service.batch(batch).unwrap().revision;
    service
        .follow_up(batch, LINYI, revision, &id, action, &payload)
        .await
}

fn claim_file(name: &str) -> Vec<u8> {
    std::fs::read(repo().join("fixtures/claim").join(name)).unwrap()
}

#[tokio::test]
async fn approval_opens_one_follow_up_per_candidate() {
    let harness = Harness::new();
    let (service, batch) = approved(&harness).await;
    let follow = service.follow_ups(&batch).unwrap();
    let rows: Vec<(String, i64, String, String)> = follow
        .iter()
        .map(|f| {
            (
                f["merchant"].as_str().unwrap().into(),
                f["amount_cents"].as_i64().unwrap(),
                f["status"].as_str().unwrap().into(),
                f["deadline"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                "京东商城".into(),
                45900,
                "discovered".into(),
                "2026-11-30".into()
            ),
            (
                "悦途酒店".into(),
                48000,
                "discovered".into(),
                "2026-11-30".into()
            ),
        ]
    );
    assert_eq!(
        follow[0]["billing"],
        json!({"name": "示例科技有限公司", "tax_id": "91440300XXXXXXXX0A"})
    );
    let posted = texts(&service, &batch);
    assert!(posted.iter().any(|t| t.contains("财务已审批通过")));
    let notice: Vec<&String> = posted
        .iter()
        .filter(|t| t.contains("可能漏票要跟进"))
        .collect();
    assert_eq!(notice.len(), 1, "{posted:?}");
    assert!(
        notice[0].contains("京东商城 ¥459.00（2026-10-22）")
            && notice[0].contains("悦途酒店 ¥480.00（2026-10-24）")
    );
    // The on-detect reminder is recorded, so the reminder run does not send it again.
    for f in &follow {
        let sent = service
            .with_store(|store| store.reminded(f["id"].as_str().unwrap()))
            .unwrap();
        assert_eq!(sent, [("on_detect".to_string(), "detect".to_string())]);
    }
    assert_eq!(service.remind_follow_ups().unwrap(), 0);
}

#[tokio::test]
async fn applicant_steps_follow_the_follow_up_state_machine() {
    let harness = Harness::new();
    let (service, batch) = approved(&harness).await;
    let id = spend(&service, &batch, "京东商城")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let revision = service.batch(&batch).unwrap().revision;
    // Only the applicant, only at the current revision, only a step the record admits now.
    assert!(matches!(
        service
            .follow_up(&batch, ZHOUMIN, revision, &id, "business", &json!({}))
            .await,
        Err(ServiceError::Forbidden)
    ));
    assert!(matches!(
        service
            .follow_up(&batch, LINYI, revision - 1, &id, "business", &json!({}))
            .await,
        Err(ServiceError::StaleRevision(_))
    ));
    assert!(matches!(
        act(
            &service,
            &batch,
            "京东商城",
            "method",
            json!({"method": "platform"})
        )
        .await,
        Err(ServiceError::Invalid(_))
    ));
    // 是公务: the history is searched first; nothing there, so a way to get the invoice is asked for.
    act(&service, &batch, "京东商城", "business", json!({}))
        .await
        .unwrap();
    assert_eq!(status(&service, &batch, "京东商城"), "business");
    assert!(matches!(
        act(
            &service,
            &batch,
            "京东商城",
            "method",
            json!({"method": "fax"})
        )
        .await,
        Err(ServiceError::Invalid(_))
    ));
    act(
        &service,
        &batch,
        "京东商城",
        "method",
        json!({"method": "platform"}),
    )
    .await
    .unwrap();
    assert_eq!(status(&service, &batch, "京东商城"), "waiting");
    let guide = texts(&service, &batch).pop().unwrap();
    assert!(
        guide.contains("自助申请开票")
            && guide.contains("名称：示例科技有限公司")
            && guide.contains("税号：91440300XXXXXXXX0A"),
        "{guide}"
    );
    assert!(matches!(
        act(&service, &batch, "京东商城", "not_business", json!({})).await,
        Err(ServiceError::Invalid(_))
    ));
    // 不是: only this one payment is dropped.
    act(&service, &batch, "悦途酒店", "not_business", json!({}))
        .await
        .unwrap();
    assert_eq!(status(&service, &batch, "悦途酒店"), "ignored");
    assert!(
        service
            .with_store(|store| store.muted_merchants(LINYI))
            .unwrap()
            .is_empty()
    );
    // 开不了: a no-invoice note of bounded length closes it; a closed record takes no more steps.
    for note in ["", &"长".repeat(201)] {
        assert!(matches!(
            act(
                &service,
                &batch,
                "京东商城",
                "no_invoice",
                json!({"note": note})
            )
            .await,
            Err(ServiceError::Invalid(_))
        ));
    }
    act(
        &service,
        &batch,
        "京东商城",
        "no_invoice",
        json!({"note": "个人垫付，商家已注销无法开票"}),
    )
    .await
    .unwrap();
    assert_eq!(
        spend(&service, &batch, "京东商城")["note"],
        "个人垫付，商家已注销无法开票"
    );
    assert!(matches!(
        act(&service, &batch, "京东商城", "abandon", json!({})).await,
        Err(ServiceError::Invalid(_))
    ));
    assert!(
        service
            .with_store(|store| store.open_spends(Some(LINYI)))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_muted_merchant_leaves_later_reports_until_unmuted() {
    let harness = Harness::new();
    let (service, batch) = approved(&harness).await;
    act(&service, &batch, "悦途酒店", "mute", json!({}))
        .await
        .unwrap();
    assert_eq!(status(&service, &batch, "悦途酒店"), "ignored");
    assert_eq!(
        service
            .with_store(|store| store.muted_merchants(LINYI))
            .unwrap(),
        ["悦途酒店"]
    );
    // The next batch reads the same bill: 悦途酒店 is no longer a candidate and the report discloses the mute.
    // The same month's files again, as new chat events.
    let mut paths: Vec<_> = std::fs::read_dir(repo().join("fixtures/demo"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap() != "history.json")
        .collect();
    paths.sort();
    for (index, path) in paths.iter().enumerate() {
        let name = path.file_name().unwrap().to_str().unwrap();
        service
            .receive_file(
                LINYI,
                ROOM,
                &format!("$again{index}"),
                name,
                &std::fs::read(path).unwrap(),
            )
            .await
            .unwrap();
    }
    service
        .receive_text(LINYI, ROOM, "$next-start", "开始对账")
        .await
        .unwrap();
    let next = service
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap()
        .id;
    assert_ne!(next, batch);
    let report = service.assessment(&next).unwrap().unwrap().report;
    // (Its invoices are duplicates of the approved batch now, so their payments show up; 悦途酒店 does not.)
    let merchants: Vec<&str> = report
        .missing_candidates
        .iter()
        .map(|m| m.merchant.as_str())
        .collect();
    assert!(
        merchants.contains(&"京东商城") && !merchants.contains(&"悦途酒店"),
        "{merchants:?}"
    );
    assert!(
        report.text.contains("已忽略商户：悦途酒店"),
        "{}",
        report.text
    );
    let revision = service.batch(&batch).unwrap().revision;
    service
        .unmute(&batch, LINYI, revision, "悦途酒店")
        .await
        .unwrap();
    assert!(matches!(
        service.unmute(&batch, LINYI, revision, "悦途酒店").await,
        Err(ServiceError::NotFound)
    ));
    assert!(matches!(
        service.unmute(&batch, ZHOUMIN, revision, "悦途酒店").await,
        Err(ServiceError::Forbidden)
    ));
}

#[tokio::test]
async fn reminders_are_due_once_per_slot_and_stop_at_the_deadline() {
    let harness = Harness::new();
    let (service, batch) = approved(&harness).await;
    // Close one follow-up so exactly one stays open.
    act(&service, &batch, "京东商城", "not_business", json!({}))
        .await
        .unwrap();
    let at = |days: i64, hours: i64| MONTH_END + days * DAY + hours * 3600;
    let reminders = |service: &Service| -> Vec<String> {
        texts(service, &batch)
            .into_iter()
            .filter(|t| t.starts_with("漏票提醒") || t.contains("已过截止日"))
            .collect()
    };
    // Sunday 2026-11-01: nothing due. Monday 2026-11-02 08:59: still nothing; 09:00: the weekly reminder.
    harness.clock.store(at(1, 0), Ordering::SeqCst);
    assert_eq!(service.remind_follow_ups().unwrap(), 0);
    harness.clock.store(at(2, 0) - 60, Ordering::SeqCst);
    assert_eq!(service.remind_follow_ups().unwrap(), 0);
    harness.clock.store(at(2, 0), Ordering::SeqCst);
    assert_eq!(service.remind_follow_ups().unwrap(), 1);
    assert_eq!(
        service.remind_follow_ups().unwrap(),
        0,
        "same slot, same run"
    );
    drop(service);
    // A restart does not send it again.
    let service = harness.service();
    assert_eq!(service.remind_follow_ups().unwrap(), 0);
    let first = reminders(&service);
    assert_eq!(first.len(), 1);
    assert!(
        first[0].contains("悦途酒店 ¥480.00（2026-10-24）") && first[0].contains("截止 2026-11-30"),
        "{first:?}"
    );
    // Friday 2026-11-27 09:00: that week's Monday slot and the three-days-before reminder.
    harness.clock.store(at(27, 0), Ordering::SeqCst);
    assert_eq!(service.remind_follow_ups().unwrap(), 2);
    assert!(
        reminders(&service)
            .iter()
            .any(|t| t.contains("距截止（2026-11-30）还有 3 天"))
    );
    // The day after the deadline: one overdue prompt, then silence.
    harness.clock.store(at(31, 0), Ordering::SeqCst);
    assert_eq!(service.remind_follow_ups().unwrap(), 1);
    harness.clock.store(at(38, 0), Ordering::SeqCst);
    assert_eq!(service.remind_follow_ups().unwrap(), 0);
    let last = reminders(&service).pop().unwrap();
    assert!(
        last.contains("已过截止日") && last.contains("写无票说明，还是放弃"),
        "{last}"
    );
    // Overdue records still take a no-invoice note or abandonment.
    act(&service, &batch, "悦途酒店", "abandon", json!({}))
        .await
        .unwrap();
    assert_eq!(status(&service, &batch, "悦途酒店"), "abandoned");
}

#[tokio::test]
async fn invoices_sent_into_chat_are_claimed_or_refused_with_the_reason() {
    let harness = Harness::new();
    let (service, batch) = approved(&harness).await;
    for merchant in ["京东商城", "悦途酒店"] {
        act(&service, &batch, merchant, "business", json!({}))
            .await
            .unwrap();
        act(
            &service,
            &batch,
            merchant,
            "method",
            json!({"method": "merchant"}),
        )
        .await
        .unwrap();
    }
    // The invoices arrive in November: they go into the next period's batch (design §8.3 归入下一批次).
    harness.clock.store(MONTH_END + 2 * DAY, Ordering::SeqCst);
    let last_reply = |service: &Service| {
        let next = service
            .with_store(|store| store.open_batch_for(LINYI))
            .unwrap();
        let mut all = texts(service, next.as_ref().map_or("-", |b| b.id.as_str()));
        all.pop().unwrap_or_default()
    };
    // A personal-title invoice for the 京东 payment: refused with the billing details, not filed anywhere.
    service
        .receive_file(LINYI, ROOM, "$c03", "C03.pdf", &claim_file("C03.pdf"))
        .await
        .unwrap();
    let reply = last_reply(&service);
    assert!(
        reply.contains("京东商城 ¥459.00")
            && reply.contains("抬头不是公司")
            && reply.contains("91440300XXXXXXXX0A"),
        "{reply}"
    );
    assert_eq!(status(&service, &batch, "京东商城"), "waiting");
    // Wrong amount for the hotel payment.
    service
        .receive_file(LINYI, ROOM, "$c02", "C02.pdf", &claim_file("C02.pdf"))
        .await
        .unwrap();
    assert!(last_reply(&service).contains("金额和这笔支付对不上"));
    let next = service
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap();
    assert_eq!(next.period, "2026-11");
    assert!(
        service
            .with_store(|store| store.files(&next.id))
            .unwrap()
            .is_empty()
    );
    // The reissued 京东 invoice and the hotel invoice are claimed and filed into the next batch.
    service
        .receive_file(LINYI, ROOM, "$c04", "C04.pdf", &claim_file("C04.pdf"))
        .await
        .unwrap();
    let reply = last_reply(&service);
    assert!(
        reply.contains(
            "这是漏票 京东商城 ¥459.00（2026-10-22） 的发票，已认领，归入下一批次（2026-11）"
        ),
        "{reply}"
    );
    service
        .receive_file(LINYI, ROOM, "$c01", "C01.pdf", &claim_file("C01.pdf"))
        .await
        .unwrap();
    // Delivered again: nothing changes.
    service
        .receive_file(LINYI, ROOM, "$c01", "C01.pdf", &claim_file("C01.pdf"))
        .await
        .unwrap();
    let claimed = spend(&service, &batch, "悦途酒店");
    assert_eq!(
        (
            claimed["status"].as_str(),
            claimed["claimed_invoice"].as_str(),
            claimed["next_batch"].as_str()
        ),
        (
            Some("claimed"),
            Some("26312000000800000101"),
            Some(next.id.as_str())
        )
    );
    assert_eq!(
        spend(&service, &batch, "京东商城")["claimed_invoice"],
        "26112000000900000202"
    );
    let files: Vec<String> = service
        .with_store(|store| store.files(&next.id))
        .unwrap()
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert_eq!(files, ["C04.pdf", "C01.pdf"]);
    // An ordinary invoice with no follow-up behind it is just an upload.
    service
        .receive_file(
            LINYI,
            ROOM,
            "$f01",
            "F01.pdf",
            &std::fs::read(repo().join("fixtures/demo/F01.pdf")).unwrap(),
        )
        .await
        .unwrap();
    assert!(last_reply(&service).starts_with("收到「F01.pdf」"));
    assert!(
        service
            .with_store(|store| store.open_spends(Some(LINYI)))
            .unwrap()
            .is_empty()
    );
}
