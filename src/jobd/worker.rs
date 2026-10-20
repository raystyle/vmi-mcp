//! Warm pwsh worker: lazy spawn, b64+JSON line protocol, timeout kill.
//!
//! One long-lived `pwsh -File worker.ps1` per daemon. Requests and
//! responses are single lines of base64(UTF-8(JSON)) — the worker runs
//! each op in a nested [powershell] instance (probe-verified: `exit` is
//! absorbed, streams captured in-memory), so the warm shell survives op
//! crashes. A transport failure (timeout, EOF, garbage) kills the whole
//! process tree; the next op cold-boots a fresh worker — the fallback
//! structure is built in, not bolted on.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::poison_lock;

/// worker stderr lines kept for protocol-failure diagnostics (worker.ps1
/// owns op-level error capture; this tail explains worker-level deaths)
const STDERR_TAIL_LINES: usize = 50;

pub struct Worker {
    /// None only in tests (duplex-injected connection); production always
    /// owns the pwsh child for kill-tree discipline
    child: Option<tokio::process::Child>,
    stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    reader: BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
}

pub struct WorkerSlot {
    pwsh: String,
    script: std::path::PathBuf,
    worker: Mutex<Option<Worker>>,
    born_at: Mutex<Option<String>>,
    /// pid of the worker child currently mid-op (0 = idle). The stale-job
    /// supervisor kills by pid because a wedged exec owns the Worker
    /// object on its stack — the slot itself holds None then.
    running_pid: Mutex<u32>,
    stderr_tail: Arc<Mutex<Vec<String>>>,
}

impl WorkerSlot {
    pub fn new(pwsh: String, script: std::path::PathBuf) -> Arc<Self> {
        Arc::new(Self {
            pwsh,
            script,
            worker: Mutex::new(None),
            born_at: Mutex::new(None),
            running_pid: Mutex::new(0),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn snapshot(&self) -> (bool, Option<String>) {
        (
            poison_lock(&self.worker).is_some(),
            poison_lock(&self.born_at).clone(),
        )
    }

    /// Execute one op on the warm worker. Ok = the job's terminal
    /// envelope (an op failure is still a completed op; the worker stays
    /// warm). Err = transport failure; the worker tree is killed and the
    /// next exec cold-boots a replacement.
    pub async fn exec(&self, request: &Value, timeout: Duration) -> Result<Value, Value> {
        let mut worker = match self.take() {
            Some(w) => w,
            None => match self.spawn_worker() {
                Ok(w) => {
                    *poison_lock(&self.born_at) = Some(crate::jobd::queue::now_iso());
                    w
                }
                Err(msg) => {
                    return Err(json!({
                        "ok": false,
                        "error": {"code": "worker_spawn_failed", "message": msg}
                    }));
                }
            },
        };
        *poison_lock(&self.running_pid) = worker.pid().unwrap_or(0);
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let line = B64.encode(serde_json::to_string(request).unwrap_or_default());
        let outcome = worker
            .roundtrip(&line, &id, timeout, &self.stderr_tail)
            .await;
        *poison_lock(&self.running_pid) = 0;
        match outcome {
            Ok(envelope) => {
                self.put_back(worker);
                Ok(envelope)
            }
            Err(err_envelope) => {
                worker.kill_tree().await;
                Err(err_envelope)
            }
        }
    }

    fn take(&self) -> Option<Worker> {
        poison_lock(&self.worker).take()
    }

    fn put_back(&self, w: Worker) {
        *poison_lock(&self.worker) = Some(w);
    }

    fn spawn_worker(&self) -> Result<Worker, String> {
        let mut cmd = tokio::process::Command::new(&self.pwsh);
        cmd.args(["-NoProfile", "-NoLogo", "-NonInteractive", "-File"])
            .arg(&self.script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", self.pwsh))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "worker stdin not piped".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "worker stdout not piped".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "worker stderr not piped".to_string())?;
        // bounded stderr drain: a wedged writer would otherwise deadlock
        // the 64K pipe buffer, and a worker death would leave no trace
        let tail = Arc::clone(&self.stderr_tail);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let mut t = poison_lock(&tail);
                        t.push(line.trim_end().to_string());
                        let len = t.len();
                        if len > STDERR_TAIL_LINES {
                            t.drain(..len - STDERR_TAIL_LINES);
                        }
                    }
                }
            }
        });
        Ok(Worker {
            child: Some(child),
            stdin: Box::new(stdin),
            reader: BufReader::new(Box::new(stdout)),
        })
    }

    /// Kill the worker if alive (daemon shutdown path).
    pub async fn shutdown(&self) {
        if let Some(mut w) = self.take() {
            w.kill_tree().await;
        }
    }

    /// Supervisor escape hatch: kill the tree of the worker currently
    /// mid-op (tracked by pid — during a wedged exec the Worker object is
    /// owned by exec's stack frame, not the slot). The kill lands as EOF
    /// in the parked roundtrip read, which unwinds exec's error path
    /// normally. Bounded: a hung taskkill must not wedge the supervisor.
    pub async fn kill_running(&self) -> bool {
        let pid = *poison_lock(&self.running_pid);
        if pid == 0 {
            return false;
        }
        let mut kill = tokio::process::Command::new("taskkill");
        kill.args(["/PID", &pid.to_string(), "/T", "/F"]);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            kill.creation_flags(CREATE_NO_WINDOW);
        }
        let _ = tokio::time::timeout(Duration::from_secs(15), kill.status()).await;
        true
    }

    /// Test seam: plant a duplex-injected worker so exec() runs against a
    /// scripted peer instead of spawning pwsh.
    #[cfg(test)]
    fn plant_for_test(&self, w: Worker) {
        *poison_lock(&self.worker) = Some(w);
    }
}

