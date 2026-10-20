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
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::poison_lock;

pub const RESULT_KEEP: usize = 100;
const HEARTBEAT_PERIOD: std::time::Duration = std::time::Duration::from_secs(5);
/// Extra slack past a job's recorded timeout before the supervisor reaps
/// it: the dispatcher itself gets timeout + kill margin to finish the op
/// normally, so a reap only fires when it is genuinely wedged.
const REAP_GRACE_S: u64 = 60;

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
    /// execution timeout (seconds) — set at mark_running, consumed by the
    /// stale-job supervisor
    timeout_secs: u64,
    started_instant: Option<std::time::Instant>,
}

impl Job {
    fn new(op: String) -> Self {
        Self {
            op,
            state: State::Pending,
            envelope: None,
            submitted_at: now_iso(),
            started_at: None,
            finished_at: None,
            timeout_secs: 600,
            started_instant: None,
        }
    }
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
    /// supervisor reaped-job total (surfaced in daemon_status for doctor)
    reaped_count: AtomicUsize,
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
            reaped_count: AtomicUsize::new(0),
            done_count: std::sync::atomic::AtomicUsize::new(0),
        }))
    }

    pub fn new_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:08x}", self.boot_nonce.wrapping_add(n) & 0xFFFF_FFFF)
    }

    pub fn submit(&self, id: &str, op: &str) {
        self.log(&format!("job {id} submitted: {op}"));
        poison_lock(&self.jobs).insert(id.to_string(), Job::new(op.to_string()));
    }

    /// pending -> running, recording the execution deadline the supervisor
    /// enforces. Returns false when the job is no longer pending (cancelled
    /// while queued) so the dispatcher skips it instead of running a
    /// cancelled op.
    pub fn mark_running(&self, id: &str, timeout_secs: u64) -> bool {
        let mut jobs = poison_lock(&self.jobs);
        match jobs.get_mut(id) {
            Some(j) if j.state == State::Pending => {
                j.state = State::Running;
                j.timeout_secs = timeout_secs;
                j.started_instant = Some(std::time::Instant::now());
                j.started_at = Some(now_iso());
                true
            }
            _ => false,
        }
    }

    /// Supervisor: force-finish running jobs that blew past their
    /// execution timeout + grace. Root-cause hardening for the observed
    /// "stuck running forever" dispatcher disease (2026-10-19/20): whatever
    /// stalls the normal completion path — worker tree wedged mid-kill,
    /// dispatcher task lost — this converts the stall into a terminal
    /// op_timeout envelope so waiters always get closure and the queue
    /// keeps draining. Returns the reaped job ids.
    pub fn reap_stale(&self, grace_secs: u64) -> Vec<String> {
        let reaped: Vec<(String, Value)> = {
            let mut jobs = poison_lock(&self.jobs);
            let now = std::time::Instant::now();
            let stale: Vec<String> = jobs
                .iter()
                .filter(|(_, j)| {
                    j.state == State::Running
                        && j.started_instant
                            .map(|t| now.duration_since(t).as_secs() > j.timeout_secs + grace_secs)
                            .unwrap_or(false)
                })
                .map(|(id, _)| id.clone())
                .collect();
            stale
                .into_iter()
                .filter_map(|id| {
                    let j = jobs.get_mut(&id)?;
                    j.state = State::Done;
                    j.finished_at = Some(now_iso());
                    j.envelope = Some(json!({
                        "ok": false,
                        "error": {
                            "code": "op_timeout",
                            "message": format!(
                                "job reaped by supervisor: ran > {}s (timeout {}s + grace {}s)",
                                j.timeout_secs + grace_secs, j.timeout_secs, grace_secs
                            ),
                        }
                    }));
                    let artifact = j.to_public(&id);
                    Some((id, artifact))
                })
                .collect()
        };
        // artifacts + logging outside the lock
        for (id, artifact) in &reaped {
            self.log(&format!("job {id} reaped by supervisor (op_timeout)"));
            self.write_result(id, artifact);
        }
        self.reaped_count.fetch_add(reaped.len(), Ordering::Relaxed);
        reaped.into_iter().map(|(id, _)| id).collect()
    }

    /// running -> done with the worker's envelope. A job already terminal
    /// (supervisor-reaped or cancelled) keeps its first verdict — a late
    /// normal finish must not resurrect a reaped job.
    pub fn finish(&self, id: &str, envelope: Value) {
        let artifact = {
            let mut jobs = poison_lock(&self.jobs);
            match jobs.get_mut(id) {
                Some(j) if j.state == State::Running => {
                    j.state = State::Done;
                    j.finished_at = Some(now_iso());
                    j.envelope = Some(envelope.clone());
                    j.to_public(id)
                }
                Some(j) => {
                    self.log(&format!(
                        "job {id} finish dropped: already {} (supervisor won)",
                        j.state.as_str()
                    ));
                    return;
                }
                None => return,
            }
        };
        self.write_result(id, &artifact);
        self.log(&format!("job {id} done"));
    }

    /// pending -> cancelled; running/done/cancelled reported as-is.
    pub fn cancel(&self, id: &str) -> (bool, &'static str) {
        let mut jobs = poison_lock(&self.jobs);
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
        let jobs = poison_lock(&self.jobs);
        jobs.get(id).map(|j| j.to_public(id))
    }

    pub fn list(&self) -> Vec<Value> {
        let jobs = poison_lock(&self.jobs);
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
        let jobs = poison_lock(&self.jobs);
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
            "supervisor": {"reaped": self.reaped_count.load(Ordering::Relaxed)},
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

    /// Spawn the heartbeat + control watchdog loop. Each tick also runs the
    /// stale-job supervisor: reaped jobs get terminal op_timeout envelopes,
    /// and the in-flight worker tree is killed so a dispatcher parked on
    /// its roundtrip wakes up (EOF -> error path -> next op cold-boots).
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
                let reaped = q.reap_stale(REAP_GRACE_S);
                if !reaped.is_empty() {
                    let killed = worker.kill_running().await;
                    q.log(&format!(
                        "supervisor: reaped {} job(s) [{}], in-flight worker killed: {}",
                        reaped.len(),
                        reaped.join(","),
                        killed
                    ));
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
        assert!(q.mark_running("j1", 600));
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
        // a job cancelled while queued is never dispatched
        assert!(!q.mark_running("p1", 600));
        // running jobs are reported, not preempted (serial worker)
        q.submit("r1", "vm-status");
        assert!(q.mark_running("r1", 600));
        let (ok, state) = q.cancel("r1");
        assert!((ok, state) == (false, "running"));
        // unknown id
        let (ok, state) = q.cancel("nope");
        assert!((ok, state) == (false, "unknown"));
    }

    #[test]
    fn reap_stale_force_finishes_wedged_running_jobs() {
        let dir = temp_dir("reap");
        let q = Queue::new(dir.clone(), "7.5.0".into()).unwrap();
        q.submit("w1", "vm-wait-state");
        assert!(q.mark_running("w1", 0)); // timeout 0: stale as soon as time passes
                                          // within grace: untouched
        assert!(q.reap_stale(600).is_empty());
        assert_eq!(q.status("w1").unwrap()["state"], "running");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // past timeout + grace: forced terminal with op_timeout evidence
        let reaped = q.reap_stale(0);
        assert_eq!(reaped, vec!["w1".to_string()]);
        let st = q.status("w1").unwrap();
        assert_eq!(st["state"], "done");
        assert_eq!(st["envelope"]["error"]["code"], "op_timeout");
        // the artifact landed on disk like any terminal job
        let raw = std::fs::read_to_string(dir.join("done").join("w1.result.json")).unwrap();
        let artifact: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(artifact["envelope"]["error"]["code"], "op_timeout");
    }

    #[test]
    fn late_finish_after_reap_does_not_resurrect() {
        let dir = temp_dir("late");
        let q = Queue::new(dir, "7.5.0".into()).unwrap();
        q.submit("w2", "vm-list");
        assert!(q.mark_running("w2", 0));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(q.reap_stale(0).len(), 1);
        // the dispatcher eventually unwinds with the real (stale) envelope:
        // the supervisor's terminal verdict wins
        q.finish("w2", json!({"ok": true, "data": []}));
        let st = q.status("w2").unwrap();
        assert_eq!(st["state"], "done");
        assert_eq!(st["envelope"]["error"]["code"], "op_timeout");
    }

    #[test]
    fn healthy_running_job_is_not_reaped() {
        let dir = temp_dir("healthy");
        let q = Queue::new(dir, "7.5.0".into()).unwrap();
        q.submit("h1", "vm-list");
        assert!(q.mark_running("h1", 600));
        assert!(q.reap_stale(REAP_GRACE_S).is_empty());
        assert_eq!(q.status("h1").unwrap()["state"], "running");
        // and it can still finish normally afterwards
        q.finish("h1", json!({"ok": true}));
        assert_eq!(q.status("h1").unwrap()["envelope"]["ok"], true);
    }

    #[test]
    fn gc_keeps_last_100() {
        let dir = temp_dir("gc");
        let q = Queue::new(dir.clone(), "7.5.0".into()).unwrap();
        for i in 0..105 {
            let id = format!("g{i:03}");
            q.submit(&id, "noop");
            assert!(q.mark_running(&id, 600));
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
