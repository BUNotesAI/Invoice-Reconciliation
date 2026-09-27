//! reimb-bot: Matrix bot, batch orchestrator and reconciliation desk in one process, driven by a config file.
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use reimb_bot::{
    agent::{AgentPort, OctosAdapter, ReplayAdapter},
    desk::{self, DeskConfig},
    matrix,
    service::{Service, ServiceConfig},
    store::Store,
};
use serde::Deserialize;

/// Private runtime configuration (`$REIMB_BOT_CONFIG`, mode 0600, never in the repository).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    homeserver: String,
    bot_user: String,
    /// JSON file holding the bot password at `accounts.<credentials_account>.password`.
    credentials: PathBuf,
    credentials_account: String,
    data_root: PathBuf,
    policy: PathBuf,
    python: PathBuf,
    core_dir: PathBuf,
    history: PathBuf,
    /// Optional fixed period; absent: the business month of the clock.
    #[serde(default)]
    period: Option<String>,
    desk_bind: String,
    desk_origin: String,
    /// `replay:<dir>`, `octos:<data dir>` or `none`.
    agent: String,
    applicants: BTreeMap<String, String>,
    #[serde(default)]
    finance_rooms: BTreeMap<String, String>,
    /// End-to-end runs only: a file holding the current Unix time in seconds, read on every clock use, so the
    /// driver can move time (month-end and follow-up reminders). Absent: the system clock.
    #[serde(default)]
    clock_file: Option<PathBuf>,
}

/// The service clock: system time, or the time written in `clock_file` (falling back to system time if unreadable).
fn clock(file: Option<PathBuf>) -> reimb_bot::service::Clock {
    let system = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    };
    match file {
        None => Arc::new(system),
        Some(path) => Arc::new(move || {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or_else(system)
        }),
    }
}

async fn agent(spec: &str) -> Result<AgentPort> {
    Ok(match spec.split_once(':') {
        Some(("replay", dir)) => AgentPort::Replay(
            ReplayAdapter::load(&PathBuf::from(dir)).context("replay recordings")?,
        ),
        Some(("octos", dir)) => {
            match OctosAdapter::start(&PathBuf::from(dir), "reimb-smoke", "bot").await {
                Ok(adapter) => AgentPort::Octos(adapter),
                Err(error) => {
                    eprintln!("octos unavailable, running in rules mode: {error}");
                    AgentPort::Unavailable
                }
            }
        }
        _ if spec == "none" => AgentPort::Unavailable,
        _ => anyhow::bail!("unknown agent {spec}"),
    })
}

async fn run() -> Result<()> {
    let path = std::env::var_os("REIMB_BOT_CONFIG")
        .map(PathBuf::from)
        .context("REIMB_BOT_CONFIG must name the bot config file")?;
    let config: Config =
        serde_json::from_slice(&std::fs::read(&path).context("cannot read bot config")?)
            .context("invalid bot config")?;
    std::fs::create_dir_all(&config.data_root)?;
    let credentials: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&config.credentials).context("cannot read credentials")?,
    )
    .context("invalid credentials")?;
    let password = credentials["accounts"][&config.credentials_account]["password"]
        .as_str()
        .context("bot password missing")?;
    let history =
        serde_json::from_slice(&std::fs::read(&config.history).context("cannot read history")?)
            .context("invalid history")?;
    let store = Store::open(&config.data_root.join("state.sqlite"))?;
    let service = Arc::new(Service::new(
        ServiceConfig {
            data_root: config.data_root.clone(),
            policy: config.policy.clone(),
            python: config.python.clone(),
            core_dir: config.core_dir.clone(),
            history,
            period: config.period.clone(),
            desk_url: config.desk_origin.clone(),
            applicants: config.applicants.clone(),
            finance_rooms: config.finance_rooms.clone(),
        },
        store,
        agent(&config.agent).await?,
        clock(config.clock_file.clone()),
    ));
    // Finish anything that was executing when the process last stopped, before taking new work.
    service.recover().await?;
    let client = matrix::connect(
        &config.homeserver,
        &config.bot_user,
        password,
        &config.data_root.join("bot").join("matrix-session.json"),
    )
    .await?;
    let router = desk::router(
        service.clone(),
        DeskConfig {
            origin: config.desk_origin.clone(),
            access_log: config.data_root.join("desk-access.log"),
        },
    );
    let listener = tokio::net::TcpListener::bind(&config.desk_bind).await?;
    println!("reimb-bot ready; desk on {}", config.desk_origin);
    tokio::select! {
        result = axum::serve(listener, router) => result.context("desk server stopped"),
        result = matrix::run_sync(client.clone(), service.clone()) => result,
        result = matrix::run_outbox(client, service) => result,
        result = tokio::signal::ctrl_c() => result.context("signal handler failed"),
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
