//! The reconciliation desk: a web page opened from the chat card, paired to a Matrix identity through the chat.
//!
//! The page URL carries only the batch id. A browser session is a high-entropy HttpOnly cookie; before pairing it
//! sees a code and nothing else. Every read and write re-checks the paired user against the batch, writes also
//! check Host, Origin, a CSRF token and the expected revision. The access log never records cookies or bodies.
// Handlers return an early `Response` as their error; it is built once per request, so its size does not matter.
#![allow(clippy::result_large_err)]

use std::{io::Write, path::PathBuf, sync::Arc, time::Instant};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    service::{Service, ServiceError},
    store::{DeskSession, sha256_hex},
};

pub const COOKIE: &str = "reimb_desk";
const CODE_SECONDS: i64 = 600;

#[derive(Clone)]
pub struct DeskConfig {
    /// Exact origin the page is served from, e.g. http://127.0.0.1:8787; writes from any other origin are refused.
    pub origin: String,
    pub access_log: PathBuf,
}

#[derive(Clone)]
struct Desk {
    service: Arc<Service>,
    config: DeskConfig,
}

pub fn router(service: Arc<Service>, config: DeskConfig) -> Router {
    let desk = Desk { service, config };
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/desk/b/{batch}", get(page))
        .route("/desk/api/b/{batch}/state", get(state))
        .route("/desk/api/b/{batch}/source/{source}", get(source))
        .route("/desk/api/b/{batch}/files/{name}", get(file))
        .route("/desk/api/b/{batch}/decide", post(decide))
        .route("/desk/api/b/{batch}/confirm-visual", post(confirm_visual))
        .route(
            "/desk/api/b/{batch}/confirm-screenshot",
            post(confirm_screenshot),
        )
        .route("/desk/api/b/{batch}/confirm", post(confirm))
        .route("/desk/api/b/{batch}/decline", post(decline))
        .layer(middleware::from_fn_with_state(desk.clone(), access_log))
        .with_state(desk)
}

/// One JSON line per request: time, method, path, status, duration. No query, headers, cookies or bodies.
async fn access_log(State(desk): State<Desk>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    let line = json!({"ts": desk.service.now(), "method": method, "path": path, "status": response.status().as_u16(),
                      "ms": started.elapsed().as_millis() as u64});
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&desk.config.access_log)
    {
        let _ = writeln!(log, "{line}");
    }
    response
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"error": code}))).into_response()
}

fn service_error(err: ServiceError) -> Response {
    match err {
        ServiceError::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        ServiceError::NotFound => error(StatusCode::NOT_FOUND, "not_found"),
        ServiceError::StaleRevision(current) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "stale_revision", "revision": current})),
        )
            .into_response(),
        ServiceError::Invalid(why) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid", "detail": why})),
        )
            .into_response(),
        ServiceError::Core(_) | ServiceError::Store(_) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.to_string())
        .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn session_for(desk: &Desk, headers: &HeaderMap, batch: &str) -> Result<DeskSession, Response> {
    let raw = cookie_value(headers).ok_or_else(|| error(StatusCode::UNAUTHORIZED, "no_session"))?;
    let session = desk
        .service
        .with_store(|store| store.desk_session(&sha256_hex(raw.as_bytes())))
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "internal"))?
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "no_session"))?;
    if session.batch_id != batch {
        return Err(error(StatusCode::FORBIDDEN, "other_batch"));
    }
    Ok(session)
}

/// The paired user, if the pairing is still valid and that user may see this batch.
fn viewer(desk: &Desk, session: &DeskSession) -> Result<Option<String>, Response> {
    let now = desk.service.now();
    let Some(user) = session
        .paired_user
        .clone()
        .filter(|_| session.paired_until.is_some_and(|until| until > now))
    else {
        return Ok(None);
    };
    let batch = desk
        .service
        .batch(&session.batch_id)
        .map_err(service_error)?;
    if !desk
        .service
        .may_view(&batch, &user)
        .map_err(service_error)?
    {
        return Err(error(StatusCode::FORBIDDEN, "forbidden"));
    }
    Ok(Some(user))
}

fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn pairing_code() -> String {
    let random = u128::from_be_bytes(*uuid::Uuid::new_v4().as_bytes());
    format!("{:06}", random % 1_000_000)
}

