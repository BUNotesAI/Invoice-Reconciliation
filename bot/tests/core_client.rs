//! The core boundary fails safely: timeouts, output floods and crashes never yield a result.
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

use reimb_bot::core_client::{CoreClient, CoreCommand, CoreFailure};
use serde_json::json;

fn fake_python(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("python");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

// First execution of a freshly written script can take about a second on macOS, so limits leave room for it.
fn client(python: PathBuf, dir: &tempfile::TempDir) -> CoreClient {
    let mut client = CoreClient::new(
        python,
        dir.path().into(),
        dir.path().into(),
        dir.path().join("b"),
        dir.path().join("p.yaml"),
    );
    client.timeout = Duration::from_secs(5);
    client
}

#[tokio::test]
async fn hanging_core_times_out_and_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("still-running");
    let python = fake_python(&dir, &format!("sleep 4; touch {}", marker.display()));
    let mut core = client(python, &dir);
    core.timeout = Duration::from_secs(2);
    assert_eq!(
        core.call(CoreCommand::Gates, &json!({})).await,
        Err(CoreFailure::Timeout)
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !marker.exists(),
        "the timed-out process group must be killed"
    );
}

#[tokio::test]
async fn output_flood_is_cut() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(&dir, "cat >/dev/null; head -c 9000000 /dev/zero");
    let result = client(python, &dir)
        .call(CoreCommand::Gates, &json!({}))
        .await;
    assert_eq!(result, Err(CoreFailure::OutputLimit));
}

#[tokio::test]
async fn crash_and_garbage_are_not_results() {
    let dir = tempfile::tempdir().unwrap();
    let crash = fake_python(&dir, "cat >/dev/null; exit 9");
    assert!(matches!(
        client(crash, &dir)
            .call(CoreCommand::Gates, &json!({}))
            .await,
        Err(CoreFailure::Crash(_))
    ));
    let dir = tempfile::tempdir().unwrap();
    let garbage = fake_python(&dir, "cat >/dev/null; echo not-json");
    assert!(matches!(
        client(garbage, &dir)
            .call(CoreCommand::Gates, &json!({}))
            .await,
        Err(CoreFailure::Crash(_))
    ));
}

#[tokio::test]
async fn answer_for_another_request_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(
        &dir,
        r#"cat >/dev/null; echo '{"schema_version":1,"request_id":"other","ok":true,"result":{}}'"#,
    );
    assert!(matches!(
        client(python, &dir)
            .call(CoreCommand::Gates, &json!({}))
            .await,
        Err(CoreFailure::Crash(_))
    ));
}

#[tokio::test]
async fn success_with_error_exit_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(
        &dir,
        r#"cat >/dev/null; echo '{"schema_version":1,"request_id":"orc-1","ok":true,"result":{}}'; exit 3"#,
    );
    assert!(matches!(
        client(python, &dir)
            .call(CoreCommand::Gates, &json!({}))
            .await,
        Err(CoreFailure::Crash(_))
    ));
}
