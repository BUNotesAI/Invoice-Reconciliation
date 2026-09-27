//! Desk authorization and flow over real HTTP (design §9.3 must-test list).
mod common;

use std::sync::{Arc, atomic::Ordering};

use common::{
    Harness, INTRUDER, LINYI, ROOM, SECOND, SECOND_ROOM, ZHOUMIN, item_id, repo, texts,
    to_needs_decision,
};
use matrix_sdk::reqwest::{self, StatusCode};
use reimb_bot::{
    desk::{DeskConfig, router},
    service::Service,
};
use serde_json::{Value, json};

struct Desk {
    service: Arc<Service>,
    origin: String,
    batch: String,
    harness: Harness,
}

async fn desk() -> Desk {
    let harness = Harness::new();
    let (service, batch) = to_needs_decision(&harness).await;
    let service = Arc::new(service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = router(
        service.clone(),
        DeskConfig {
            origin: origin.clone(),
            access_log: harness.root.join("desk-access.log"),
        },
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Desk {
        service,
        origin,
        batch,
        harness,
    }
}

struct Browser {
    http: reqwest::Client,
    origin: String,
    cookie: Option<String>,
    csrf: Option<String>,
    revision: i64,
}

impl Browser {
    fn new(origin: &str) -> Self {
        Self {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            origin: origin.into(),
            cookie: None,
            csrf: None,
            revision: 0,
        }
    }

    async fn open(&mut self, batch: &str) -> StatusCode {
        let mut request = self.http.get(format!("{}/desk/b/{batch}", self.origin));
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", cookie);
        }
        let response = request.send().await.unwrap();
        if let Some(set) = response.headers().get("set-cookie") {
            let text = set.to_str().unwrap();
            assert!(
                text.contains("HttpOnly") && text.contains("SameSite=Strict"),
                "{text}"
            );
            self.cookie = Some(text.split(';').next().unwrap().to_string());
        }
        response.status()
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let mut request = self.http.get(format!("{}{path}", self.origin));
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", cookie);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (
            status,
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap_or(Value::Null),
        )
    }

    async fn state(&mut self, batch: &str) -> (StatusCode, Value) {
        let (status, body) = self.get(&format!("/desk/api/b/{batch}/state")).await;
        if body["paired"] == true {
            self.csrf = body["csrf"].as_str().map(String::from);
            self.revision = body["batch"]["revision"].as_i64().unwrap();
        }
        (status, body)
    }

    async fn post_with(
        &self,
        batch: &str,
        path: &str,
        body: Value,
        origin: Option<&str>,
        csrf: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .http
            .post(format!("{}/desk/api/b/{batch}/{path}", self.origin))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).unwrap());
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", cookie);
        }
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        if let Some(csrf) = csrf {
            request = request.header("x-csrf-token", csrf);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (
            status,
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap_or(Value::Null),
        )
    }

    async fn post(&self, batch: &str, path: &str, body: Value) -> (StatusCode, Value) {
        self.post_with(
            batch,
            path,
            body,
            Some(&self.origin.clone()),
            self.csrf.as_deref(),
        )
        .await
    }
}

async fn paired(desk: &Desk, user: &str, event: &str) -> Browser {
    paired_to(desk, &desk.batch, user, event).await
}

async fn paired_to(desk: &Desk, batch: &str, user: &str, event: &str) -> Browser {
    let mut browser = Browser::new(&desk.origin);
    assert_eq!(browser.open(batch).await, StatusCode::OK);
    let (_, state) = browser.state(batch).await;
    let code = state["code"].as_str().unwrap().to_string();
    desk.service
        .receive_text(user, ROOM, event, &code)
        .await
        .unwrap();
    browser.state(batch).await;
    browser
}