/// The page itself holds no batch data. A browser without a session for this batch gets a new one.
async fn page(State(desk): State<Desk>, Path(batch): Path<String>, headers: HeaderMap) -> Response {
    if desk.service.batch(&batch).is_err() {
        return error(StatusCode::NOT_FOUND, "not_found");
    }
    let mut response = Html(include_str!("../../desk/app.html")).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; img-src 'self' blob:; style-src 'unsafe-inline'; script-src 'unsafe-inline'; frame-ancestors 'none'"),
    );
    if session_for(&desk, &headers, &batch).is_ok() {
        return response;
    }
    let raw = random_token();
    let session = DeskSession {
        id_hash: sha256_hex(raw.as_bytes()),
        csrf: random_token(),
        batch_id: batch,
        code_hash: None,
        code_expires_at: None,
        paired_user: None,
        paired_until: None,
    };
    if desk
        .service
        .with_store(|store| {
            store.begin().and_then(|work| {
                work.put_session(&session)?;
                work.commit()
            })
        })
        .is_err()
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    }
    let cookie = format!("{COOKIE}={raw}; HttpOnly; SameSite=Strict; Path=/desk; Max-Age=3600");
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie is ASCII"),
    );
    response
}

async fn state(
    State(desk): State<Desk>,
    Path(batch): Path<String>,
    headers: HeaderMap,
) -> Response {
    let mut session = match session_for(&desk, &headers, &batch) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let user = match viewer(&desk, &session) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let now = desk.service.now();
    let Some(user) = user else {
        // Unpaired: a fresh one-time code for this session, nothing about the batch.
        let code = pairing_code();
        session.code_hash = Some(sha256_hex(code.as_bytes()));
        session.code_expires_at = Some(now + CODE_SECONDS);
        session.paired_user = None;
        session.paired_until = None;
        if desk
            .service
            .with_store(|store| {
                store.begin().and_then(|work| {
                    work.put_session(&session)?;
                    work.commit()
                })
            })
            .is_err()
        {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
        return Json(json!({"paired": false, "code": code, "expires_in": CODE_SECONDS}))
            .into_response();
    };
    let batch_row = match desk.service.batch(&batch) {
        Ok(batch) => batch,
        Err(err) => return service_error(err),
    };
    let assessment = desk.service.assessment(&batch).ok().flatten();
    let reading = desk
        .service
        .with_store(|store| store.document(&batch, "reading"))
        .ok()
        .flatten();
    let screenshots: Vec<Value> = reading
        .as_ref()
        .and_then(|r| r["screenshots"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|shot| !shot["fields"].is_null())
        .map(|shot| json!({"source_id": shot["source_id"], "file": shot["file"], "fields": shot["fields"]}))
        .collect();
    let candidates: Map<String, Value> = reading
        .as_ref()
        .and_then(|r| r["items"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|item| item["invoice"]["invoice_no"]["level"] == "candidate")
        .filter_map(|item| {
            let invoice = item["invoice"].as_object()?;
            let fields: Map<String, Value> = invoice
                .iter()
                .filter(|(_, fact)| fact.get("level").is_some())
                .map(|(name, fact)| (name.clone(), fact["value"].clone()))
                .collect();
            Some((
                item["id"].as_str()?.to_string(),
                json!({"source_id": item["source_id"], "fields": fields}),
            ))
        })
        .collect();
    let published = batch_row.published_revision.and_then(|revision| {
        let path = desk
            .service
            .config
            .data_root
            .join("batches")
            .join(&batch)
            .join("published")
            .join(revision.to_string())
            .join("manifest.json");
        let manifest: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        Some(
            manifest["files"]
                .as_array()?
                .iter()
                .filter_map(|f| f["relative_name"].as_str().map(String::from))
                .collect::<Vec<_>>(),
        )
    });
    Json(json!({
        "paired": true, "user": user, "role": if user == batch_row.applicant { "applicant" } else { "viewer" },
        "csrf": session.csrf,
        "batch": {"id": batch_row.id, "state": batch_row.state.as_str(), "label": batch_row.state.label(), "revision": batch_row.revision},
        "report": assessment.as_ref().map(|a| json!({"text": a.report.text, "items": a.report.items,
                                                     "missing": a.report.missing_candidates, "replacements": a.replacements})),
        "links": assessment.as_ref().map(|a| a.links.clone()),
        "visual_checks": candidates,
        "screenshots": screenshots,
        "published": published,
    }))
    .into_response()
}

/// Download only by object id registered to this batch.
async fn source(
    State(desk): State<Desk>,
    Path((batch, source)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let session = match session_for(&desk, &headers, &batch) {
        Ok(session) => session,
        Err(response) => return response,
    };
    match viewer(&desk, &session) {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "not_paired"),
        Err(response) => return response,
    }
    let files = desk
        .service
        .with_store(|store| store.files(&batch))
        .unwrap_or_default();
    if !files.iter().any(|(id, _)| *id == source) {
        return error(StatusCode::NOT_FOUND, "not_found");
    }
    let root = desk.service.config.data_root.join("batches").join(&batch);
    let derived = root.join("derived").join(format!("{source}.png"));
    let (path, kind) = if derived.is_file() {
        (derived, "image/png")
    } else {
        (
            root.join("objects").join(&source),
            "application/octet-stream",
        )
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return error(StatusCode::NOT_FOUND, "not_found");
    };
    let kind = if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else {
        kind
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from(bytes),
    )
        .into_response()
}

/// Download only files named in the published manifest of this batch.
async fn file(
    State(desk): State<Desk>,
    Path((batch, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let session = match session_for(&desk, &headers, &batch) {
        Ok(session) => session,
        Err(response) => return response,
    };
    match viewer(&desk, &session) {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "not_paired"),
        Err(response) => return response,
    }
    let Ok(batch_row) = desk.service.batch(&batch) else {
        return error(StatusCode::NOT_FOUND, "not_found");
    };
    let Some(revision) = batch_row.published_revision else {
        return error(StatusCode::NOT_FOUND, "not_found");
    };
    let folder = desk
        .service
        .config
        .data_root
        .join("batches")
        .join(&batch)
        .join("published")
        .join(revision.to_string());
    let listed = std::fs::read(folder.join("manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|manifest| manifest["files"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .any(|f| f["relative_name"].as_str() == Some(name.as_str()));
    if !listed {
        return error(StatusCode::NOT_FOUND, "not_found");
    }
    let Ok(bytes) = std::fs::read(folder.join(&name)) else {
        return error(StatusCode::NOT_FOUND, "not_found");
    };
    let kind = if name.ends_with(".pdf") {
        "application/pdf"
    } else {
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    };
    let disposition = format!("attachment; filename*=UTF-8''{}", percent(&name));
    (
        [
            (header::CONTENT_TYPE, kind.to_string()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
        Body::from(bytes),
    )
        .into_response()
}

fn percent(text: &str) -> String {
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

/// Writes: same Host and Origin as the desk, CSRF token of the session, a valid pairing, then the service decides.
fn writer(desk: &Desk, headers: &HeaderMap, batch: &str) -> Result<String, Response> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let expected_host = desk.config.origin.split("://").nth(1).unwrap_or_default();
    if origin != Some(desk.config.origin.as_str()) || host != Some(expected_host) {
        return Err(error(StatusCode::FORBIDDEN, "origin"));
    }
    let session = session_for(desk, headers, batch)?;
    let token = headers.get("x-csrf-token").and_then(|v| v.to_str().ok());
    if token != Some(session.csrf.as_str()) {
        return Err(error(StatusCode::FORBIDDEN, "csrf"));
    }
    viewer(desk, &session)?.ok_or_else(|| error(StatusCode::UNAUTHORIZED, "not_paired"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decide {
    expected_revision: i64,
    item_id: String,
    kind: String,
    payload: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corrections {
    expected_revision: i64,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    source_id: Option<String>,
    #[serde(default)]
    corrections: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    expected_revision: i64,
}

fn done(result: Result<(), ServiceError>) -> Response {
    match result {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(err) => service_error(err),
    }
}

async fn decide(
    State(desk): State<Desk>,
    Path(batch): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Decide>,
) -> Response {
    let user = match writer(&desk, &headers, &batch) {
        Ok(user) => user,
        Err(response) => return response,
    };
    done(
        desk.service
            .decide(
                &batch,
                &user,
                body.expected_revision,
                &body.item_id,
                &body.kind,
                body.payload,
            )
            .await,
    )
}

async fn confirm_visual(
    State(desk): State<Desk>,
    Path(batch): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Corrections>,
) -> Response {
    let user = match writer(&desk, &headers, &batch) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let Some(item) = body.item_id else {
        return error(StatusCode::BAD_REQUEST, "item_id");
    };
    done(
        desk.service
            .confirm_visual(
                &batch,
                &user,
                body.expected_revision,
                &item,
                &body.corrections,
            )
            .await,
    )
}

async fn confirm_screenshot(
    State(desk): State<Desk>,
    Path(batch): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Corrections>,
) -> Response {
    let user = match writer(&desk, &headers, &batch) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let Some(source) = body.source_id else {
        return error(StatusCode::BAD_REQUEST, "source_id");
    };
    done(
        desk.service
            .confirm_screenshot(
                &batch,
                &user,
                body.expected_revision,
                &source,
                &body.corrections,
            )
            .await,
    )
}

async fn confirm(
    State(desk): State<Desk>,
    Path(batch): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Revision>,
) -> Response {
    let user = match writer(&desk, &headers, &batch) {
        Ok(user) => user,
        Err(response) => return response,
    };
    done(
        desk.service
            .confirm(&batch, &user, body.expected_revision)
            .await,
    )
}

async fn decline(
    State(desk): State<Desk>,
    Path(batch): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Revision>,
) -> Response {
    let user = match writer(&desk, &headers, &batch) {
        Ok(user) => user,
        Err(response) => return response,
    };
    done(
        desk.service
            .decline(&batch, &user, body.expected_revision)
            .await,
    )
}
