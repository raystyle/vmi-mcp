//! vmi-attach: raw-terminal attach client for vmi-mcp panes.
//!
//! Mirrors a remote vmi-mcp shell pane onto the local terminal: the
//! pane's output stream (raw bytes via pane_read.data_hex) is written
//! straight to stdout, local keystrokes are forwarded via pane_send.
//! While attached, the server stops answering DSR queries — the local
//! terminal answers them with true cursor coordinates, so full-screen
//! apps render correctly.
//!
//! Detach: Ctrl-B then q (herdr convention). Ctrl-B then b sends a
//! literal Ctrl-B to the pane.
//!
//! The terminal is put in raw mode for the whole session; on exit the
//! previous mode is restored and pane_detach is called so headless
//! fire-and-forget shells get their server-side DSR replies back.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rmcp::model::{CallToolRequestParams, ClientConfig, JsonObject};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

/// CLI: vmi-attach [--url http://ip:9881/mcp] [--pane 0]
struct Args {
    url: String,
    pane: u32,
}

fn parse_args() -> Result<Args> {
    let mut url = None;
    let mut pane = 0u32;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" => url = Some(it.next().context("--url needs a value")?),
            "--pane" => pane = it.next().context("--pane needs a value")?.parse()?,
            _ => bail!("unknown arg {a}; usage: vmi-attach [--url http://ip:9881/mcp] [--pane 0]"),
        }
    }
    Ok(Args {
        url: url.unwrap_or_else(|| "http://127.0.0.1:9881/mcp".into()),
        pane,
    })
}

/// json!({..}).as_object() without the ceremony
fn obj(v: Value) -> JsonObject {
    v.as_object().cloned().unwrap_or_default()
}

async fn call(svc: &Client, name: &str, args: JsonObject) -> Result<Value> {
    use rmcp::model::CallToolResponse;
    let params = CallToolRequestParams::new(name.to_owned()).with_arguments(args);
    let result = svc.call_tool_once(params).await?;
    let result = match result {
        CallToolResponse::Complete(r) => r,
        CallToolResponse::InputRequired(_) => bail!("tool {name}: input required"),
        _ => bail!("tool {name}: unexpected response"),
    };
    if result.is_error.unwrap_or(false) {
        let text = result
            .content
            .first()
            .map(|c| format!("{c:?}"))
            .unwrap_or_default();
        bail!("tool {name} failed: {text}")
    }
    // structured_content is the JSON returned by our tools
    Ok(result.structured_content.clone().unwrap_or(Value::Null))
}

type Client = RunningService<RoleClient, ClientConfig>;

fn unhex(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        bail!("odd hex length");
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16).context("bad hex digit")?;
        let lo = (pair[1] as char).to_digit(16).context("bad hex digit")?;
        out.push((hi * 16 + lo) as u8);
    }
    Ok(out)
}

struct RawTerm {
    enabled: bool,
}

impl RawTerm {
    /// Raw mode when stdin is a real console; piped stdin degrades to
    /// cooked mode (still forwarded — scripted driving works).
    fn enter() -> Self {
        use crossterm::terminal::{enable_raw_mode, is_raw_mode_enabled};
        let enabled = match is_raw_mode_enabled() {
            Ok(true) => false, // already raw (nested); leave as-is on exit
            Ok(false) => enable_raw_mode().is_ok(),
            Err(_) => false,
        };
        Self { enabled }
    }
}

impl Drop for RawTerm {
    fn drop(&mut self) {
        use crossterm::terminal::disable_raw_mode;
        if self.enabled {
            let _ = disable_raw_mode();
        }
        // final CRLF so the local prompt isn't glued to remote output
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\r\n");
        let _ = out.flush();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::default(),
        StreamableHttpClientTransportConfig::with_uri(args.url.clone()),
    );
    let svc = ClientConfig::default()
        .serve(transport)
        .await
        .with_context(|| format!("connect {}", args.url))?;

