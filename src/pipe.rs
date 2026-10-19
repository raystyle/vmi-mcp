//! Local pipe transport: MCP over a named pipe (Windows) or Unix-domain
//! socket (same build, other platforms) using the exact framing of MCP
//! stdio — newline-delimited JSON-RPC. Local programs talk here without
//! HTTP; remote clients keep the HTTP face. One MCP session per
//! connection; a fresh handler instance per connection shares the
//! process-wide workbench state through the Arc fields (same factory
//! shape as the HTTP service).

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::Workbench;

/// Per-connection handler factory (mirrors the StreamableHttpService
/// closure: a new Workbench per session, Arc state shared).
pub type HandlerFactory = Arc<dyn Fn() -> Workbench + Send + Sync>;

#[cfg(windows)]
pub async fn listen(name: &str, factory: HandlerFactory, ct: CancellationToken) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    // Single-instance lock: only the first daemon may own the pipe name.
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(name)
        .map_err(|e| anyhow::anyhow!("another instance holds the pipe {name}: {e}"))?;
    loop {
        server
            .connect()
            .await
            .with_context(|| format!("pipe {name} connect failed"))?;
        if ct.is_cancelled() {
            return Ok(());
        }
        // Create the next instance before serving this one, so a client
        // never sees ENOENT between connections. The CONNECTED instance
        // goes to the session; the fresh one becomes the next acceptor.
        let next = ServerOptions::new().create(name)?;
        let handler = factory();
        let session_ct = ct.child_token();
        tokio::spawn(async move {
            use rmcp::ServiceExt;
            match handler.serve_with_ct(server, session_ct).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(e) => {
                    eprintln!("pipe session init failed: {e}");
                }
            }
        });
        server = next;
    }
}

#[cfg(not(windows))]
pub async fn listen(name: &str, factory: HandlerFactory, ct: CancellationToken) -> Result<()> {
    use tokio::net::UnixListener;

    let _ = std::fs::remove_file(name); // stale socket from a crashed daemon
    let listener = UnixListener::bind(name).with_context(|| format!("bind {name} failed"))?;
    loop {
        let (stream, _) = listener.accept().await?;
        if ct.is_cancelled() {
            return Ok(());
        }
        let handler = factory();
        let session_ct = ct.child_token();
        tokio::spawn(async move {
            use rmcp::ServiceExt;
            if let Ok(running) = handler.serve_with_ct(stream, session_ct).await {
                let _ = running.waiting().await;
            }
        });
    }
}
