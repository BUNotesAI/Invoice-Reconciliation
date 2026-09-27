//! P0 transport probe. Reimbursement mutations are introduced in later slices.
use std::{collections::HashMap, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use axum::{Router, response::Html, routing::get};
use matrix_sdk::{
    Client, Room,
    config::SyncSettings,
    ruma::{
        OwnedUserId,
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
        },
    },
};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct CredentialFile {
    accounts: HashMap<String, Account>,
}

#[derive(Deserialize)]
struct Account {
    password: String,
    user_id: OwnedUserId,
}

const DESK_URL: &str = "http://127.0.0.1:8787/desk/b/p0-demo";

async fn echo(event: OriginalSyncRoomMessageEvent, room: Room) {
    if event.sender == room.own_user_id() || event.sender.as_str() != "@reimb-linyi:reimb.local" {
        return;
    }
    let MessageType::Text(text) = event.content.msgtype else {
        return;
    };
    if text.body != "开始对账" && text.body != "P0 echo" {
        return;
    }
    // P0 uses fixed synthetic text. No user text is interpolated into HTML.
    let notice = RoomMessageEventContent::notice_html(
        "收到测试消息。P0 连接检查通过，请打开对账台测试卡片。",
        "<b>收到测试消息</b><br>这是一条格式化消息。<ul><li>P0 连接检查通过</li></ul>",
    );
    if room.send(notice).await.is_err() {
        eprintln!("Failed to send echo notice");
        return;
    }
    let content = json!({
        "msgtype": "rs.robius.robrix.mini_app",
        "body": format!("[Mini app] 报销对账台\n{DESK_URL}"),
        "mini_app": {"version": 1, "title": "报销对账台", "url": DESK_URL}
    });
    if room.send_raw("m.room.message", content).await.is_err() {
        eprintln!("Failed to send mini app card");
    }
}

async fn run() -> Result<()> {
    let data = std::env::var_os("REIMB_DATA")
        .map(PathBuf::from)
        .context("REIMB_DATA must name the local runtime directory")?;
    let bytes = tokio::fs::read(data.join("credentials.json"))
        .await
        .context("Cannot read local credentials file")?;
    let credentials: CredentialFile =
        serde_json::from_slice(&bytes).context("Invalid local credentials schema")?;
    let account = credentials
        .accounts
        .get("reimb-bot")
        .context("Missing bot account")?;
    let client = Client::builder()
        .homeserver_url("http://127.0.0.1:18128")
        .build()
        .await
        .map_err(|_| anyhow::anyhow!("Matrix client initialization failed"))?;
    client
        .matrix_auth()
        .login_username(&account.user_id, &account.password)
        .initial_device_display_name("Reimbursement P0 bot")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Matrix login failed"))?;
    client
        .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
        .await
        .map_err(|_| anyhow::anyhow!("Initial Matrix sync failed"))?;
    client.add_event_handler(echo);
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/desk/b/p0-demo",
            get(|| async { Html(include_str!("../../desk/p0.html")) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8787").await?;
    println!("P0 bot ready; desk listening on 127.0.0.1:8787");
    tokio::select! {
        result = axum::serve(listener, router) => result.context("Desk server stopped"),
        result = client.sync(SyncSettings::default()) => result.map_err(|_| anyhow::anyhow!("Matrix sync stopped")),
        result = tokio::signal::ctrl_c() => result.context("Signal handler failed"),
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