impl Worker {
    fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    async fn roundtrip(
        &mut self,
        line: &str,
        id: &Value,
        timeout: Duration,
        tail: &Arc<Mutex<Vec<String>>>,
    ) -> Result<Value, Value> {
        let tail_str = || poison_lock(tail).join("\n");
        let io = async {
            self.stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| proto_err("worker stdin write failed", e, &tail_str()))?;
            self.stdin
                .write_all(b"\n")
                .await
                .map_err(|e| proto_err("worker stdin newline write failed", e, &tail_str()))?;
            self.stdin
                .flush()
                .await
                .map_err(|e| proto_err("worker stdin flush failed", e, &tail_str()))?;
            let mut buf = String::new();
            let n = self
                .reader
                .read_line(&mut buf)
                .await
                .map_err(|e| proto_err("worker stdout read failed", e, &tail_str()))?;
            if n == 0 {
                return Err(json!({
                    "ok": false,
                    "error": {
                        "code": "pwsh_protocol",
                        "message": "worker exited before responding",
                        "errTail": tail_str(),
                    }
                }));
            }
            Ok(buf)
        };
        // one clock over write+read: the op timeout covers both a wedged
        // writer and a long-running op
        let resp_line = match tokio::time::timeout(timeout, io).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(json!({
                    "ok": false,
                    "error": {
                        "code": "op_timeout",
                        "message": format!(
                            "op exceeded {}s (worker tree killed; next op cold-boots)",
                            timeout.as_secs()
                        ),
                    }
                }));
            }
        };
        let raw = match B64.decode(resp_line.trim()) {
            Ok(r) => r,
            Err(e) => return Err(proto_err("response is not base64", &e, &tail_str())),
        };
        let v: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => return Err(proto_err("response is not JSON", &e, &tail_str())),
        };
        if v.get("id") != Some(id) {
            return Err(json!({
                "ok": false,
                "error": {
                    "code": "pwsh_protocol",
                    "message": format!(
                        "response id mismatch: sent {id}, got {}",
                        v.get("id").cloned().unwrap_or(Value::Null)
                    ),
                    "errTail": tail_str(),
                }
            }));
        }
        if v.get("ok") == Some(&Value::Bool(true)) {
            match v.get("envelope") {
                Some(e) if e.is_object() => Ok(e.clone()),
                _ => Err(json!({
                    "ok": false,
                    "error": {
                        "code": "pwsh_protocol",
                        "message": "response ok=true but envelope missing or not an object",
                        "errTail": tail_str(),
                    }
                })),
            }
        } else {
            // worker-level failure (op output wasn't a single JSON etc.):
            // still a completed op carrying an error envelope; the worker
            // absorbed the crash and stays warm
            let error = v.get("error").cloned().unwrap_or(json!({
                "code": "pwsh_protocol",
                "message": "worker reported failure without an error object"
            }));
            Ok(json!({ "ok": false, "error": error }))
        }
    }

    /// taskkill /T /F the whole tree (ops spawn native children that a
    /// bare child.kill() would orphan), then reap. Bounded: a hung
    /// taskkill must not wedge the dispatcher forever — that was one of
    /// the observed hang modes.
    async fn kill_tree(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if let Some(pid) = child.id() {
                let mut kill = tokio::process::Command::new("taskkill");
                kill.args(["/PID", &pid.to_string(), "/T", "/F"]);
                #[cfg(windows)]
                {
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    kill.creation_flags(CREATE_NO_WINDOW);
                }
                let _ = tokio::time::timeout(Duration::from_secs(15), kill.status()).await;
            }
            let _ = child.kill().await;
        }
    }
}

