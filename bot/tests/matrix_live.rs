//! Script steps 2-14 over the real local Palpo: the bot in-process, 林一, 周敏 (finance) and a stranger as scripted
//! Matrix clients.
//!
//! Opt-in: `REIMB_LIVE=1 cargo test --test matrix_live -- --nocapture`. Needs `python3 scripts/dev_env.py` first;
//! reads account tokens from `$REIMB_DATA/credentials.json` (default ~/.reimb-demo) and never prints them.
mod common;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use common::{honest, repo, scripted};
use matrix_sdk::reqwest::{self, StatusCode};
use reimb_bot::{
    desk::{DeskConfig, router},
    matrix,
    service::{Service, ServiceConfig},
    store::Store,
};
use serde_json::{Value, json};

const HOMESERVER: &str = "http://127.0.0.1:18128";

fn credentials() -> Value {
    let root = std::env::var_os("REIMB_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join(".reimb-demo"));
    serde_json::from_slice(
        &std::fs::read(root.join("credentials.json")).expect("run scripts/dev_env.py first"),
    )
    .unwrap()
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

struct Person {
    http: reqwest::Client,
    token: String,
    room: String,
}

impl Person {
    fn new(credentials: &Value, account: &str, room: &str) -> Self {
        Self {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            token: credentials["accounts"][account]["access_token"]
                .as_str()
                .unwrap()
                .to_string(),
            room: credentials["rooms"][room].as_str().unwrap().to_string(),
        }
    }

    async fn send(&self, content: Value) {
        let txn = uuid::Uuid::new_v4().simple().to_string();
        let url = format!(
            "{HOMESERVER}/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
            encode(&self.room)
        );
        let response = self
            .http
            .put(url)
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&content).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn text(&self, body: &str) {
        self.send(json!({"msgtype": "m.text", "body": body})).await;
    }

    async fn file(&self, name: &str, bytes: Vec<u8>) {
        let size = bytes.len();
        let url = format!(
            "{HOMESERVER}/_matrix/media/v3/upload?filename={}",
            encode(name)
        );
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let uri: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        self.send(
            json!({"msgtype": "m.file", "body": name, "filename": name, "url": uri["content_uri"],
                         "info": {"size": size, "mimetype": "application/octet-stream"}}),
        )
        .await;
    }

    /// Bot messages in this room since `since` (ms), oldest first.
    async fn bot_messages(&self, bot: &str, since: u64) -> Vec<Value> {
        let url = format!(
            "{HOMESERVER}/_matrix/client/v3/rooms/{}/messages?dir=b&limit=200",
            encode(&self.room)
        );
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        let page: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        let mut events: Vec<Value> = page["chunk"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| {
                e["sender"] == bot
                    && e["type"] == "m.room.message"
                    && e["origin_server_ts"].as_u64().unwrap_or(0) >= since
            })
            .collect();
        events.reverse();
        events
    }

    async fn wait_for(
        &self,
        bot: &str,
        since: u64,
        what: &str,
        check: impl Fn(&Value) -> bool,
    ) -> Value {
        let started = Instant::now();
        loop {
            if let Some(found) = self
                .bot_messages(bot, since)
                .await
                .into_iter()
                .find(|e| check(&e["content"]))
            {
                return found;
            }
            if started.elapsed() > Duration::from_secs(180) {
                let seen: Vec<String> = self
                    .bot_messages(bot, since)
                    .await
                    .iter()
                    .map(|e| e["content"]["body"].to_string())
                    .collect();
                panic!("timed out waiting for {what}; bot said: {seen:#?}");
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    }
}

fn body_contains(text: &'static str) -> impl Fn(&Value) -> bool {
    move |content: &Value| {
        content["body"]
            .as_str()
            .is_some_and(|body| body.contains(text))
    }
}

struct Browser {
    http: reqwest::Client,
    origin: String,
    batch: String,
    cookie: String,
    csrf: String,
    revision: i64,
}

impl Browser {
    async fn open(origin: &str, batch: &str) -> Self {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = http
            .get(format!("{origin}/desk/b/{batch}"))
            .send()
            .await
            .unwrap();
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        Self {
            http,
            origin: origin.into(),
            batch: batch.into(),
            cookie,
            csrf: String::new(),
            revision: 0,
        }
    }

    async fn state(&mut self) -> Value {
        let response = self
            .http
            .get(format!("{}/desk/api/b/{}/state", self.origin, self.batch))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        let state: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        if state["paired"] == true {
            self.csrf = state["csrf"].as_str().unwrap().to_string();
            self.revision = state["batch"]["revision"].as_i64().unwrap();
        }
        state
    }

    async fn post(&self, path: &str, mut body: Value) -> StatusCode {
        body["expected_revision"] = json!(self.revision);
        self.http
            .post(format!("{}/desk/api/b/{}/{path}", self.origin, self.batch))
            .header("cookie", &self.cookie)
            .header("origin", &self.origin)
            .header("x-csrf-token", &self.csrf)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).unwrap())
            .send()
            .await
            .unwrap()
            .status()
    }
}

#[tokio::test]
async fn script_over_real_matrix() {
    if std::env::var("REIMB_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set REIMB_LIVE=1 with the local Palpo running");
        return;
    }
    let credentials = credentials();
    let bot_user = credentials["accounts"]["reimb-bot"]["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let linyi_id = credentials["accounts"]["reimb-linyi"]["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let linyi = Person::new(&credentials, "reimb-linyi", "applicant_bot");
    let stranger = Person::new(&credentials, "reimb-intruder", "intruder_bot");
    let zhoumin_id = credentials["accounts"]["reimb-zhoumin"]["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let zhoumin = Person::new(&credentials, "reimb-zhoumin", "finance_bot");
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(repo().join("policy/example.yaml"), root.join("policy.yaml")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let service = Arc::new(Service::new(
        ServiceConfig {
            data_root: root.clone(),
            policy: root.join("policy.yaml"),
            python: repo().join(".venv/bin/python"),
            core_dir: repo().join("core"),
            history: serde_json::from_slice(
                &std::fs::read(repo().join("fixtures/demo/history.json")).unwrap(),
            )
            .unwrap(),
            period: "2026-10".into(),
            desk_url: origin.clone(),
            applicants: BTreeMap::from([(linyi_id.clone(), linyi.room.clone())]),
            finance_rooms: BTreeMap::from([(zhoumin_id.clone(), zhoumin.room.clone())]),
        },
        Store::open(&root.join("state.sqlite")).unwrap(),
        scripted(honest),
        Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
        }),
    ));
    let password = credentials["accounts"]["reimb-bot"]["password"]
        .as_str()
        .unwrap();
    let session_file = root.join("bot").join("matrix-session.json");
    let client = matrix::connect(HOMESERVER, &bot_user, password, &session_file)
        .await
        .unwrap();
    let device = client.device_id().unwrap().to_owned();
    // A second start reuses the saved device instead of logging in again (F2).
    let again = matrix::connect(HOMESERVER, &bot_user, "not-the-password", &session_file)
        .await
        .unwrap();
    assert_eq!(again.device_id().unwrap(), &*device);
    tokio::spawn(
        axum::serve(
            listener,
            router(
                service.clone(),
                DeskConfig {
                    origin: origin.clone(),
                    access_log: root.join("access.log"),
                },
            ),
        )
        .into_future(),
    );
    let first_sync = tokio::spawn(matrix::run_sync(client.clone(), service.clone()));
    let first_outbox = tokio::spawn(matrix::run_outbox(client.clone(), service.clone()));
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Step 2: files in chat; the stranger is refused.
    stranger
        .file(
            "F01.pdf",
            std::fs::read(repo().join("fixtures/demo/F01.pdf")).unwrap(),
        )
        .await;
    stranger
        .wait_for(
            &bot_user,
            since,
            "stranger refusal",
            body_contains("你不在报销名单里"),
        )
        .await;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(repo().join("fixtures/demo"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    paths.retain(|p| p.file_name().unwrap() != "history.json");
    for path in &paths {
        linyi
            .file(
                path.file_name().unwrap().to_str().unwrap(),
                std::fs::read(path).unwrap(),
            )
            .await;
    }
    let count = paths.len();
    linyi
        .wait_for(&bot_user, since, "all files received", move |c| {
            c["body"]
                .as_str()
                .is_some_and(|b| b.contains(&format!("本批次共 {count} 个文件")))
        })
        .await;

    // Steps 3-6: seal, read, gate, link, report card and desk card.
    linyi.text("开始对账").await;
    let report = linyi
        .wait_for(&bot_user, since, "report", body_contains("待你判断"))
        .await;
    let body = report["content"]["body"].as_str().unwrap();
    assert!(
        body.contains("自动匹配 6 张，合计 ¥1,475.40")
            && body.contains("待你判断 4 张，合计 ¥3,426.00")
            && body.contains("拒收 2 张"),
        "{body}"
    );
    assert_eq!(report["content"]["format"], "org.matrix.custom.html");
    let card = linyi
        .wait_for(&bot_user, since, "desk card", |c| {
            c["msgtype"] == "rs.robius.robrix.mini_app"
        })
        .await;
    let url = card["content"]["mini_app"]["url"]
        .as_str()
        .unwrap()
        .to_string();
    let batch = url.rsplit('/').next().unwrap().to_string();
    assert!(
        url.starts_with(&origin) && !url.contains("token") && !url.contains('?'),
        "{url}"
    );

    // Step 7: pairing in chat; the stranger cannot use someone else's code.
    let mut desk = Browser::open(&origin, &batch).await;
    let code = desk.state().await["code"].as_str().unwrap().to_string();
    stranger.text(&code).await;
    stranger
        .wait_for(
            &bot_user,
            since,
            "stranger pairing refusal",
            body_contains("不属于你"),
        )
        .await;
    let code = desk.state().await["code"].as_str().unwrap().to_string();
    linyi.text(&code).await;
    linyi
        .wait_for(&bot_user, since, "pairing", body_contains("配对成功"))
        .await;
    let state = desk.state().await;
    assert_eq!(state["role"], "applicant");

    // Steps 8-9: visual check, screenshot evidence, decisions, each bound to the current revision.
    let items = state["report"]["items"].as_array().unwrap().clone();
    let id_of = |file: &str| {
        items.iter().find(|i| i["file"] == file).unwrap()["item_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        desk.post("confirm-visual", json!({"item_id": id_of("F09.pdf")}))
            .await,
        StatusCode::OK
    );
    let state = desk.state().await;
    let shot = state["screenshots"][0]["source_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        desk.post("confirm-screenshot", json!({"source_id": shot}))
            .await,
        StatusCode::OK
    );
    desk.state().await;
    for (file, kind, payload) in [
        (
            "F08.pdf",
            "explain_over_limit",
            json!({"explanation": "会展期间协议价上浮"}),
        ),
        (
            "F09.pdf",
            "explain_over_limit",
            json!({"explanation": "客户接待"}),
        ),
        (
            "F10.pdf",
            "replace_unpaid_invoice",
            json!({"invoice_no": "26112000000300002208"}),
        ),
    ] {
        assert_eq!(
            desk.post(
                "decide",
                json!({"item_id": id_of(file), "kind": kind, "payload": payload})
            )
            .await,
            StatusCode::OK,
            "{file}"
        );
        desk.state().await;
    }
    linyi
        .wait_for(
            &bot_user,
            since,
            "all decided",
            body_contains("所有待判断项都处理完了"),
        )
        .await;

    // Steps 10-11: confirmation at the current revision, package, final review, publish, post.
    let stale = Browser {
        revision: desk.revision - 1,
        ..Browser::open(&origin, &batch).await
    };
    assert!(
        stale.post("confirm", json!({})).await.is_client_error(),
        "an unpaired or stale browser cannot confirm"
    );
    assert_eq!(desk.post("confirm", json!({})).await, StatusCode::OK);
    let result = linyi
        .wait_for(&bot_user, since, "result", body_contains("终审全部通过"))
        .await;
    let text = result["content"]["body"].as_str().unwrap();
    assert!(
        text.contains("（7/7）") && text.contains("10 张发票") && text.contains("¥4,901.40"),
        "{text}"
    );

    // Outbox: resending an already sent message with its stored transaction id adds nothing to the room.
    let before = linyi
        .bot_messages(&bot_user, since)
        .await
        .iter()
        .filter(|e| body_contains("终审全部通过")(&e["content"]))
        .count();
    let pending = service.with_store(|store| store.outbox(&batch)).unwrap();
    let (txn, _, _) = pending
        .iter()
        .find(|(_, content, _)| body_contains("终审全部通过")(content))
        .unwrap()
        .clone();
    let connection = rusqlite::Connection::open(root.join("state.sqlite")).unwrap();
    connection
        .execute(
            "UPDATE outbox SET sent_event = NULL WHERE txn_id = ?1",
            [txn],
        )
        .unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after = linyi
        .bot_messages(&bot_user, since)
        .await
        .iter()
        .filter(|e| body_contains("终审全部通过")(&e["content"]))
        .count();
    assert_eq!((before, after), (1, 1));

    // Restart: a new client restored from the saved session must reach the quiet room and send queued replies,
    // and handle a message that arrived while the bot was down.
    // Stop the first bot completely: its tasks hold their own client handles.
    first_sync.abort();
    first_outbox.abort();
    let _ = first_sync.await;
    let _ = first_outbox.await;
    drop(client);
    connection
        .execute(
            "UPDATE outbox SET sent_event = NULL WHERE txn_id = ?1",
            [pending.last().unwrap().0.clone()],
        )
        .unwrap();
    let restarted = matrix::connect(HOMESERVER, &bot_user, "not-the-password", &session_file)
        .await
        .unwrap();
    // No new activity in the room yet: only the warm-up sync can make the quiet room known.
    tokio::spawn(matrix::run_sync(restarted.clone(), service.clone()));
    let started = Instant::now();
    loop {
        let unsent = service.with_store(|store| store.pending_outbox()).unwrap();
        if unsent.is_empty() {
            break;
        }
        matrix::send_outbox(&restarted, &service).await;
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "queued replies stayed unsent after restart: {}",
            unsent.len()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tokio::spawn(matrix::run_outbox(restarted.clone(), service.clone()));
    linyi.text("重试").await;
    linyi
        .wait_for(
            &bot_user,
            since,
            "reply after the restart",
            body_contains("当前没有需要重试的步骤"),
        )
        .await;

    // Step 12: 林一 shares with finance; 周敏 gets a notice and a card in her own room and pairs with her own account.
    desk.state().await;
    assert_eq!(desk.post("submit", json!({})).await, StatusCode::OK);
    linyi
        .wait_for(&bot_user, since, "submitted", body_contains("已提交给财务"))
        .await;
    zhoumin
        .wait_for(
            &bot_user,
            since,
            "finance notice",
            body_contains("请在对账台审核"),
        )
        .await;
    let card = zhoumin
        .wait_for(&bot_user, since, "finance card", |c| {
            c["msgtype"] == "rs.robius.robrix.mini_app"
        })
        .await;
    assert_eq!(card["content"]["mini_app"]["url"].as_str().unwrap(), url);
    let mut finance = Browser::open(&origin, &batch).await;
    let code = finance.state().await["code"].as_str().unwrap().to_string();
    zhoumin.text(&code).await;
    zhoumin
        .wait_for(
            &bot_user,
            since,
            "finance pairing",
            body_contains("配对成功"),
        )
        .await;
    let state = finance.state().await;
    assert_eq!(
        (state["role"].clone(), state["user"].clone()),
        (json!("finance"), json!(zhoumin_id))
    );
    // 周敏 has none of 林一's rights: she cannot decide or confirm.
    assert_eq!(
        finance.post("confirm", json!({})).await,
        StatusCode::FORBIDDEN
    );

    // Step 13: finance returns one item, which makes a new revision.
    let submitted = finance.revision;
    let f08 = id_of("F08.pdf");
    assert_eq!(
        finance
            .post(
                "return",
                json!({"items": {f08.clone(): "请补充住宿的事由和人数"}})
            )
            .await,
        StatusCode::OK
    );
    linyi
        .wait_for(
            &bot_user,
            since,
            "return notice",
            body_contains("财务退回了 1 项"),
        )
        .await;

    // Step 14: 林一 supplements from the rule template, confirms the rebuilt revision and resubmits; finance approves.
    desk.state().await;
    assert!(desk.revision > submitted);
    assert_eq!(
        desk.post(
            "supplement",
            json!({"item_id": f08, "people": 3, "purpose": "客户会展接待"})
        )
        .await,
        StatusCode::OK
    );
    desk.state().await;
    assert_eq!(desk.post("confirm", json!({})).await, StatusCode::OK);
    desk.state().await;
    assert_eq!(desk.post("submit", json!({})).await, StatusCode::OK);
    finance.state().await;
    assert_eq!(finance.post("approve", json!({})).await, StatusCode::OK);
    linyi
        .wait_for(
            &bot_user,
            since,
            "approval",
            body_contains("财务已审批通过"),
        )
        .await;
}
