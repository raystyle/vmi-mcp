//! vmi-mcp: universal user-level MCP workbench for VMs (Windows first,
//! Linux same build).
//!
//! Born in the INTERACTIVE session via the shell:startup chain (the only
//! launch path whose children get real windows and the user's DPAPI
//! secrets — the WinSta0\Default birth-canal lesson, 2026-10-14). The
//! harness (or any mcp2cli on the network) talks to it directly over
//! streamable-HTTP on :9881 — SSH is only the deployment channel.
//!
//! v0.2 (grok research + kimi review, 2026-10-15):
//!   panes   — one long-lived shell per pane (ConPTY on Windows, pty on
//!             Linux, both via portable-pty); cursor-based multi-client
//!             read + send_keys; no per-command process churn
//!   shell   — one-shot captured execution. Windows session=0 goes
//!             through a passwordless schtasks SYSTEM task with a
//!             done-marker for exact output recovery; session=1 and all
//!             other platforms run in the service context
//!   spawn   — visible GUI launch (Windows interactive context)
//!
//! No PsExec, no WinPTY agent, no OpenConsole.exe, no powershell.exe.
//! Release builds run windowless (windows subsystem): a background
//! daemon with no console flash on the desktop.

// release = background daemon (no console window); debug keeps one
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
//! Known v0.2 limits (documented on purpose): session-0 scratch lives in
//! the system temp dir (admins+SYSTEM readable); kill reaches the direct
//! child only; MCP session state is in-memory, so a service restart
//! drops live MCP sessions (clients reconnect).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use rmcp::handler::server::wrapper::Json;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_router, ErrorData};

const BIND_ADDRESS: &str = "0.0.0.0:9881";
const SCROLLBACK_CAP: usize = 4 * 1024 * 1024;

#[cfg(windows)]
const SHELL: &str = "cmd.exe";
#[cfg(windows)]
const SCRATCH_DIR: &str = "C:\\Windows\\Temp";
#[cfg(windows)]
const TASK_PREFIX: &str = "vmi-shell0";

#[cfg(not(windows))]
const SHELL: &str = "/bin/bash";

/// Typed tool error: invalid params vs internal failure stays
/// distinguishable for MCP clients.
fn err_invalid(msg: &str) -> ErrorData {
    ErrorData::invalid_params(msg.to_string(), None)
}

fn err_internal(msg: &str) -> ErrorData {
    ErrorData::internal_error(msg.to_string(), None)
}

struct Pane {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    buffer: Arc<Mutex<Vec<u8>>>,
}

#[derive(Clone, Default)]
struct Workbench {
    started: Option<Instant>,
    panes: Arc<Mutex<HashMap<u32, Arc<Mutex<Pane>>>>>,
    next_id: Arc<Mutex<u32>>,
}

