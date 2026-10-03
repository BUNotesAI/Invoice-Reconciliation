//! The desk page in a real browser: it pairs through the chat and keeps polling afterwards.
//!
//! Runs only with REIMB_BROWSER set to a Chrome or Chromium executable, e.g.
//! `REIMB_BROWSER="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" cargo test --test browser`.
mod common;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use common::{Harness, LINYI, ROOM, to_needs_decision};
use reimb_bot::desk::{DeskConfig, router};

fn state_requests(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("/state"))
        .count()
}

#[tokio::test]
async fn a_paired_page_keeps_polling() {
    let Some(chrome) = std::env::var_os("REIMB_BROWSER") else {
        return;
    };
    let harness = Harness::new();
    let (service, batch) = to_needs_decision(&harness).await;
    let service = Arc::new(service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let log = harness.root.join("desk-access.log");
    let app = router(
        service.clone(),
        DeskConfig {
            origin: origin.clone(),
            access_log: log.clone(),
        },
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let profile = harness.root.join("browser-profile");
    let mut browser = tokio::process::Command::new(chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--no-proxy-server",
            &format!("--user-data-dir={}", profile.display()),
            &format!("{origin}/desk/b/{batch}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // The page script asks for a code; read it where the desk stored it, as a person would read it off the page.
    let database = rusqlite::Connection::open(harness.root.join("state.sqlite")).unwrap();
    let started = Instant::now();
    let code = loop {
        let code: Option<String> = database
            .query_row(
                "SELECT code FROM desk_sessions WHERE batch_id = ?1 AND code IS NOT NULL",
                [&batch],
                |row| row.get(0),
            )
            .ok();
        if let Some(code) = code {
            break code;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the page never asked for a code"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    service
        .receive_text(LINYI, ROOM, "$browser-pair", &code)
        .await
        .unwrap();
    // The unpaired page polls every 3 s and notices the pairing; from then on it keeps polling every 5 s.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let paired_at = state_requests(&log);
    tokio::time::sleep(Duration::from_secs(16)).await;
    let polls = state_requests(&log) - paired_at;
    browser.kill().await.unwrap();
    assert!(
        (3..=4).contains(&polls),
        "a paired page polls every 5 s: {polls} requests in 16 s"
    );
    let paired: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM desk_sessions WHERE batch_id = ?1 AND paired_user = ?2",
            rusqlite::params![batch, LINYI],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(paired, 1);
}
