//! jobd: headless elevated job daemon (pipe-only face, no HTTP).
//!
//! Tool set mirrors the daemon.ps1 file-queue era so the Python jobface
//! keeps its shape: job_submit/status/wait/cancel/list/last plus
//! daemon_status/daemon_stop. Ops run serially on one warm pwsh worker;
//! the op table (timeouts) stays on the Python side — the daemon only
//! executes and remembers results.

pub mod queue;
pub mod worker;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Json;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use queue::Queue;
use worker::WorkerSlot;

/// Elevated daemon's pipe, connectable by same-machine non-elevated
/// users (GWGR for Authenticated Users): the elevated context is the
/// daemon's, but the queue dir is per-user, so the owning user must be
/// able to reach their own daemon without a second elevation.
const JOBD_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GWGR;;;AU)";

/// One dispatch unit: a submitted op waiting for its serial turn.
struct JobMsg {
    id: String,
    request: Value,
    timeout: Duration,
}

struct JobFace {
    queue: std::sync::Arc<Queue>,
    worker: std::sync::Arc<WorkerSlot>,
    tx: mpsc::Sender<JobMsg>,
    ct: CancellationToken,
    ops_dir: PathBuf,
}

#[tool_router(server_handler)]
impl JobFace {
    #[tool(
        description = "Submit an op for serial execution on the warm pwsh worker. Returns {id, op, state=pending}"
    )]
    fn job_submit(
        &self,
        Parameters(args): Parameters<JobSubmitArgs>,
    ) -> Result<Json<Value>, ErrorData> {
        let op = args.op.trim().to_string();
        let ok_name = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if op.is_empty() || !op.chars().all(ok_name) {
            return Err(crate::err_invalid(
                "op must match ^[a-z0-9-]+$ (kebab-case script name)",
            ));
        }
        let script = self.ops_dir.join(format!("{op}.ps1"));
        if !script.is_file() {
            return Err(crate::err_invalid(&format!(
                "no such op script: {}",
                script.display()
            )));
        }
        let id = self.queue.new_id();
        self.queue.submit(&id, &op);
        let request = json!({
            "id": id,
            "op": op,
            "params": args.params.unwrap_or_else(|| json!({})),
            "env": args.env.unwrap_or_else(|| json!({})),
        });
        let msg = JobMsg {
            id: id.clone(),
            request,
            timeout: Duration::from_secs(args.timeout_s.unwrap_or(600).min(3600)),
        };
        let tx = self.tx.clone();
        // the serial dispatcher is the only consumer; a full channel means
        // the daemon is wedged, which the pipe liveness probe surfaces
        if tx.try_send(msg).is_err() {
            return Err(crate::err_internal(
                "dispatch channel full (worker wedged?; check daemon_status)",
            ));
        }
        Ok(Json(json!({ "id": id, "op": op, "state": "pending" })))
    }

    #[tool(description = "Job state: {id, op, state, envelope?, timestamps}")]
    fn job_status(
        &self,
        Parameters(args): Parameters<JobIdArgs>,
    ) -> Result<Json<Value>, ErrorData> {
        self.queue
            .status(&args.id)
            .map(Json)
            .ok_or_else(|| crate::err_invalid(&format!("unknown job {}", args.id)))
    }

    #[tool(
        description = "Wait for a job to finish (polls internally, default 10s max). Returns the terminal status with envelope when done/cancelled, the current status on wait timeout"
    )]
    async fn job_wait(
        &self,
        Parameters(args): Parameters<JobWaitArgs>,
    ) -> Result<Json<Value>, ErrorData> {
        let deadline = Instant::now() + Duration::from_secs(args.timeout_s.unwrap_or(10).min(600));
        loop {
            let st = self
                .queue
                .status(&args.id)
                .ok_or_else(|| crate::err_invalid(&format!("unknown job {}", args.id)))?;
            let state = st["state"].as_str().unwrap_or_default();
            if state == "done" || state == "cancelled" || Instant::now() >= deadline {
                return Ok(Json(st));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    #[tool(
        description = "Cancel a job. Only pending jobs cancel (the serial worker has no preemption); returns {cancelled, state}"
    )]
    fn job_cancel(
        &self,
        Parameters(args): Parameters<JobIdArgs>,
    ) -> Result<Json<Value>, ErrorData> {
        let (cancelled, state) = self.queue.cancel(&args.id);
        Ok(Json(
            json!({ "cancelled": cancelled, "state": state, "id": args.id }),
        ))
    }

    #[tool(description = "All jobs this daemon boot has seen: [{id, op, state, ...}]")]
    fn job_list(&self) -> Json<Value> {
        Json(json!({ "jobs": self.queue.list() }))
    }

    #[tool(
        description = "Recently finished job artifacts from disk (survives daemon restarts), newest first. n default 5"
    )]
    fn job_last(&self, Parameters(args): Parameters<JobLastArgs>) -> Json<Value> {
        let n = args.n.unwrap_or(5).min(50) as usize;
        Json(json!({ "jobs": self.queue.last(n) }))
    }

    #[tool(description = "Daemon heartbeat: pid, uptime, pwsh version, worker state, queue counts")]
    fn daemon_status(&self) -> Json<Value> {
        let (alive, born) = self.worker.snapshot();
        Json(self.queue.daemon_status(alive, born))
    }

    #[tool(description = "Stop the daemon (worker killed, heartbeat removed, process exits)")]
    async fn daemon_stop(&self) -> Json<Value> {
        // delay the cancel so this response flushes before the pipe dies
        let ct = self.ct.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            ct.cancel();
        });
        Json(json!({ "stopping": true }))
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct JobSubmitArgs {
    /// op name (= <op>.ps1 in the ops dir, [a-z0-9-] only)
    pub op: String,
    /// op parameters (JSON object; splatted to the script by the worker)
    #[serde(default)]
    pub params: Option<Value>,
    /// env credentials injected for this op only (JSON object; removed in
    /// the worker's finally; never persisted or logged)
    #[serde(default)]
    pub env: Option<Value>,
    /// hard timeout seconds (worker tree killed past this; default 600)
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct JobIdArgs {
    /// job id from job_submit
    pub id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct JobWaitArgs {
    /// job id from job_submit
    pub id: String,
    /// max seconds to wait before returning the current state (default 10)
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct JobLastArgs {
    /// how many recent artifacts to return (default 5, max 50)
    #[serde(default)]
    pub n: Option<u64>,
}

/// pwsh resolution + version probe (one cold spawn at boot; the version
/// is reported in daemon_status).
fn probe_pwsh() -> anyhow::Result<(String, String)> {
    let mut cmd = std::process::Command::new("pwsh");
    cmd.args([
        "-NoProfile",
        "-NoLogo",
        "-NonInteractive",
        "-Command",
        "$PSVersionTable.PSVersion.ToString()",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output()?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    anyhow::ensure!(
        out.status.success() && !version.is_empty(),
        "pwsh probe failed (exit {:?}): {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(("pwsh".to_string(), version))
}

#[cfg(windows)]
fn default_jobd_pipe_name() -> String {
    format!(
        r"\\.\pipe\hvperv-jobd-{}",
        std::env::var("USERNAME").unwrap_or_default()
    )
}

#[cfg(not(windows))]
fn default_jobd_pipe_name() -> String {
    format!(
        "/tmp/hvperv-jobd-{}.sock",
        std::env::var("USER").unwrap_or_default()
    )
}

/// jobd entry: assemble queue + worker + serial dispatcher, then serve
/// the pipe face until cancelled.
pub async fn run_jobd(
    ops_dir: PathBuf,
    queue_dir: PathBuf,
    pipe_name: Option<String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(ops_dir.is_dir(), "ops dir missing: {}", ops_dir.display());
    let worker_script = ops_dir.join("worker.ps1");
    anyhow::ensure!(
        worker_script.is_file(),
        "worker script missing: {}",
        worker_script.display()
    );
    let (pwsh, pwsh_version) = probe_pwsh()?;
    let queue = Queue::new(queue_dir.clone(), pwsh_version.clone())?;
    let worker = WorkerSlot::new(pwsh, worker_script);
    let ct = CancellationToken::new();

    // serial dispatcher: one op at a time on the warm worker
    let (tx, mut rx) = mpsc::channel::<JobMsg>(64);
    {
        let queue = std::sync::Arc::clone(&queue);
        let worker = std::sync::Arc::clone(&worker);
        let ct = ct.clone();
        tokio::spawn(async move {
            while let Some(JobMsg {
                id,
                request,
                timeout,
            }) = rx.recv().await
            {
                queue.mark_running(&id);
                let envelope = match worker.exec(&request, timeout).await {
                    Ok(e) => e,
                    Err(e) => e,
                };
                queue.finish(&id, envelope);
                if ct.is_cancelled() {
                    break;
                }
            }
        });
    }

    // heartbeat + control-flag watchdog (display artifact + fallback path
    // for when the pipe itself is wedged)
    queue.spawn_watchdog(std::sync::Arc::clone(&worker), ct.child_token());

    // deterministic exit: the accept loop parks in connect() with no
    // client, so cancellation alone never returns — exit explicitly once
    // the cleanup settles
    {
        let ct = ct.clone();
        let worker = std::sync::Arc::clone(&worker);
        let queue_dir = queue_dir.clone();
        tokio::spawn(async move {
            ct.cancelled().await;
            worker.shutdown().await;
            // grace must exceed the watchdog's in-flight heartbeat write
            // (cancellable select sleep) so the file is gone before exit
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let _ = std::fs::remove_file(queue_dir.join("daemon.json"));
            std::process::exit(0);
        });
    }

    let pipe_addr = pipe_name.unwrap_or_else(default_jobd_pipe_name);
    queue.log(&format!(
        "jobd up (pwsh {pwsh_version}, ops {}, pipe {pipe_addr})",
        ops_dir.display()
    ));
    let factory = {
        let queue = std::sync::Arc::clone(&queue);
        let worker = std::sync::Arc::clone(&worker);
        let tx = tx.clone();
        let ct = ct.clone();
        let ops_dir = ops_dir.clone();
        move || JobFace {
            queue: std::sync::Arc::clone(&queue),
            worker: std::sync::Arc::clone(&worker),
            tx: tx.clone(),
            ct: ct.clone(),
            ops_dir: ops_dir.clone(),
        }
    };
    crate::pipe::listen(&pipe_addr, Some(JOBD_SDDL), factory, ct).await
}
