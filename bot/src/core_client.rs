//! Subprocess boundary to the deterministic core: one JSON envelope per call, bounded in time and output.
use std::{
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

pub const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;
pub const REQUEST_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreCommand {
    Ingest,
    Extract,
    Gates,
    Evidence,
    Link,
    Missing,
    History,
    Package,
    Verify,
    Claim,
}

impl CoreCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Extract => "extract",
            Self::Gates => "gates",
            Self::Evidence => "evidence",
            Self::Link => "link",
            Self::Missing => "missing",
            Self::History => "history",
            Self::Package => "package",
            Self::Verify => "verify",
            Self::Claim => "claim",
        }
    }
}

/// Why a core call produced no result. Only `Rejected` carries a core-defined code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreFailure {
    Rejected {
        code: String,
        message: String,
        exit: i32,
    },
    Timeout,
    OutputLimit,
    Crash(String),
}

impl std::fmt::Display for CoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected {
                code,
                message,
                exit,
            } => write!(f, "core rejected ({exit}) {code}: {message}"),
            Self::Timeout => write!(f, "CORE_TIMEOUT"),
            Self::OutputLimit => write!(f, "CORE_OUTPUT_LIMIT"),
            Self::Crash(detail) => write!(f, "CORE_CRASH: {detail}"),
        }
    }
}

impl std::error::Error for CoreFailure {}

#[derive(Serialize)]
struct Envelope<'a> {
    schema_version: u32,
    command: &'a str,
    request_id: String,
    batch_dir: &'a str,
    policy_path: &'a str,
    input: &'a Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorBody {
    code: String,
    message: String,
    #[allow(dead_code)]
    retriable: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    schema_version: u32,
    request_id: Option<String>,
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ErrorBody>,
}

pub struct CoreClient {
    pub python: PathBuf,
    pub core_dir: PathBuf,
    pub data_root: PathBuf,
    pub batch_dir: PathBuf,
    pub policy: PathBuf,
    pub timeout: Duration,
    sequence: AtomicU64,
}

impl CoreClient {
    pub fn new(
        python: PathBuf,
        core_dir: PathBuf,
        data_root: PathBuf,
        batch_dir: PathBuf,
        policy: PathBuf,
    ) -> Self {
        Self {
            python,
            core_dir,
            data_root,
            batch_dir,
            policy,
            timeout: Duration::from_secs(30),
            sequence: AtomicU64::new(1),
        }
    }

    /// Runs one command. The result stays opaque JSON here; callers parse the parts they read into typed records.
    pub async fn call(&self, command: CoreCommand, input: &Value) -> Result<Value, CoreFailure> {
        let request_id = format!("orc-{}", self.sequence.fetch_add(1, Ordering::SeqCst));
        let envelope = Envelope {
            schema_version: 1,
            command: command.as_str(),
            request_id: request_id.clone(),
            batch_dir: path_str(&self.batch_dir)?,
            policy_path: path_str(&self.policy)?,
            input,
        };
        let body =
            serde_json::to_vec(&envelope).map_err(|error| CoreFailure::Crash(error.to_string()))?;
        if body.len() > REQUEST_LIMIT {
            return Err(CoreFailure::Rejected {
                code: "INPUT_TOO_LARGE".into(),
                message: "Request size limit exceeded".into(),
                exit: 2,
            });
        }
        let (stdout, exit) = self.run(command, body).await?;
        let response: Response = serde_json::from_slice(&stdout)
            .map_err(|_| CoreFailure::Crash("unparseable core response".into()))?;
        if response.schema_version != 1
            || response.request_id.as_deref() != Some(request_id.as_str())
        {
            return Err(CoreFailure::Crash(
                "core response does not match the request".into(),
            ));
        }
        match (response.ok, response.result, response.error) {
            (true, Some(result), None) if exit == 0 => Ok(result),
            (false, None, Some(error)) if exit != 0 => Err(CoreFailure::Rejected {
                code: error.code,
                message: error.message,
                exit,
            }),
            _ => Err(CoreFailure::Crash("inconsistent core response".into())),
        }
    }

    async fn run(
        &self,
        command: CoreCommand,
        body: Vec<u8>,
    ) -> Result<(Vec<u8>, i32), CoreFailure> {
        let mut child = Command::new(&self.python)
            .arg("-m")
            .arg("reimb_core")
            .arg(command.as_str())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("PYTHONPATH", &self.core_dir)
            .env("REIMB_DATA", &self.data_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| CoreFailure::Crash(error.kind().to_string()))?;
        let group = child.id().map(|id| id as i32);
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreFailure::Crash("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreFailure::Crash("no stdout".into()))?;
        let work = async move {
            // A child that exits early closes stdin; its answer on stdout still decides the outcome.
            let _ = stdin.write_all(&body).await;
            drop(stdin);
            let mut output = Vec::new();
            stdout
                .take(OUTPUT_LIMIT + 1)
                .read_to_end(&mut output)
                .await?;
            if output.len() as u64 > OUTPUT_LIMIT {
                return Ok::<_, std::io::Error>((output, None));
            }
            let status = child.wait().await?;
            Ok((output, Some(status)))
        };
        let outcome = tokio::time::timeout(self.timeout, work).await;
        let failure = match outcome {
            Err(_) => Some(CoreFailure::Timeout),
            Ok(Err(error)) => Some(CoreFailure::Crash(error.kind().to_string())),
            Ok(Ok((_, None))) => Some(CoreFailure::OutputLimit),
            Ok(Ok((output, Some(status)))) => {
                return match status.code() {
                    Some(code @ (0 | 2 | 3 | 4)) => Ok((output, code)),
                    _ => Err(CoreFailure::Crash(format!("exit {status}"))),
                };
            }
        };
        if let Some(group) = group {
            // SAFETY: killpg only sends a signal to the process group this call created.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
        }
        Err(failure.unwrap_or_else(|| CoreFailure::Crash("unknown".into())))
    }
}

fn path_str(path: &std::path::Path) -> Result<&str, CoreFailure> {
    path.to_str()
        .ok_or_else(|| CoreFailure::Crash("non UTF-8 path".into()))
}
