//! Command-line reconciliation pass: runs the real core and an agent port, prints the report as JSON.
use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, Result, bail};
use reimb_bot::{
    agent::{AgentPort, OctosAdapter, ReplayAdapter},
    core_client::CoreClient,
    reconcile::{BatchInput, reconcile},
};

const USAGE: &str = "usage: reimb-reconcile --data-root DIR --batch-dir DIR --policy FILE --uploads DIR --history FILE \
--period YYYY-MM --applicant NAME --python FILE --core-dir DIR --agent replay:DIR|octos:DIR|none [--record DIR] [--out FILE]";

struct Args {
    values: std::collections::BTreeMap<String, String>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut values = std::collections::BTreeMap::new();
        let mut raw = std::env::args().skip(1);
        while let Some(flag) = raw.next() {
            let key = flag
                .strip_prefix("--")
                .with_context(|| USAGE.to_string())?
                .to_string();
            let value = raw
                .next()
                .with_context(|| format!("missing value for --{key}"))?;
            if values.insert(key.clone(), value).is_some() {
                bail!("--{key} given twice");
            }
        }
        Ok(Self { values })
    }

    fn get(&self, key: &str) -> Result<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .with_context(|| format!("--{key} is required\n{USAGE}"))
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        let path = PathBuf::from(self.get(key)?);
        if !path.is_absolute() {
            bail!("--{key} must be an absolute path");
        }
        Ok(path)
    }
}

async fn agent(args: &Args, batch_id: &str) -> Result<AgentPort> {
    let spec = args.get("agent")?;
    Ok(match spec.split_once(':') {
        Some(("replay", dir)) => AgentPort::Replay(
            ReplayAdapter::load(&PathBuf::from(dir)).context("replay recordings")?,
        ),
        Some(("octos", dir)) => {
            match OctosAdapter::start(&PathBuf::from(dir), "reimb-smoke", batch_id).await {
                Ok(adapter) => match args.values.get("record") {
                    Some(record) => AgentPort::Recording(adapter, PathBuf::from(record)),
                    None => AgentPort::Octos(adapter),
                },
                // octos unreachable: the whole batch runs in rules mode, and the report says so.
                Err(_) => AgentPort::Unavailable,
            }
        }
        _ if spec == "none" => AgentPort::Unavailable,
        _ => bail!("unknown --agent {spec}"),
    })
}

async fn run() -> Result<()> {
    let args = Args::parse()?;
    let uploads_dir = args.path("uploads")?;
    let mut uploads: Vec<PathBuf> = std::fs::read_dir(&uploads_dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    uploads.retain(|path| path.is_file());
    uploads.sort();
    let history: serde_json::Value =
        serde_json::from_slice(&std::fs::read(args.path("history")?)?).context("history file")?;
    let batch_dir = args.path("batch-dir")?;
    let batch_id = batch_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("batch")
        .to_string();
    let core = CoreClient::new(
        args.path("python")?,
        args.path("core-dir")?,
        args.path("data-root")?,
        batch_dir,
        args.path("policy")?,
    );
    let mut port = agent(&args, &batch_id).await?;
    let input = BatchInput {
        uploads,
        history,
        period: args.get("period")?.to_string(),
        applicant: args.get("applicant")?.to_string(),
    };
    let report = reconcile(&core, &mut port, &input).await;
    if let AgentPort::Octos(adapter) | AgentPort::Recording(adapter, _) = port {
        adapter.close().await;
    }
    let body = serde_json::to_string_pretty(&report?)? + "\n";
    match args.values.get("out") {
        Some(out) => std::fs::write(out, body)?,
        None => print!("{body}"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reimb-reconcile: {error:#}");
            ExitCode::FAILURE
        }
    }
}
