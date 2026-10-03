//! Shared test helpers: repository paths and a well-behaved scripted model.
#![allow(dead_code)]
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use reimb_bot::{
    agent::{AgentError, AgentPort, AgentRequest, AgentTask},
    service::{Service, ServiceConfig},
    store::Store,
};
use serde_json::{Map, Value, json};

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

pub fn expected(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(repo().join("fixtures/expected").join(name)).unwrap())
        .unwrap()
}

pub fn category(seller: &str) -> (&'static str, &'static str) {
    match seller {
        s if s.contains("瑞幸") => ("餐饮", "瑞幸"),
        s if s.contains("滴滴") => ("打车", "滴滴"),
        s if s.contains("航空") => ("机票", "国航"),
        s if s.contains("酒店") => ("住宿", "燕园会展酒店"),
        s if s.contains("潮海居") => ("餐饮", "潮海居"),
        _ => ("餐饮", "某店"),
    }
}

/// Correct readings, in-policy categories, explanations citing real facts.
pub fn honest(request: &AgentRequest) -> Result<String, AgentError> {
    Ok(match request.task {
        AgentTask::ReadInvoice => json!({"invoice_no": "26442000000500010191", "issue_date": "2026-10-19", "amount_cents": "386.00",
            "amount_upper": "叁佰捌拾陆圆整", "buyer_name": "示例科技有限公司", "buyer_tax_id": "91440300XXXXXXXX0A",
            "seller_name": "深圳潮海居酒楼有限公司", "project": "*餐饮服务*餐费", "remark": ""})
        .to_string(),
        AgentTask::ReadScreenshot => {
            json!({"merchant": "滴滴出行", "amount": "20.00", "service_date": "2026-10-17", "order_ref": "DD-20261017-6630"}).to_string()
        }
        AgentTask::Classify => {
            let (category, short) = category(request.data["seller_name"].as_str().unwrap());
            json!({"category": category, "short_name": short, "rationale": "按销售方与项目判断。"}).to_string()
        }
        AgentTask::Rank => {
            let first = request.data["candidates"][0]["id"].as_str().unwrap();
            json!({"choice": first, "reason": "日期最接近开票日。"}).to_string()
        }
        AgentTask::Explain => {
            let items: Vec<Value> = request.data["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    let id = item["item_id"].as_str().unwrap();
                    let amount = format!("{id}.amount");
                    match item["facts"].get(&amount).and_then(Value::as_str) {
                        Some(yuan) => json!({"item_id": id, "fact_refs": [amount], "explanation": format!("这张票 {yuan} 元，需要你看一下。")}),
                        None => json!({"item_id": id, "fact_refs": [], "explanation": "这张票需要你看一下。"}),
                    }
                })
                .collect();
            json!({"items": items}).to_string()
        }
    })
}

pub fn scripted(
    mut answer: impl FnMut(&AgentRequest) -> Result<String, AgentError> + Send + 'static,
) -> AgentPort {
    AgentPort::Scripted(Box::new(move |request| answer(request)))
}

pub const LINYI: &str = "@reimb-linyi:reimb.local";
pub const ZHOUMIN: &str = "@reimb-zhoumin:reimb.local";
pub const INTRUDER: &str = "@reimb-intruder:reimb.local";
pub const FINANCE_ROOM: &str = "!zhoumin-bot:reimb.local";
pub const ROOM: &str = "!linyi-bot:reimb.local";
/// A second applicant with a batch of their own, for cross-batch checks.
pub const SECOND: &str = "@reimb-second:reimb.local";
pub const SECOND_ROOM: &str = "!second-bot:reimb.local";
// 2026-10-31 09:00 in Asia/Shanghai.
pub const MONTH_END: i64 = 1_793_408_400;

pub struct Harness {
    pub _dir: tempfile::TempDir,
    /// Reimbursement history the service starts from (the demo history unless a test swaps it).
    pub history: PathBuf,
    pub root: PathBuf,
    pub clock: Arc<AtomicI64>,
}

impl Harness {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("data");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::copy(repo().join("policy/example.yaml"), root.join("policy.yaml")).unwrap();
        Self {
            _dir: dir,
            root,
            clock: Arc::new(AtomicI64::new(MONTH_END)),
            history: repo().join("fixtures/demo/history.json"),
        }
    }

    pub fn service(&self) -> Service {
        self.service_with(scripted(honest))
    }

    pub fn service_with(&self, agent: AgentPort) -> Service {
        self.service_using(agent, repo().join(".venv/bin/python"))
    }

    /// A service whose core runs under `python`: a broken path makes every core call fail.
    pub fn service_using(&self, agent: AgentPort, python: PathBuf) -> Service {
        let clock = self.clock.clone();
        let config = ServiceConfig {
            data_root: self.root.clone(),
            policy: self.root.join("policy.yaml"),
            python,
            core_dir: repo().join("core"),
            history: serde_json::from_slice(&std::fs::read(&self.history).unwrap()).unwrap(),
            // Clock-driven: the harness clock starts on 2026-10-31.
            period: None,
            desk_url: "http://127.0.0.1:8787".into(),
            applicants: BTreeMap::from([
                (LINYI.to_string(), ROOM.to_string()),
                (SECOND.to_string(), SECOND_ROOM.to_string()),
            ]),
            finance_rooms: BTreeMap::from([(ZHOUMIN.to_string(), FINANCE_ROOM.to_string())]),
        };
        let store = Store::open(&self.root.join("state.sqlite")).unwrap();
        Service::new(
            config,
            store,
            agent,
            Arc::new(move || clock.load(Ordering::SeqCst)),
        )
    }
}

/// A stand-in interpreter that waits before running the real core, so a test can cut a handler off mid-way.
pub fn slow_python(root: &Path, seconds: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("slow-python");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nsleep {seconds}\nexec '{}' \"$@\"\n",
            repo().join(".venv/bin/python").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub async fn upload_demo(service: &Service) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(repo().join("fixtures/demo"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    for (index, path) in paths.iter().enumerate() {
        let name = path.file_name().unwrap().to_str().unwrap();
        if name == "history.json" {
            continue;
        }
        service
            .receive_file(
                LINYI,
                ROOM,
                &format!("$up{index}"),
                name,
                &std::fs::read(path).unwrap(),
            )
            .await
            .unwrap();
    }
}

pub fn texts(service: &Service, batch: &str) -> Vec<String> {
    service
        .with_store(|store| store.outbox(batch))
        .unwrap()
        .into_iter()
        .filter_map(|(_, content, _)| content["body"].as_str().map(String::from))
        .collect()
}

pub fn item_id(service: &Service, batch: &str, file: &str) -> String {
    let assessment = service.assessment(batch).unwrap().unwrap();
    assessment
        .report
        .items
        .iter()
        .find(|item| item.file == file)
        .unwrap()
        .item_id
        .clone()
}

pub async fn to_needs_decision(harness: &Harness) -> (Service, String) {
    let service = harness.service();
    upload_demo(&service).await;
    service
        .receive_text(LINYI, ROOM, "$start", "开始对账")
        .await
        .unwrap();
    let batch = service
        .with_store(|store| store.open_batch_for(LINYI))
        .unwrap()
        .unwrap();
    (service, batch.id)
}

/// Decisions a careful applicant makes for F06, F08, F09 and F10, all bound to the current revision.
pub async fn decide_everything(service: &Service, batch: &str) {
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