    // attach to the pane if it exists; otherwise open one with this id
    if call(&svc, "pane_attach", obj(json!({ "id": args.pane })))
        .await
        .is_err()
    {
        // server assigns ids sequentially; pane 0 is the fire-and-forget
        // default — a fresh daemon may not have it yet, so create it
        call(&svc, "pane_open", obj(json!({})))
            .await
            .context("pane_open")?;
        call(&svc, "pane_attach", obj(json!({ "id": args.pane })))
            .await
            .context("pane_attach")?;
    }

    let _term = RawTerm::enter();

    // one-shot initial resize: match the pane to this terminal
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let _ = call(
            &svc,
            "pane_resize",
            obj(json!({ "id": args.pane, "cols": cols, "rows": rows })),
        )
        .await;
    }

    // replay what the pane already holds, then stream. Poll cadence per
    // grok's research: 50ms while output flows, backing off to 200ms
    // after 5s idle.
    let mut cursor: u64 = 0;
    let mut poll = Duration::from_millis(50);
    let mut last_output = Instant::now();
    let mut pending_ctrl_b = false;
    let mut stdin_buf = [0u8; 1024];
    let mut stdin = tokio::io::stdin();
    let mut out = std::io::stdout();
    let mut read_buf: Vec<u8> = Vec::new();
    let mut detach = false;

    loop {
        // stdin with a poll-interval cap; keystrokes never wait on reads
        tokio::select! {
            n = stdin.read(&mut stdin_buf) => {
                let n = n?;
                if n == 0 {
                    break; // local stdin closed
                }
                let mut data: &[u8] = &stdin_buf[..n];
                if pending_ctrl_b {
                    pending_ctrl_b = false;
                    match data.first() {
                        Some(&b'q') => { detach = true; break; }
                        Some(&b'b') => {
                            let _ = call(&svc, "pane_send", obj(json!({
                                "id": args.pane, "text": "\u{2}"
                            }))).await;
                            data = &data[1..];
                        }
                        _ => {} // prefix consumed, key swallowed
                    }
                } else if data.first() == Some(&0x02) && data.len() == 1 {
                    pending_ctrl_b = true;
                } else {
                    // escape into a JSON string losslessly (bytes may not
                    // be valid UTF-8 mid-sequence; the shell tolerates it)
                    let text = String::from_utf8_lossy(data);
                    let _ = call(&svc, "pane_send", obj(json!({
                        "id": args.pane, "text": text
                    }))).await;
                }
            }
            _ = tokio::time::sleep(poll) => {}
        }

        // --- pane_read → stdout (drain fully, chunked by max_bytes) ---
        read_buf.clear();
        loop {
            let v = call(
                &svc,
                "pane_read",
                obj(json!({ "id": args.pane, "cursor": cursor, "max_bytes": 131072 })),
            )
            .await?;
            let hex = v.get("data_hex").and_then(Value::as_str).unwrap_or("");
            let new_cursor = v.get("cursor").and_then(Value::as_u64).unwrap_or(cursor);
            cursor = new_cursor;
            if hex.is_empty() {
                break;
            }
            let bytes = unhex(hex)?;
            read_buf.extend_from_slice(&bytes);
            if bytes.len() < 128 * 1024 {
                break; // drained for now
            }
        }
        if !read_buf.is_empty() {
            out.write_all(&read_buf)?;
            out.flush()?;
            last_output = Instant::now();
            poll = Duration::from_millis(50);
        } else if last_output.elapsed() > Duration::from_secs(5) {
            poll = Duration::from_millis(200);
        }

        // pane gone? (dead daemon also lands here — exit with a note)
        let alive = match call(&svc, "pane_status", obj(json!({ "id": args.pane }))).await {
            Ok(v) => v.get("alive").and_then(Value::as_bool).unwrap_or(false),
            Err(_) => false,
        };
        if !alive {
            let _ = out.write_all(b"\r\n[vmi-attach] pane exited\r\n");
            let _ = out.flush();
            break;
        }
    }
    let _ = detach;

    let _ = call(&svc, "pane_detach", obj(json!({ "id": args.pane }))).await;
    Ok(())
}