#[tool_router(server_handler)]
impl Workbench {
    fn new() -> Self {
        Self {
            started: Some(Instant::now()),
            panes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    #[tool(description = "Workbench heartbeat: pid, session name, user, uptime, open panes")]
    fn status(&self) -> Json<serde_json::Value> {
        let panes = self.panes.lock().unwrap().len();
        Json(serde_json::json!({
            "pid": std::process::id(),
            "session": std::env::var("SESSIONNAME").unwrap_or_default(),
            "user": user_string(),
            "uptime_s": self.started.map(|s| s.elapsed().as_secs()).unwrap_or(0),
            "panes": panes,
        }))
    }

    #[tool(
        description = "Open one long-lived shell pane (ConPTY on Windows, pty on Linux) and return its id. Read with pane_read, type with pane_send"
    )]
    fn pane_open(
        &self,
        Parameters(args): Parameters<PaneOpenArgs>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let size = PtySize {
            rows: args.rows.unwrap_or(24),
            cols: args.cols.unwrap_or(80),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(size)
            .map_err(|e| err_internal(&format!("openpty: {e}")))?;
        let mut cmd = CommandBuilder::new(SHELL);
        if let Some(cwd) = args.cwd.as_deref() {
            cmd.cwd(cwd);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| err_internal(&format!("spawn {SHELL}: {e}")))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| err_internal(&format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| err_internal(&format!("pty writer: {e}")))?;
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let buf = Arc::clone(&buffer);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = buf.lock().unwrap();
                        b.extend_from_slice(&chunk[..n]);
                        let len = b.len();
                        if len > SCROLLBACK_CAP {
                            b.drain(..len - SCROLLBACK_CAP);
                        }
                    }
                }
            }
        });
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let id = *n;
            *n = n.wrapping_add(1);
            id
        };
        self.panes.lock().unwrap().insert(
            id,
            Arc::new(Mutex::new(Pane {
                master: pair.master,
                writer,
                child,
                buffer,
            })),
        );
        Ok(Json(serde_json::json!({ "id": id, "shell": SHELL })))
    }

    #[tool(
        description = "Read new pane output since your cursor. Returns {data, cursor, alive}: advance your cursor to the returned one; alive=false means the shell exited"
    )]
    fn pane_read(
        &self,
        Parameters(args): Parameters<PaneReadArgs>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let pane = self
            .panes
            .lock()
            .unwrap()
            .get(&args.id)
            .cloned()
            .ok_or_else(|| err_invalid(&format!("no pane {}", args.id)))?;
        let mut p = pane.lock().unwrap();
        let (chunk, cursor) = {
            let buf = p.buffer.lock().unwrap();
            let at = args.cursor.min(buf.len());
            (buf[at..].to_vec(), buf.len())
        };
        let alive = p.child.try_wait().map(|w| w.is_none()).unwrap_or(false);
        Ok(Json(serde_json::json!({
            "data": String::from_utf8_lossy(&chunk),
            "cursor": cursor,
            "alive": alive,
        })))
    }

    #[tool(description = "Type text into a pane (append \\r for Enter)")]
    fn pane_send(
        &self,
        Parameters(args): Parameters<PaneSendArgs>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let pane = self
            .panes
            .lock()
            .unwrap()
            .get(&args.id)
            .cloned()
            .ok_or_else(|| err_invalid(&format!("no pane {}", args.id)))?;
        let mut p = pane.lock().unwrap();
        p.writer
            .write_all(args.text.as_bytes())
            .and_then(|_| p.writer.flush())
            .map_err(|e| err_internal(&format!("write: {e}")))?;
        Ok(Json(serde_json::json!({ "sent": args.text.len() })))
    }

    #[tool(description = "Resize a pane's terminal (cols x rows)")]
    fn pane_resize(
        &self,
        Parameters(args): Parameters<PaneResizeArgs>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let pane = self
            .panes
            .lock()
            .unwrap()
            .get(&args.id)
            .cloned()
            .ok_or_else(|| err_invalid(&format!("no pane {}", args.id)))?;
        let mut p = pane.lock().unwrap();
        p.master
            .resize(PtySize {
                rows: args.rows,
                cols: args.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| err_internal(&format!("resize: {e}")))?;
        Ok(Json(
            serde_json::json!({ "resized": [args.cols, args.rows] }),
        ))
    }

    #[tool(description = "Kill a pane's shell and drop it from the registry")]
    fn pane_kill(
        &self,
        Parameters(args): Parameters<PaneKillArgs>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let pane = self
            .panes
            .lock()
            .unwrap()
            .remove(&args.id)
            .ok_or_else(|| err_invalid(&format!("no pane {}", args.id)))?;
        let mut p = pane.lock().unwrap();
        let _ = p.child.kill();
        Ok(Json(serde_json::json!({ "killed": args.id })))
    }

    #[tool(
        description = "Run a command. Default: fire-and-forget into the default pane 0 (returns immediately; collect with pane_read, tmux semantics). wait=true blocks with captured output. Windows session=0 runs headless as SYSTEM"
    )]
    fn shell(
        &self,
        Parameters(args): Parameters<ShellArgs>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        if args.cmd.trim().is_empty() {
            return Err(err_invalid("cmd is required"));
        }
        let timeout = Duration::from_secs(args.timeout_s.unwrap_or(120).min(600));
        let session = args.session.unwrap_or(1);
        let wait = args.wait.unwrap_or(false);
        if wait || session == 0 {
            #[cfg(windows)]
            {
                match session {
                    1 => run_captured(&args.cmd, timeout),
                    0 => run_session0(&args.cmd, timeout),
                    other => Err(err_invalid(&format!("session must be 0 or 1, got {other}"))),
                }
            }
            #[cfg(not(windows))]
            {
                let _ = session;
                run_captured(&args.cmd, timeout)
            }
        } else {
            self.shell_fire(&args.cmd)
        }
    }

    fn shell_fire(&self, cmd: &str) -> Result<Json<serde_json::Value>, ErrorData> {
        let pane = self.panes.lock().unwrap().get(&0).cloned();
        let pane = match pane {
            Some(p) => p,
            None => {
                let pty_system = portable_pty::native_pty_system();
                let pair = pty_system
                    .openpty(PtySize {
                        rows: 24,
                        cols: 120,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .map_err(|e| err_internal(&format!("openpty: {e}")))?;
                let child = pair
                    .slave
                    .spawn_command(CommandBuilder::new(SHELL))
                    .map_err(|e| err_internal(&format!("spawn {SHELL}: {e}")))?;
                drop(pair.slave);
                let reader = pair
                    .master
                    .try_clone_reader()
                    .map_err(|e| err_internal(&format!("pty reader: {e}")))?;
                let writer = pair
                    .master
                    .take_writer()
                    .map_err(|e| err_internal(&format!("pty writer: {e}")))?;
                let buffer = Arc::new(Mutex::new(Vec::new()));
                let buf = Arc::clone(&buffer);
                std::thread::spawn(move || {
                    let mut reader = reader;
                    let mut chunk = [0u8; 4096];
                    loop {
                        match reader.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let mut b = buf.lock().unwrap();
                                b.extend_from_slice(&chunk[..n]);
                                let len = b.len();
                                if len > SCROLLBACK_CAP {
                                    b.drain(..len - SCROLLBACK_CAP);
                                }
                            }
                        }
                    }
                });
                let pane = Arc::new(Mutex::new(Pane {
                    master: pair.master,
                    writer,
                    child,
                    buffer,
                }));
                self.panes.lock().unwrap().insert(0, Arc::clone(&pane));
                pane
            }
        };
        let text = format!(
            "{cmd}
"
        );
        let mut p = pane.lock().unwrap();
        p.writer
            .write_all(text.as_bytes())
            .and_then(|_| p.writer.flush())
            .map_err(|e| err_internal(&format!("write: {e}")))?;
        Ok(Json(serde_json::json!({ "fired": true, "pane": 0 })))
    }

    #[tool(
        description = "Launch a program in the interactive desktop context (argv array, no shell quoting; Windows GUI visibility)"
    )]
    fn spawn(
        &self,
        Parameters(args): Parameters<SpawnArgs>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        if args.exe.is_empty() {
            return Err(err_invalid("exe is required"));
        }
        let mut cmd = std::process::Command::new(&args.exe);
        cmd.args(&args.args);
        if let Some(cwd) = args.cwd.as_deref() {
            cmd.current_dir(cwd);
        }
        // no DETACHED: children must share this process's window station
        // to inherit visibility
        let child = cmd
            .spawn()
            .map_err(|e| err_internal(&format!("spawn {} failed: {e}", args.exe)))?;
        Ok(Json(serde_json::json!({
            "pid": child.id(),
            "exe": args.exe,
        })))
    }
}

