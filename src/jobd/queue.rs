//! In-memory job registry + serial dispatch + on-disk artifacts.
//!
//! Disk contract (compatible with the daemon.ps1 file-queue era):
//!   <queue_dir>/done/<id>.result.json   terminal artifacts, GC keeps 100
//!   <queue_dir>/daemon.json             heartbeat (display artifact; the
//!                                       pipe IS the liveness probe)
//!   <queue_dir>/daemon.log              diagnostics
//!   <queue_dir>/control/stop.flag       stop request (watchdog fallback
//!                                       for when the pipe is wedged)
//!   <queue_dir>/control/cancel-<id>.flag cancel request (same fallback)
//!
//! pending/running live in memory only: a daemon death breaks the pipe,
//! which the client notices immediately (deterministic, unlike the old
//! heartbeat-staleness guess).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

pub const RESULT_KEEP: usize = 100;
const HEARTBEAT_PERIOD: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Pending,
    Running,
    Done,
    Cancelled,
}

impl State {
    fn as_str(&self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Running => "running",
            State::Done => "done",
            State::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug)]
struct Job {
    op: String,
    state: State,
    envelope: Option<Value>,
    submitted_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

impl Job {
    fn to_public(&self, id: &str) -> Value {
        let mut v = json!({
            "id": id,
            "op": self.op,
            "state": self.state.as_str(),
        });
        if let Some(env) = &self.envelope {
            v["envelope"] = env.clone();
        }
        v["submitted_at"] = json!(self.submitted_at);
        if let Some(t) = &self.started_at {
            v["started_at"] = json!(t);
        }
        if let Some(t) = &self.finished_at {
            v["finished_at"] = json!(t);
        }
        v
    }
}

pub fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // civil-time conversion without chrono (UTC)
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

pub struct Queue {
    dir: std::path::PathBuf,
    jobs: Mutex<HashMap<String, Job>>,
    counter: AtomicU32,
    boot_nonce: u32,
    started: std::time::Instant,
    pwsh_version: String,
    pub done_count: std::sync::atomic::AtomicUsize,
}

impl Queue {
    pub fn new(dir: std::path::PathBuf, pwsh_version: String) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(dir.join("done"))?;
        std::fs::create_dir_all(dir.join("control"))?;
        let boot_nonce = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0))
            ^ (std::process::id() << 16);
        Ok(Arc::new(Self {
            dir,
            jobs: Mutex::new(HashMap::new()),
            counter: AtomicU32::new(1),
            boot_nonce,
            started: std::time::Instant::now(),
            pwsh_version,
            done_count: std::sync::atomic::AtomicUsize::new(0),
        }))
    }

    pub fn new_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:08x}", self.boot_nonce.wrapping_add(n) & 0xFFFF_FFFF)
    }

    pub fn submit(&self, id: &str, op: &str) {
        self.log(&format!("job {id} submitted: {op}"));
        self.jobs.lock().unwrap().insert(
            id.to_string(),
            Job {
                op: op.to_string(),
                state: State::Pending,
                envelope: None,
                submitted_at: now_iso(),
                started_at: None,
                finished_at: None,
            },
        );
    }

    pub fn mark_running(&self, id: &str) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(id) {
            j.state = State::Running;
            j.started_at = Some(now_iso());
        }
    }

    pub fn finish(&self, id: &str, envelope: Value) {
        let artifact = {
            let mut jobs = self.jobs.lock().unwrap();
            match jobs.get_mut(id) {
                Some(j) => {
                    j.state = State::Done;
                    j.finished_at = Some(now_iso());
                    j.envelope = Some(envelope.clone());
                    j.to_public(id)
                }
                None => return,
            }
        };
        self.write_result(id, &artifact);
        self.log(&format!("job {id} done"));
    }

    /// pending -> cancelled; running/done/cancelled reported as-is.
    pub fn cancel(&self, id: &str) -> (bool, &'static str) {
        let mut jobs = self.jobs.lock().unwrap();
        match jobs.get_mut(id) {
            Some(j) if j.state == State::Pending => {
                j.state = State::Cancelled;
                j.finished_at = Some(now_iso());
                (true, "cancelled")
            }
            Some(j) => (false, j.state.as_str()),
            None => (false, "unknown"),
        }
    }

    pub fn status(&self, id: &str) -> Option<Value> {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(id).map(|j| j.to_public(id))
    }

    pub fn list(&self) -> Vec<Value> {
        let jobs = self.jobs.lock().unwrap();
        let mut out: Vec<Value> = jobs.iter().map(|(id, j)| j.to_public(id)).collect();
        out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        out
    }

    /// Recently finished artifacts, newest first, read from disk so
    /// history survives daemon restarts. `n` defaults to 5.
    pub fn last(&self, n: usize) -> Vec<Value> {
        let done = self.dir.join("done");
        let mut items: Vec<_> = std::fs::read_dir(&done)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .collect()
            })
            .unwrap_or_default();
        items.sort_by_key(|e| std::cmp::Reverse(e.metadata().ok().and_then(|m| m.modified().ok())));
        items
            .into_iter()
            .take(n)
            .filter_map(|e| {
                std::fs::read_to_string(e.path())
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
            })
            .collect()
    }

    fn write_result(&self, id: &str, artifact: &Value) {
        self.done_count.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join("done").join(format!("{id}.result.json"));
        if let Err(e) = std::fs::write(&path, serde_json::to_vec(artifact).unwrap_or_default()) {
            self.log(&format!("write {path:?} failed: {e}"));
        }
        self.gc_done();
    }

    fn gc_done(&self) {
        let done = self.dir.join("done");
        let Ok(rd) = std::fs::read_dir(&done) else {
            return;
        };
        let mut items: Vec<_> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .collect();
        let total = items.len();
        if total <= RESULT_KEEP {
            return;
        }
        items.sort_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()));
        for e in items.into_iter().take(total - RESULT_KEEP) {
            let _ = std::fs::remove_file(e.path());
        }
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let jobs = self.jobs.lock().unwrap();
        let pending = jobs.values().filter(|j| j.state == State::Pending).count();
        let running = jobs.values().filter(|j| j.state == State::Running).count();
        (pending, running, self.done_count.load(Ordering::Relaxed))
    }

    pub fn daemon_status(&self, worker_alive: bool, worker_born_at: Option<String>) -> Value {
        let (pending, running, done) = self.counts();
        json!({
            "pid": std::process::id(),
            "started": now_iso(),
            "uptime_s": self.started.elapsed().as_secs(),
            "pwsh_version": self.pwsh_version,
            "mode": "jobd",
            "worker": {"alive": worker_alive, "born_at": worker_born_at},
            "queue": {"pending": pending, "running": running, "done": done},
        })
    }

    pub fn write_heartbeat(&self, worker_alive: bool, worker_born_at: Option<String>) {
        let status = self.daemon_status(worker_alive, worker_born_at);
        let path = self.dir.join("daemon.json");
        if let Err(e) = std::fs::write(&path, serde_json::to_vec(&status).unwrap_or_default()) {
            self.log(&format!("heartbeat write failed: {e}"));
        }
    }

    pub fn log(&self, line: &str) {
        let path = self.dir.join("daemon.log");
        let stamped = format!("[{}] {line}", now_iso());
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{stamped}");
        }
        eprintln!("{stamped}");
    }

    /// Watchdog fallbacks: control/stop.flag and control/cancel-<id>.flag
    /// (used when the pipe is wedged). Returns true when a stop was
    /// requested.
    pub fn poll_control(&self) -> bool {
        let stop = self.dir.join("control").join("stop.flag");
        if stop.is_file() {
            let _ = std::fs::remove_file(&stop);
            self.log("stop requested via control/stop.flag");
            return true;
        }
        let control = self.dir.join("control");
        if let Ok(rd) = std::fs::read_dir(&control) {
            for e in rd.filter_map(|e| e.ok()) {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(id) = name
                    .strip_prefix("cancel-")
                    .and_then(|s| s.strip_suffix(".flag"))
                {
                    let _ = std::fs::remove_file(e.path());
                    let (ok, state) = self.cancel(id);
                    self.log(&format!(
                        "cancel {id} via control flag: ok={ok} state={state}"
                    ));
                }
            }
        }
        false
    }

    /// Spawn the heartbeat + control watchdog loop.
    pub fn spawn_watchdog(
        self: &Arc<Self>,
        worker: Arc<crate::jobd::worker::WorkerSlot>,
        ct: tokio_util::sync::CancellationToken,
    ) {
        let q = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if ct.is_cancelled() {
                    break;
                }
                let (alive, born) = worker.snapshot();
                q.write_heartbeat(alive, born);
                if q.poll_control() {
                    ct.cancel();
                    break;
                }
                // cancellable sleep: a stop must wake us immediately so
                // the heartbeat file is removed before the exit monitor
                // reaps the process (a plain sleep would race it)
                tokio::select! {
                    _ = tokio::time::sleep(HEARTBEAT_PERIOD) => {}
                    _ = ct.cancelled() => {}
                }
            }
            // remove the heartbeat file on exit (same as daemon.ps1)
            let _ = std::fs::remove_file(q.dir.join("daemon.json"));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "vmi-jobd-queue-{tag}-{}-{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn job_lifecycle_and_disk_artifact() {
        let dir = temp_dir("lifecycle");
        let q = Queue::new(dir.clone(), "7.5.0".into()).unwrap();
        q.submit("j1", "vm-list");
        assert_eq!(q.status("j1").unwrap()["state"], "pending");
        q.mark_running("j1");
        assert_eq!(q.status("j1").unwrap()["state"], "running");
        q.finish("j1", json!({"ok": true, "data": []}));
        let st = q.status("j1").unwrap();
        assert_eq!(st["state"], "done");
        assert_eq!(st["envelope"]["ok"], true);
        // the artifact on disk carries today's shape (id/op/state/envelope)
        let raw = std::fs::read_to_string(dir.join("done").join("j1.result.json")).unwrap();
        let artifact: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(artifact["id"], "j1");
        assert_eq!(artifact["op"], "vm-list");
        assert_eq!(artifact["state"], "done");
        // last() reads it back from disk
        let last = q.last(5);
        assert_eq!(last.len(), 1);
        assert_eq!(last[0]["id"], "j1");
    }

    #[test]
    fn cancel_semantics() {
        let dir = temp_dir("cancel");
        let q = Queue::new(dir, "7.5.0".into()).unwrap();
        q.submit("p1", "vm-list");
        let (ok, state) = q.cancel("p1");
        assert!((ok, state) == (true, "cancelled"));
        // second cancel of a cancelled job: not cancelled again
        let (ok, state) = q.cancel("p1");
        assert!((ok, state) == (false, "cancelled"));
        // running jobs are reported, not preempted (serial worker)
        q.submit("r1", "vm-status");
        q.mark_running("r1");
        let (ok, state) = q.cancel("r1");
        assert!((ok, state) == (false, "running"));
        // unknown id
        let (ok, state) = q.cancel("nope");
        assert!((ok, state) == (false, "unknown"));
    }

    #[test]
    fn gc_keeps_last_100() {
        let dir = temp_dir("gc");
        let q = Queue::new(dir.clone(), "7.5.0".into()).unwrap();
        for i in 0..105 {
            let id = format!("g{i:03}");
            q.submit(&id, "noop");
            q.finish(&id, json!({"ok": true, "seq": i}));
        }
        let remaining = std::fs::read_dir(dir.join("done")).unwrap().count();
        assert_eq!(remaining, RESULT_KEEP);
        // newest artifacts survive, oldest evicted
        let last = q.last(2);
        assert_eq!(last[0]["id"], "g104");
        assert_eq!(last[1]["id"], "g103");
    }

    #[test]
    fn control_flags() {
        let dir = temp_dir("control");
        let q = Queue::new(dir.clone(), "7.5.0".into()).unwrap();
        // cancel flag applies to a pending job, poll returns false (no stop)
        q.submit("c1", "vm-list");
        std::fs::write(dir.join("control").join("cancel-c1.flag"), b"").unwrap();
        assert!(!q.poll_control());
        assert_eq!(q.status("c1").unwrap()["state"], "cancelled");
        // stop flag stops the daemon loop
        std::fs::write(dir.join("control").join("stop.flag"), b"").unwrap();
        assert!(q.poll_control());
    }

    #[test]
    fn heartbeat_and_status_shape() {
        let dir = temp_dir("heartbeat");
        let q = Queue::new(dir.clone(), "7.5.0".into()).unwrap();
        q.write_heartbeat(true, Some("2026-10-19T00:00:00Z".into()));
        let raw = std::fs::read_to_string(dir.join("daemon.json")).unwrap();
        let hb: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(hb["mode"], "jobd");
        assert_eq!(hb["pwsh_version"], "7.5.0");
        assert_eq!(hb["worker"]["alive"], true);
        assert_eq!(hb["pid"], std::process::id());
        let (pending, running, done) = q.counts();
        assert_eq!((pending, running, done), (0, 0, 0));
    }

    #[test]
    fn ids_are_unique_and_hexish() {
        let dir = temp_dir("ids");
        let q = Queue::new(dir, "7.5.0".into()).unwrap();
        let a = q.new_id();
        let b = q.new_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn now_iso_shape() {
        let t = now_iso();
        assert_eq!(t.len(), 20);
        assert!(t.starts_with("20"));
        assert_eq!(&t[10..11], "T");
        assert!(t.ends_with('Z'));
    }
}