impl Browser {
    async fn code(&mut self, batch: &str) -> String {
        self.state(batch).await.1["code"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn status(&self, path: &str) -> StatusCode {
        self.get(path).await.0
    }
}

#[tokio::test]
async fn unpaired_page_shows_only_a_code() {
    let desk = desk().await;
    let mut browser = Browser::new(&desk.origin);
    browser.open(&desk.batch).await;
    let (status, state) = browser.state(&desk.batch).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["code", "expires_in", "paired"]
    );
    assert!(state["code"].as_str().unwrap().len() == 6);
    // Nothing without a session, and nothing for a batch that does not exist.
    assert_eq!(
        Browser::new(&desk.origin).state(&desk.batch).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        Browser::new(&desk.origin).open("bdoesnotexist").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_code_stays_the_same_while_the_page_polls() {
    // The page polls every few seconds; a code that changed on each poll could never be typed in time.
    let desk = desk().await;
    let mut browser = Browser::new(&desk.origin);
    browser.open(&desk.batch).await;
    let first = browser.state(&desk.batch).await.1["code"].clone();
    desk.harness.clock.fetch_add(30, Ordering::SeqCst);
    assert_eq!(browser.state(&desk.batch).await.1["code"], first);
    desk.harness.clock.fetch_add(600, Ordering::SeqCst);
    assert_ne!(
        browser.state(&desk.batch).await.1["code"],
        first,
        "an expired code is replaced"
    );
}

#[tokio::test]
async fn pairing_binds_the_chat_sender_once() {
    let desk = desk().await;
    let mut browser = Browser::new(&desk.origin);
    browser.open(&desk.batch).await;
    let code = browser.state(&desk.batch).await.1["code"]
        .as_str()
        .unwrap()
        .to_string();
    // A stranger sending the code cannot pair to someone else's batch.
    desk.service
        .receive_text(INTRUDER, "!intruder:reimb.local", "$p1", &code)
        .await
        .unwrap();
    assert_eq!(browser.state(&desk.batch).await.1["paired"], false);
    // The code shown is unchanged by the stranger's attempt; the applicant pairs with it.
    let code = browser.state(&desk.batch).await.1["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.service
        .receive_text(LINYI, ROOM, "$p2", &code)
        .await
        .unwrap();
    let (_, state) = browser.state(&desk.batch).await;
    assert_eq!(
        (state["paired"].clone(), state["role"].clone()),
        (json!(true), json!("applicant"))
    );
    // The used code cannot pair a second browser.
    let mut other = Browser::new(&desk.origin);
    other.open(&desk.batch).await;
    other.state(&desk.batch).await;
    desk.service
        .receive_text(LINYI, ROOM, "$p3", &code)
        .await
        .unwrap();
    assert_eq!(other.state(&desk.batch).await.1["paired"], false);
    assert!(
        texts(&desk.service, "-")
            .iter()
            .any(|t| t.contains("配对码无效"))
    );
}

#[tokio::test]
async fn too_many_wrong_codes_lock_the_sender() {
    let desk = desk().await;
    for attempt in 0..5 {
        desk.service
            .receive_text(LINYI, ROOM, &format!("$wrong{attempt}"), "000000")
            .await
            .unwrap();
    }
    let mut browser = Browser::new(&desk.origin);
    browser.open(&desk.batch).await;
    let code = browser.state(&desk.batch).await.1["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.service
        .receive_text(LINYI, ROOM, "$right", &code)
        .await
        .unwrap();
    assert_eq!(
        browser.state(&desk.batch).await.1["paired"],
        false,
        "locked even with the right code"
    );
    assert!(
        texts(&desk.service, "-")
            .iter()
            .any(|t| t.contains("错误次数过多"))
    );
    // Ten minutes later the sender may try again.
    desk.harness.clock.fetch_add(601, Ordering::SeqCst);
    let code = browser.state(&desk.batch).await.1["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.service
        .receive_text(LINYI, ROOM, "$later", &code)
        .await
        .unwrap();
    assert_eq!(browser.state(&desk.batch).await.1["paired"], true);
}

#[tokio::test]
async fn writes_need_origin_csrf_revision_and_the_applicant() {
    let desk = desk().await;
    let browser = paired(&desk, LINYI, "$pair").await;
    let item = item_id(&desk.service, &desk.batch, "F08.pdf");
    let body = json!({"expected_revision": browser.revision, "item_id": item, "kind": "explain_over_limit",
                      "payload": {"explanation": "会展"}});
    let csrf = browser.csrf.clone();
    assert_eq!(
        browser
            .post_with(&desk.batch, "decide", body.clone(), None, csrf.as_deref())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        browser
            .post_with(
                &desk.batch,
                "decide",
                body.clone(),
                Some("http://evil.example"),
                csrf.as_deref()
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        browser
            .post_with(
                &desk.batch,
                "decide",
                body.clone(),
                Some(&desk.origin),
                None
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        browser
            .post_with(
                &desk.batch,
                "decide",
                body.clone(),
                Some(&desk.origin),
                Some("0".repeat(64).as_str())
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut stale = body.clone();
    stale["expected_revision"] = json!(browser.revision - 1);
    assert_eq!(
        browser.post(&desk.batch, "decide", stale).await.0,
        StatusCode::CONFLICT
    );
    let mut extra = body.clone();
    extra["actor"] = json!(ZHOUMIN);
    assert_eq!(
        browser.post(&desk.batch, "decide", extra).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown fields are refused"
    );
    assert_eq!(
        browser.post(&desk.batch, "decide", body.clone()).await.0,
        StatusCode::OK
    );
    // The same request again is now stale.
    assert_eq!(
        browser.post(&desk.batch, "decide", body).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn finance_views_but_cannot_change_and_strangers_see_nothing() {
    let desk = desk().await;
    let finance = paired(&desk, ZHOUMIN, "$fin").await;
    let (status, state) = finance
        .get(&format!("/desk/api/b/{}/state", desk.batch))
        .await;
    assert_eq!(
        (status, state["role"].clone()),
        (StatusCode::OK, json!("viewer"))
    );
    let item = item_id(&desk.service, &desk.batch, "F08.pdf");
    let body = json!({"expected_revision": finance.revision, "item_id": item, "kind": "explain_over_limit", "payload": {"explanation": "x"}});
    assert_eq!(
        finance.post(&desk.batch, "decide", body).await.0,
        StatusCode::FORBIDDEN
    );
    // Finance may download the batch's sources.
    let files = desk
        .service
        .with_store(|store| store.files(&desk.batch))
        .unwrap();
    let source = format!("/desk/api/b/{}/source/{}", desk.batch, files[0].0);
    assert_eq!(finance.status(&source).await, StatusCode::OK);
    // A stranger's code is refused, their page stays unpaired and opens nothing.
    let mut stranger = Browser::new(&desk.origin);
    stranger.open(&desk.batch).await;
    let code = stranger.code(&desk.batch).await;
    desk.service
        .receive_text(INTRUDER, "!intruder:reimb.local", "$x", &code)
        .await
        .unwrap();
    let (status, state) = stranger.state(&desk.batch).await;
    assert_eq!(
        (status, state["paired"].clone()),
        (StatusCode::OK, json!(false))
    );
    assert!(state.get("report").is_none() && state.get("batch").is_none());
    assert_eq!(stranger.status(&source).await, StatusCode::UNAUTHORIZED);
    assert!(
        texts(&desk.service, "-")
            .iter()
            .any(|t| t.contains("不属于你"))
    );
    // A pairing whose user is no longer allowed on the batch is refused on every read.
    let mut reading = desk
        .service
        .with_store(|store| store.document(&desk.batch, "reading"))
        .unwrap()
        .unwrap();
    reading["finance"] = json!([]);
    desk.service
        .with_store(|store| {
            let work = store.begin()?;
            work.put_document(&desk.batch, "reading", &reading)?;
            work.commit()
        })
        .unwrap();
    assert_eq!(
        finance
            .status(&format!("/desk/api/b/{}/state", desk.batch))
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(finance.status(&source).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn sessions_and_downloads_stay_inside_their_batch() {
    let desk = desk().await;
    let browser = paired(&desk, LINYI, "$pair").await;
    // A second, real batch of another applicant, with its own paired session.
    let file = std::fs::read(repo().join("fixtures/demo/F01.pdf")).unwrap();
    desk.service
        .receive_file(SECOND, SECOND_ROOM, "$second-f01", "F01.pdf", &file)
        .await
        .unwrap();
    let second = desk
        .service
        .with_store(|store| store.open_batch_for(SECOND))
        .unwrap()
        .unwrap()
        .id;
    let other = paired_to(&desk, &second, SECOND, "$pair-second").await;
    assert_eq!(
        other.status(&format!("/desk/api/b/{second}/state")).await,
        StatusCode::OK
    );
    let mine = desk
        .service
        .with_store(|store| store.files(&desk.batch))
        .unwrap();
    let theirs = desk
        .service
        .with_store(|store| store.files(&second))
        .unwrap();
    // Each session opens nothing of the other batch: state, sources (even an id registered there), files.
    for (browser, batch, source) in [
        (&browser, second.as_str(), &theirs[0].0),
        (&other, desk.batch.as_str(), &mine[0].0),
    ] {
        assert_eq!(
            browser.status(&format!("/desk/api/b/{batch}/state")).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            browser
                .status(&format!("/desk/api/b/{batch}/source/{source}"))
                .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            browser
                .status(&format!("/desk/api/b/{batch}/files/x.pdf"))
                .await,
            StatusCode::FORBIDDEN
        );
    }
    // Inside its own batch a session reaches only ids registered to that batch.
    assert_eq!(
        browser
            .status(&format!(
                "/desk/api/b/{}/source/{}",
                desk.batch, theirs[0].0
            ))
            .await,
        if mine.iter().any(|(id, _)| *id == theirs[0].0) {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        }
    );
    assert_eq!(
        browser
            .status(&format!("/desk/api/b/{}/source/{}", desk.batch, mine[0].0))
            .await,
        StatusCode::OK
    );
    for probe in ["0".repeat(64), "..%2Fstate.sqlite".into(), "%2E%2E".into()] {
        let status = browser
            .status(&format!("/desk/api/b/{}/source/{probe}", desk.batch))
            .await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "{probe}: {status}"
        );
    }
    assert_eq!(
        browser
            .status(&format!("/desk/api/b/{}/files/state.sqlite", desk.batch))
            .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn live_codes_are_unique_and_consumed_once() {
    let desk = desk().await;
    let mut first = Browser::new(&desk.origin);
    first.open(&desk.batch).await;
    let code = first.code(&desk.batch).await;
    let mut second = Browser::new(&desk.origin);
    second.open(&desk.batch).await;
    second.code(&desk.batch).await;
    let now = desk.service.now();
    let sessions: Vec<String> = desk
        .service
        .with_store(|store| {
            let hash = reimb_bot::store::sha256_hex(code.as_bytes());
            let holder = store.session_by_code(&hash, now)?.unwrap().id_hash;
            let mut all = Vec::new();
            for browser in [&first, &second] {
                let raw = browser.cookie.as_ref().unwrap().split('=').nth(1).unwrap();
                all.push(reimb_bot::store::sha256_hex(raw.as_bytes()));
            }
            assert_eq!(holder, all[0]);
            Ok::<_, rusqlite::Error>(all)
        })
        .unwrap();
    // The live code of one session cannot be given to another: the store refuses the clash.
    let clash = desk
        .service
        .with_store(|store| {
            let work = store.begin()?;
            let issued = work.issue_code(
                &sessions[1],
                &code,
                &reimb_bot::store::sha256_hex(code.as_bytes()),
                now + 600,
                now,
            )?;
            work.commit()?;
            Ok::<_, rusqlite::Error>(issued)
        })
        .unwrap();
    assert!(!clash);
    // The code pairs its own session and nothing else; a second claim of the same code finds nothing.
    desk.service
        .receive_text(LINYI, ROOM, "$pair", &code)
        .await
        .unwrap();
    assert_eq!(first.state(&desk.batch).await.1["paired"], true);
    assert_eq!(second.state(&desk.batch).await.1["paired"], false);
    let again = desk
        .service
        .with_store(|store| {
            let work = store.begin()?;
            work.claim_code(
                &sessions[0],
                &reimb_bot::store::sha256_hex(code.as_bytes()),
                ZHOUMIN,
                now + 3600,
                now,
            )
        })
        .unwrap();
    assert!(!again);
    // Every outcome is in the pairing audit.
    let audit = desk
        .service
        .with_store(|store| store.pairing_audit(&desk.batch))
        .unwrap();
    assert_eq!(audit, vec![(LINYI.to_string(), "paired".to_string())]);
}

#[tokio::test]
async fn five_wrong_codes_withdraw_every_live_code() {
    // Wrong codes from five different senders: no sender reaches its own limit, yet each live code has now
    // absorbed five wrong attempts and is withdrawn (design §9.3 "每码最多 5 次错误尝试，超限作废").
    let desk = desk().await;
    let mut browser = Browser::new(&desk.origin);
    browser.open(&desk.batch).await;
    let code = browser.code(&desk.batch).await;
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for index in 0..5 {
        desk.service
            .receive_text(
                &format!("@guess{index}:reimb.local"),
                "!guess:reimb.local",
                &format!("$g{index}"),
                wrong,
            )
            .await
            .unwrap();
    }
    desk.service
        .receive_text(LINYI, ROOM, "$late", &code)
        .await
        .unwrap();
    assert_eq!(browser.state(&desk.batch).await.1["paired"], false);
    // The page draws a fresh code, which pairs.
    let fresh = browser.code(&desk.batch).await;
    desk.service
        .receive_text(LINYI, ROOM, "$fresh", &fresh)
        .await
        .unwrap();
    assert_eq!(browser.state(&desk.batch).await.1["paired"], true);
    let results: Vec<String> = desk
        .service
        .with_store(|store| store.pairing_audit(&desk.batch))
        .unwrap()
        .into_iter()
        .map(|(_, result)| result)
        .collect();
    assert_eq!(results, ["paired"]);
    let unknown = desk
        .service
        .with_store(|store| store.pairing_audit("-"))
        .unwrap();
    assert_eq!(
        unknown.len(),
        6,
        "five guesses and the withdrawn code: {unknown:?}"
    );
}

#[tokio::test]
async fn one_batch_holds_a_bounded_number_of_unpaired_sessions() {
    let desk = desk().await;
    for _ in 0..20 {
        assert_eq!(
            Browser::new(&desk.origin).open(&desk.batch).await,
            StatusCode::OK
        );
    }
    assert_eq!(
        Browser::new(&desk.origin).open(&desk.batch).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Sessions older than their cookie are dropped, which frees room.
    desk.harness.clock.fetch_add(3601, Ordering::SeqCst);
    assert_eq!(
        Browser::new(&desk.origin).open(&desk.batch).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn every_route_answers_only_to_the_desk_host() {
    let desk = desk().await;
    let browser = paired(&desk, LINYI, "$pair").await;
    let port = desk.origin.rsplit(':').next().unwrap();
    for path in [
        format!("/desk/b/{}", desk.batch),
        format!("/desk/api/b/{}/state", desk.batch),
        "/health".to_string(),
    ] {
        let response = browser
            .http
            .get(format!("{}{path}", desk.origin))
            .header("host", format!("rebound.example:{port}"))
            .header("cookie", browser.cookie.clone().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    }
    assert_eq!(
        browser
            .status(&format!("/desk/api/b/{}/state", desk.batch))
            .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn expired_pairing_needs_a_new_code() {
    let desk = desk().await;
    let mut browser = paired(&desk, LINYI, "$pair").await;
    desk.harness.clock.fetch_add(3601, Ordering::SeqCst);
    let item = item_id(&desk.service, &desk.batch, "F08.pdf");
    let body = json!({"expected_revision": browser.revision, "item_id": item, "kind": "explain_over_limit", "payload": {"explanation": "x"}});
    assert_eq!(
        browser.post(&desk.batch, "decide", body).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(browser.state(&desk.batch).await.1["paired"], false);
}

#[tokio::test]
async fn whole_flow_through_the_desk_and_a_clean_access_log() {
    let desk = desk().await;
    let mut browser = paired(&desk, LINYI, "$pair").await;
    let (_, state) = browser.state(&desk.batch).await;
    let f09 = item_id(&desk.service, &desk.batch, "F09.pdf");
    assert!(state["visual_checks"][&f09]["fields"]["invoice_no"].is_string());
    assert_eq!(
        browser
            .post(
                &desk.batch,
                "confirm-visual",
                json!({"expected_revision": browser.revision, "item_id": f09})
            )
            .await
            .0,
        StatusCode::OK
    );
    browser.state(&desk.batch).await;
    let (_, state) = browser.state(&desk.batch).await;
    let shot = state["screenshots"][0]["source_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        browser
            .post(
                &desk.batch,
                "confirm-screenshot",
                json!({"expected_revision": browser.revision, "source_id": shot})
            )
            .await
            .0,
        StatusCode::OK
    );
    for (file, kind, payload) in [
        (
            "F08.pdf",
            "explain_over_limit",
            json!({"explanation": "会展期间"}),
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
        browser.state(&desk.batch).await;
        let item = item_id(&desk.service, &desk.batch, file);
        let (status, body) = browser.post(&desk.batch, "decide", json!({"expected_revision": browser.revision, "item_id": item, "kind": kind, "payload": payload})).await;
        assert_eq!(status, StatusCode::OK, "{file}: {body}");
    }
    let (_, state) = browser.state(&desk.batch).await;
    assert_eq!(state["batch"]["state"], "awaiting_confirm");
    assert_eq!(
        browser
            .post(
                &desk.batch,
                "confirm",
                json!({"expected_revision": browser.revision})
            )
            .await
            .0,
        StatusCode::OK
    );
    let (_, state) = browser.state(&desk.batch).await;
    assert_eq!(state["batch"]["state"], "ready_to_share");
    let ledger = state["published"]
        .as_array()
        .unwrap()
        .iter()
        .find(|name| name.as_str().unwrap().ends_with(".xlsx"))
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let response = browser
        .http
        .get(format!(
            "{}/desk/api/b/{}/files/{}",
            desk.origin,
            desk.batch,
            urlencode(&ledger)
        ))
        .header("cookie", browser.cookie.clone().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.bytes().await.unwrap().starts_with(b"PK"));
    // Finance, paired with their own account, downloads the published ledger too.
    let finance = paired(&desk, ZHOUMIN, "$fin").await;
    assert_eq!(
        finance
            .status(&format!(
                "/desk/api/b/{}/files/{}",
                desk.batch,
                urlencode(&ledger)
            ))
            .await,
        StatusCode::OK
    );
    let log = std::fs::read_to_string(desk.harness.root.join("desk-access.log")).unwrap();
    let secret = browser.cookie.unwrap();
    let secret = secret.split('=').nth(1).unwrap();
    assert!(
        !log.contains(secret)
            && !log.contains(browser.csrf.as_deref().unwrap())
            && !log.contains("会展期间")
    );
    assert!(log.lines().all(|line| {
        serde_json::from_str::<Value>(line)
            .unwrap()
            .as_object()
            .unwrap()
            .len()
            == 5
    }));
}

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