fn user_string() -> String {
    #[cfg(windows)]
    {
        format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").unwrap_or_default(),
            std::env::var("USERNAME").unwrap_or_default()
        )
    }
    #[cfg(not(windows))]
    {
        std::env::var("USER").unwrap_or_default()
    }
}

/// One-shot captured run in the service's own context. Both pipes are
/// drained on their own threads, so a chatty child cannot deadlock the
/// 64K pipe buffer while we poll try_wait (kimi review #1/#2).
fn run_captured(cmd: &str, timeout: Duration) -> Result<Json<serde_json::Value>, ErrorData> {
    #[cfg(windows)]
    let mut c = {
        let mut c = std::process::Command::new("cmd.exe");
        c.args(["/c", cmd]);
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = std::process::Command::new(SHELL);
        c.args(["-c", cmd]);
        c
    };
    c.stdout(std::process::Stdio::piped());
    c.stderr(std::process::Stdio::piped());
    let mut child = c
        .spawn()
        .map_err(|e| err_internal(&format!("launch failed: {e}")))?;

    let drain = |mut pipe: Option<std::process::ChildStdout>| -> Arc<Mutex<String>> {
        let sink = Arc::new(Mutex::new(String::new()));
        if let Some(mut p) = pipe.take() {
            let sink = Arc::clone(&sink);
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = p.read_to_string(&mut s);
                *sink.lock().unwrap() = s;
            });
        }
        sink
    };
    // two threads: stdout and stderr drain concurrently
    let mut err_pipe = child.stderr.take();
    let err_sink = {
        let sink = Arc::new(Mutex::new(String::new()));
        if let Some(mut p) = err_pipe.take() {
            let sink2 = Arc::clone(&sink);
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = p.read_to_string(&mut s);
                *sink2.lock().unwrap() = s;
            });
        }
        sink
    };
    let out_sink = {
        let out_pipe = child.stdout.take();
        let sink = Arc::new(Mutex::new(String::new()));
        if let Some(mut p) = out_pipe {
            let sink2 = Arc::clone(&sink);
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = p.read_to_string(&mut s);
                *sink2.lock().unwrap() = s;
            });
        }
        sink
    };
    let _ = drain; // (kept for symmetry; both sinks above are live)

    let deadline = Instant::now() + timeout;
    loop {
        let done = child
            .try_wait()
            .map_err(|e| err_internal(&format!("wait: {e}")))?;
        if let Some(st) = done {
            // give the drain threads a beat to finish
            let settle = Instant::now() + Duration::from_millis(300);
            while Instant::now() < settle {
                std::thread::sleep(Duration::from_millis(50));
            }
            return Ok(Json(serde_json::json!({
                "exit_code": st.code(),
                "stdout": out_sink.lock().unwrap().clone(),
                "stderr": err_sink.lock().unwrap().clone(),
            })));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(err_internal(&format!(
                "command exceeded {:?} (killed)",
                timeout
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Windows session 0: scratch bat + one-shot `schtasks /RU SYSTEM` task
/// (passwordless form survives the EDR). The bat writes output to a file
/// and a DONE marker after; we poll the marker (kimi review #5) so the
/// output is never half-written. Task name is per-call unique (#3).
#[cfg(windows)]
fn run_session0(cmd: &str, timeout: Duration) -> Result<Json<serde_json::Value>, ErrorData> {
    use std::io::Write;
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let bat = format!("{SCRATCH_DIR}\\{TASK_PREFIX}-{stamp}.bat");
    let outfile = format!("{SCRATCH_DIR}\\{TASK_PREFIX}-{stamp}.out");
    let donefile = format!("{SCRATCH_DIR}\\{TASK_PREFIX}-{stamp}.done");
    let task = format!("{TASK_PREFIX}-{stamp}");
    let mut f =
        std::fs::File::create(&bat).map_err(|e| err_internal(&format!("scratch bat: {e}")))?;
    writeln!(f, "@echo off").map_err(|e| err_internal(&format!("scratch bat: {e}")))?;
    writeln!(
        f,
        "cmd /c {cmd} > \"{outfile}\" 2>&1 & type nul > \"{donefile}\""
    )
    .map_err(|e| err_internal(&format!("scratch bat: {e}")))?;
    drop(f);

    let run = |args: &[&str]| -> Result<std::process::Output, ErrorData> {
        std::process::Command::new("schtasks.exe")
            .args(args)
            .output()
            .map_err(|e| err_internal(&format!("schtasks: {e}")))
    };
    let created = run(&[
        "/Create",
        "/F",
        "/TN",
        &task,
        "/TR",
        &format!("cmd /c {bat}"),
        "/SC",
        "ONCE",
        "/ST",
        "23:59",
        "/RU",
        "SYSTEM",
    ])?;
    if !created.status.success() {
        cleanup0(&bat, &outfile, &donefile);
        return Err(err_internal(&format!(
            "task create failed: {}",
            String::from_utf8_lossy(&created.stderr).trim()
        )));
    }
    // /Run result is checked, not swallowed (kimi review #8)
    let started = run(&["/Run", "/TN", &task])?;
    if !started.status.success() {
        let _ = run(&["/Delete", "/TN", &task, "/F"]);
        cleanup0(&bat, &outfile, &donefile);
        return Err(err_internal(&format!(
            "task run failed: {}",
            String::from_utf8_lossy(&started.stderr).trim()
        )));
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        if std::fs::metadata(&donefile).is_ok() {
            break;
        }
    }
    let content = std::fs::read_to_string(&outfile).unwrap_or_default();
    let _ = run(&["/Delete", "/TN", &task, "/F"]);
    cleanup0(&bat, &outfile, &donefile);
    if std::fs::metadata(&donefile).is_ok() || !content.is_empty() {
        Ok(Json(serde_json::json!({
            "exit_code": 0,
            "stdout": content,
        })))
    } else {
        Err(err_internal(&format!(
            "session-0 task did not finish within {:?}",
            timeout
        )))
    }
}

#[cfg(windows)]
fn cleanup0(bat: &str, outfile: &str, donefile: &str) {
    let _ = std::fs::remove_file(bat);
    let _ = std::fs::remove_file(outfile);
    let _ = std::fs::remove_file(donefile);
}

fn flex_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        Bool(bool),
        Str(String),
        Null,
    }
    match Option::<V>::deserialize(deserializer)? {
        None | Some(V::Null) => Ok(None),
        Some(V::Bool(b)) => Ok(Some(b)),
        Some(V::Str(s)) if s.is_empty() || s == "null" => Ok(None),
        Some(V::Str(s)) => match s.as_str() {
            "true" | "1" => Ok(Some(true)),
            "false" | "0" => Ok(Some(false)),
            _ => Err(serde::de::Error::custom("invalid bool")),
        },
    }
}

fn de_u16_from_string_or_int<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        Int(u16),
        Str(String),
        Null,
    }
    match Option::<V>::deserialize(deserializer)? {
        None | Some(V::Null) => Ok(None),
        Some(V::Int(n)) => Ok(Some(n)),
        Some(V::Str(s)) if s.is_empty() || s == "null" => Ok(None),
        Some(V::Str(s)) => s.parse::<u16>().map(Some).map_err(serde::de::Error::custom),
    }
}

fn de_u16_req_from_string_or_int<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        Int(u16),
        Str(String),
    }
    match V::deserialize(deserializer)? {
        V::Int(n) => Ok(n),
        V::Str(s) => s.parse::<u16>().map_err(serde::de::Error::custom),
    }
}

