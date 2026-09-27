//! Failure exits of design §8.1 through the service, with the core made to fail on purpose.
mod common;

use std::{os::unix::fs::PermissionsExt, path::PathBuf};

use common::{Harness, LINYI, ROOM, decide_everything, honest, repo, scripted, texts, upload_demo};
use reimb_bot::store::State;

/// An interpreter stand-in: `crash` exits with 9; `verify-fails` answers verify with a failed review and passes every
/// other command to the real core.
fn fake_python(harness: &Harness, mode: &str) -> PathBuf {
    let path = harness.root.join(format!("fake-python-{mode}"));
    let real = repo().join(".venv/bin/python");
    let body = match mode {
        "crash" => "#!/bin/sh\ncat >/dev/null\nexit 9\n".to_string(),
        _ => format!(
            "#!{real}\nimport json, os, sys\nif sys.argv[1:] == ['-m', 'reimb_core', 'verify']:\n    request = json.loads(sys.stdin.buffer.read())\n    sys.stdout.write(json.dumps({{'schema_version': 1, 'request_id': request['request_id'], 'ok': True, 'result': {{'passed': False, 'checks': [], 'issues': ['injected']}}}}) + '\\n')\n    sys.exit(0)\nos.execv('{real}', ['{real}'] + sys.argv[1:])\n",
            real = real.display()
        ),
    };
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn a_crashing_core_goes_to_manual_and_retry_resumes() {
    let harness = Harness::new();
    let service = harness.service();
    upload_demo(&service).await;
    let broken = harness.service_using(scripted(honest), fake_python(&harness, "crash"));
    drop(service);
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
    assert!(
        texts(&broken, &batch.id)
            .iter()
            .any(|t| t.contains("这一步出错了，已记录，可以回复「重试」"))
    );
    drop(broken);
    let service = harness.service();
    service
        .receive_text(LINYI, ROOM, "$retry", "重试")
        .await
        .unwrap();
    let batch = service.batch(&batch.id).unwrap();
    assert_eq!(batch.state, State::NeedsDecision);
    assert!(
        texts(&service, &batch.id)
            .iter()
            .any(|t| t.contains("待你判断 4 张"))
    );
}

#[tokio::test]
async fn a_busy_output_waits_without_overwriting_and_retries() {
    let harness = Harness::new();
    let (service, batch) = common::to_needs_decision(&harness).await;
    decide_everything(&service, &batch).await;
    let published = harness.root.join("batches").join(&batch).join("published");
    std::fs::create_dir_all(&published).unwrap();
    std::fs::set_permissions(&published, std::fs::Permissions::from_mode(0o500)).unwrap();
    service
        .confirm(&batch, LINYI, service.batch(&batch).unwrap().revision)
        .await
        .unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::OutputWait);
    assert!(
        texts(&service, &batch)
            .iter()
            .any(|t| t.contains("暂时写不进去"))
    );
    assert!(
        std::fs::read_dir(&published).unwrap().next().is_none(),
        "nothing was written"
    );
    // Still busy: a retry waits again and does not post a second notice.
    service.retry_waiting().await.unwrap();
    assert_eq!(service.batch(&batch).unwrap().state, State::OutputWait);
    assert_eq!(
        texts(&service, &batch)
            .iter()
            .filter(|t| t.contains("暂时写不进去"))
            .count(),
        1
    );
    std::fs::set_permissions(&published, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(service.retry_waiting().await.unwrap(), 1);
    assert_eq!(service.batch(&batch).unwrap().state, State::ReadyToShare);
    assert!(
        texts(&service, &batch)
            .iter()
            .any(|t| t.contains("终审全部通过"))
    );
}

#[tokio::test]
async fn three_failed_final_reviews_hand_over_to_a_person() {
    let harness = Harness::new();
    let (service, batch) = common::to_needs_decision(&harness).await;
    decide_everything(&service, &batch).await;
    drop(service);
    let failing = harness.service_using(scripted(honest), fake_python(&harness, "verify-fails"));
    for attempt in 1..=3 {
        let current = failing.batch(&batch).unwrap();
        assert_eq!(current.state, State::AwaitingConfirm, "attempt {attempt}");
        failing
            .confirm(&batch, LINYI, current.revision)
            .await
            .unwrap();
    }
    let done = failing.batch(&batch).unwrap();
    assert_eq!((done.state, done.verify_attempts), (State::Manual, 3));
    assert!(
        done.published_revision.is_none(),
        "a failed review never publishes"
    );
    assert_eq!(
        texts(&failing, &batch)
            .iter()
            .filter(|t| t.contains("终审没有通过"))
            .count(),
        3
    );
}

#[tokio::test]
async fn an_unreadable_image_invoice_asks_for_the_pdf_or_a_manual_check() {
    let harness = Harness::new();
    let service = harness.service_with(scripted(|request| match request.task {
        reimb_bot::agent::AgentTask::ReadInvoice => Ok("这张图看不清".into()),
        _ => honest(request),
    }));
    upload_demo(&service).await;
    service
        .receive_text(LINYI, ROOM, "$start", "开始对账")
        .await
        .unwrap();
    let batch = service
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap();
    assert_eq!(batch.state, State::NeedsDecision);
    let report = service.assessment(&batch.id).unwrap().unwrap().report;
    let f09 = report
        .items
        .iter()
        .find(|item| item.file == "F09.pdf")
        .unwrap();
    assert_eq!(f09.disposition, "needs_decision");
    assert!(
        report.text.contains("请发原始 PDF 或手动核对"),
        "{}",
        report.text
    );
}

#[tokio::test]
async fn a_correction_that_fails_the_format_check_changes_nothing() {
    let harness = Harness::new();
    let (service, batch) = common::to_needs_decision(&harness).await;
    let before = service.batch(&batch).unwrap();
    let f09 = common::item_id(&service, &batch, "F09.pdf");
    for (field, value) in [("amount_cents", "三百"), ("invoice_no", "123")] {
        let mut corrections = serde_json::Map::new();
        corrections.insert(field.into(), serde_json::json!(value));
        let result = service
            .confirm_visual(&batch, LINYI, before.revision, &f09, &corrections)
            .await;
        assert!(
            matches!(result, Err(reimb_bot::service::ServiceError::Invalid(_))),
            "{field}: {result:?}"
        );
    }
    let after = service.batch(&batch).unwrap();
    assert_eq!(
        (after.state, after.revision),
        (before.state, before.revision)
    );
    let reading = service
        .with_store(|store| store.document(&batch, "reading"))
        .unwrap()
        .unwrap();
    let item = reading["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == f09.as_str())
        .unwrap();
    assert_eq!(item["invoice"]["invoice_no"]["level"], "candidate");
    assert!(
        service
            .with_store(|store| store.decisions(&batch))
            .unwrap()
            .is_empty()
    );
}