fn proto_err(what: &str, e: impl std::fmt::Display, err_tail: &str) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": "pwsh_protocol",
            "message": format!("{what}: {e}"),
            "errTail": err_tail,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    /// Worker wired to a duplex whose far side the test drives as the
    /// fake worker peer.
    fn worker_from_duplex(buf: usize) -> (Worker, tokio::io::DuplexStream) {
        let (client, server) = tokio::io::duplex(buf);
        let (r, w) = tokio::io::split(client);
        (
            Worker {
                child: None,
                stdin: Box::new(w),
                reader: BufReader::new(Box::new(r)),
            },
            server,
        )
    }

    fn empty_tail() -> Arc<Mutex<Vec<String>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// Peer that decodes one request and answers with a scripted response.
    async fn serve_one(server: tokio::io::DuplexStream, respond: fn(&Value) -> Value) {
        let mut server = BufReader::new(server);
        let mut line = String::new();
        server.read_line(&mut line).await.unwrap();
        let raw = B64.decode(line.trim()).unwrap();
        let req: Value = serde_json::from_slice(&raw).unwrap();
        let resp = respond(&req);
        server
            .get_mut()
            .write_all(format!("{}\n", B64.encode(resp.to_string())).as_bytes())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn roundtrip_ok_carries_envelope() {
        let (mut worker, server) = worker_from_duplex(4096);
        let peer = tokio::spawn(serve_one(server, |req| {
            json!({
                "id": req["id"],
                "ok": true,
                "envelope": {"ok": true, "data": "vm-list done"},
            })
        }));
        let id = json!("abc12345");
        let line = B64.encode(
            json!({"id": "abc12345", "op": "vm-list", "params": {}, "env": {}}).to_string(),
        );
        let env = worker
            .roundtrip(&line, &id, Duration::from_secs(2), &empty_tail())
            .await
            .unwrap();
        assert_eq!(env["data"], "vm-list done");
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn roundtrip_worker_error_is_completed_op() {
        // ok:false from the worker = op failed inside, warm shell survives:
        // the same Worker answers a second roundtrip afterwards
        let (mut worker, server) = worker_from_duplex(4096);
        let peer = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            for i in 0..2 {
                let mut line = String::new();
                server.read_line(&mut line).await.unwrap();
                let req: Value = serde_json::from_slice(&B64.decode(line.trim()).unwrap()).unwrap();
                let resp = if i == 0 {
                    json!({
                        "id": req["id"],
                        "ok": false,
                        "error": {"code": "pwsh_protocol", "message": "op output not single JSON"},
                    })
                } else {
                    json!({"id": req["id"], "ok": true, "envelope": {"ok": true}})
                };
                server
                    .get_mut()
                    .write_all(format!("{}\n", B64.encode(resp.to_string())).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let tail = empty_tail();
        let line = B64.encode(json!({"id": "j1", "op": "x"}).to_string());
        let env = worker
            .roundtrip(&line, &json!("j1"), Duration::from_secs(2), &tail)
            .await
            .unwrap();
        assert_eq!(env["ok"], false);
        assert_eq!(env["error"]["code"], "pwsh_protocol");
        // second roundtrip on the same (still warm) worker
        let env2 = worker
            .roundtrip(&line, &json!("j1"), Duration::from_secs(2), &tail)
            .await
            .unwrap();
        assert_eq!(env2["ok"], true);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn roundtrip_timeout_is_op_timeout() {
        let (mut worker, server) = worker_from_duplex(4096);
        tokio::spawn(async move {
            // read the request, then stay silent past the deadline
            let mut server = BufReader::new(server);
            let mut line = String::new();
            let _ = server.read_line(&mut line).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let line = B64.encode(json!({"id": "j2", "op": "x"}).to_string());
        let err = worker
            .roundtrip(
                &line,
                &json!("j2"),
                Duration::from_millis(150),
                &empty_tail(),
            )
            .await
            .unwrap_err();
        assert_eq!(err["error"]["code"], "op_timeout");
    }

    #[tokio::test]
    async fn roundtrip_eof_and_garbage_are_protocol_errors() {
        // EOF: peer closes without answering
        let (mut worker, server) = worker_from_duplex(64);
        tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut line = String::new();
            let _ = server.read_line(&mut line).await;
            drop(server);
        });
        let line = B64.encode(json!({"id": "j3", "op": "x"}).to_string());
        let err = worker
            .roundtrip(&line, &json!("j3"), Duration::from_secs(2), &empty_tail())
            .await
            .unwrap_err();
        assert_eq!(err["error"]["code"], "pwsh_protocol");

        // garbage: peer answers non-base64
        let (mut worker, server) = worker_from_duplex(64);
        tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut line = String::new();
            let _ = server.read_line(&mut line).await;
            let _ = server.get_mut().write_all(b"!!!not-base64!!!\n").await;
        });
        let err = worker
            .roundtrip(&line, &json!("j3"), Duration::from_secs(2), &empty_tail())
            .await
            .unwrap_err();
        assert_eq!(err["error"]["code"], "pwsh_protocol");
        assert!(err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not base64"));
    }

    #[tokio::test]
    async fn roundtrip_id_mismatch_rejected() {
        let (mut worker, server) = worker_from_duplex(4096);
        let peer = tokio::spawn(serve_one(
            server,
            |_| json!({"id": "someone-else", "ok": true, "envelope": {"ok": true}}),
        ));
        let line = B64.encode(json!({"id": "j4", "op": "x"}).to_string());
        let err = worker
            .roundtrip(&line, &json!("j4"), Duration::from_secs(2), &empty_tail())
            .await
            .unwrap_err();
        assert_eq!(err["error"]["code"], "pwsh_protocol");
        assert!(err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("mismatch"));
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn roundtrip_ok_true_without_envelope_rejected() {
        let (mut worker, server) = worker_from_duplex(4096);
        let peer = tokio::spawn(serve_one(
            server,
            |req| json!({"id": req["id"], "ok": true}),
        ));
        let line = B64.encode(json!({"id": "j5", "op": "x"}).to_string());
        let err = worker
            .roundtrip(&line, &json!("j5"), Duration::from_secs(2), &empty_tail())
            .await
            .unwrap_err();
        assert_eq!(err["error"]["code"], "pwsh_protocol");
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn exec_timeout_drops_worker_for_cold_respawn() {
        let slot = WorkerSlot::new("pwsh".into(), std::path::PathBuf::from("unused"));
        let (worker, server) = worker_from_duplex(4096);
        tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut line = String::new();
            let _ = server.read_line(&mut line).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        slot.plant_for_test(worker);
        let (alive, born) = slot.snapshot();
        assert!(alive);
        assert!(born.is_none()); // planted, not spawned
        let err = slot
            .exec(&json!({"id": "j6", "op": "x"}), Duration::from_millis(150))
            .await
            .unwrap_err();
        assert_eq!(err["error"]["code"], "op_timeout");
        let (alive, _) = slot.snapshot();
        assert!(!alive, "transport failure must drop the worker");
    }

    #[tokio::test]
    async fn exec_ok_keeps_worker_warm() {
        let slot = WorkerSlot::new("pwsh".into(), std::path::PathBuf::from("unused"));
        let (worker, server) = worker_from_duplex(4096);
        tokio::spawn(serve_one(
            server,
            |req| json!({"id": req["id"], "ok": true, "envelope": {"ok": true}}),
        ));
        slot.plant_for_test(worker);
        let env = slot
            .exec(&json!({"id": "j7", "op": "x"}), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(env["ok"], true);
        let (alive, _) = slot.snapshot();
        assert!(alive, "completed op must keep the worker warm");
    }

    /// Real-pwsh protocol contract check (run explicitly on a dev box):
    /// a minimal echo worker.ps1 answering in the daemon's frame format.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "needs pwsh on PATH; protocol contract vs worker.ps1 (M3)"]
    async fn real_pwsh_echo_worker() {
        let dir = std::env::temp_dir().join(format!("vmi-jobd-real-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("worker.ps1");
        std::fs::write(
            &script,
            "$reader = [IO.StreamReader]::new([Console]::OpenStandardInput())\n\
             $writer = [IO.StreamWriter]::new([Console]::OpenStandardOutput())\n\
             $writer.AutoFlush = $true\n\
             while ($null -ne ($line = $reader.ReadLine())) {\n\
               $req = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($line)) | ConvertFrom-Json\n\
               $resp = @{ id = $req.id; ok = $true; envelope = @{ ok = $true; data = 'warm' } } | ConvertTo-Json -Compress\n\
               $writer.WriteLine([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($resp)))\n\
             }\n",
        )
        .unwrap();
        let slot = WorkerSlot::new("pwsh".into(), script);
        let env = slot
            .exec(
                &json!({"id": "real1", "op": "echo", "params": {}, "env": {}}),
                Duration::from_secs(30),
            )
            .await
            .expect("real pwsh worker roundtrip");
        assert_eq!(env["data"], "warm");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