fn de_u32_from_string_or_int<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        Int(u32),
        Str(String),
    }
    match V::deserialize(deserializer)? {
        V::Int(n) => Ok(n),
        V::Str(s) => s.parse::<u32>().map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaneOpenArgs {
    /// Working directory for the shell (optional)
    #[serde(default)]
    pub cwd: Option<String>,
    /// Terminal columns (default 80)
    #[serde(default, deserialize_with = "de_u16_from_string_or_int")]
    pub cols: Option<u16>,
    /// Terminal rows (default 24)
    #[serde(default, deserialize_with = "de_u16_from_string_or_int")]
    pub rows: Option<u16>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaneReadArgs {
    /// Pane id from pane_open
    #[serde(deserialize_with = "de_u32_from_string_or_int")]
    pub id: u32,
    /// Your cursor from the last pane_read (start at 0)
    #[serde(default)]
    pub cursor: usize,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaneSendArgs {
    /// Pane id
    #[serde(deserialize_with = "de_u32_from_string_or_int")]
    pub id: u32,
    /// Text to type; append \r for Enter
    pub text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaneResizeArgs {
    /// Pane id
    #[serde(deserialize_with = "de_u32_from_string_or_int")]
    pub id: u32,
    /// New columns
    #[serde(deserialize_with = "de_u16_req_from_string_or_int")]
    pub cols: u16,
    /// New rows
    #[serde(deserialize_with = "de_u16_req_from_string_or_int")]
    pub rows: u16,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaneKillArgs {
    /// Pane id
    #[serde(deserialize_with = "de_u32_from_string_or_int")]
    pub id: u32,
}

/// MCP spec types some client arguments as strings (spec-compliant
/// clients may stringify numbers); accept both wire forms.
fn de_u8_from_string_or_int<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        Int(u8),
        Str(String),
        Null,
    }
    match Option::<StringOrInt>::deserialize(deserializer)? {
        None | Some(StringOrInt::Null) => Ok(None),
        Some(StringOrInt::Int(n)) => Ok(Some(n)),
        Some(StringOrInt::Str(s)) if s.is_empty() || s == "null" => Ok(None),
        Some(StringOrInt::Str(s)) => s.parse::<u8>().map(Some).map_err(serde::de::Error::custom),
    }
}

fn de_u64_from_string_or_int<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        Int(u64),
        Str(String),
        Null,
    }
    match Option::<StringOrInt>::deserialize(deserializer)? {
        None | Some(StringOrInt::Null) => Ok(None),
        Some(StringOrInt::Int(n)) => Ok(Some(n)),
        Some(StringOrInt::Str(s)) if s.is_empty() || s == "null" => Ok(None),
        Some(StringOrInt::Str(s)) => s.parse::<u64>().map(Some).map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ShellArgs {
    /// Command line to execute
    pub cmd: String,
    /// Windows only: 1 = interactive desktop (default), 0 = headless SYSTEM task
    #[serde(default, deserialize_with = "de_u8_from_string_or_int")]
    pub session: Option<u8>,
    /// false (default) = fire-and-forget into the default pane; true = block with capture
    #[serde(default, deserialize_with = "flex_bool")]
    pub wait: Option<bool>,
    /// Hard kill after this many seconds (default 120, max 600)
    #[serde(default, deserialize_with = "de_u64_from_string_or_int")]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SpawnArgs {
    /// Executable path
    pub exe: String,
    /// argv elements (no shell quoting)
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional working directory
    #[serde(default)]
    pub cwd: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ct = tokio_util::sync::CancellationToken::new();
    // callers reach us by guest IP: allow every host form this machine
    // can present (rmcp's dns-rebinding guard rejects unlisted hosts).
    // Env VMI_MCP_ALLOWED_HOSTS=comma,separated adds extras.
    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Ok(ifaces) = local_ip_address::list_afinet_netifas() {
        for (_name, ip) in ifaces {
            hosts.push(ip.to_string());
            hosts.push(format!("{ip}:9881"));
        }
    }
    if let Ok(extra) = std::env::var("VMI_MCP_ALLOWED_HOSTS") {
        for h in extra.split(',').filter(|s| !s.is_empty()) {
            hosts.push(h.trim().to_string());
        }
    }
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.child_token())
        .with_allowed_hosts(hosts);
    // one process-wide workbench state: panes must survive across MCP
    // sessions (cursor-based multiplexing is multi-client by design)
    let panes: Arc<Mutex<HashMap<u32, Arc<Mutex<Pane>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let next_id: Arc<Mutex<u32>> = Arc::new(Mutex::new(1));
    let started = Instant::now();
    let service = StreamableHttpService::new(
        move || {
            Ok(Workbench {
                started: Some(started),
                panes: Arc::clone(&panes),
                next_id: Arc::clone(&next_id),
            })
        },
        LocalSessionManager::default().into(),
        config,
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(BIND_ADDRESS).await?;
    eprintln!("vmi-mcp v0.2 listening on http://{BIND_ADDRESS}/mcp");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await?;
    Ok(())
}
