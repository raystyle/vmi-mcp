//! Local pipe transport: MCP over a named pipe (Windows) or Unix-domain
//! socket (same build, other platforms) using the exact framing of MCP
//! stdio — newline-delimited JSON-RPC. Local programs talk here without
//! HTTP; remote clients keep the HTTP face. One MCP session per
//! connection; a fresh handler per connection shares the process-wide
//! state through the Arc fields (same factory shape as the HTTP
//! service).

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
pub async fn listen<H, F>(
    name: &str,
    sddl: Option<&str>,
    factory: F,
    ct: CancellationToken,
) -> Result<()>
where
    H: rmcp::handler::server::ServerHandler,
    F: Fn() -> H + Send + Sync + 'static,
{
    // Single-instance lock: only the first daemon may own the pipe name.
    let mut server = create_instance(name, sddl, true)
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
        let next = create_instance(name, sddl, false)?;
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

#[cfg(windows)]
fn create_instance(
    name: &str,
    sddl: Option<&str>,
    first: bool,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;

    // SAFETY: attrs comes from build_sa (a valid SECURITY_ATTRIBUTES the
    // daemon leaks on purpose — it must outlive the pipe instance) or is
    // null, which is the documented no-attribute path.
    unsafe {
        match sddl {
            Some(sddl) => {
                let mut opts = ServerOptions::new();
                opts.first_pipe_instance(first);
                opts.create_with_security_attributes_raw(name, build_sa(sddl)?)
            }
            None => {
                let mut opts = ServerOptions::new();
                opts.first_pipe_instance(first);
                opts.create(name)
            }
        }
    }
}

/// Build a raw SECURITY_ATTRIBUTES from an SDDL string so an elevated
/// daemon's pipe stays connectable by same-machine non-elevated clients.
/// The returned pointer must stay valid for the lifetime of the pipe
/// instance (leaked on purpose: one allocation per daemon run).
#[cfg(windows)]
fn build_sa(sddl: &str) -> std::io::Result<*mut std::ffi::c_void> {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sa: Box<SECURITY_ATTRIBUTES> = Box::new(SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 0,
    });
    // SAFETY: wide is NUL-terminated; sa is a valid out-param.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            1, // SDDL_REVISION_1
            &mut sa.lpSecurityDescriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Box::into_raw(sa) as *mut std::ffi::c_void)
}

#[cfg(not(windows))]
pub async fn listen<H, F>(
    name: &str,
    _sddl: Option<&str>,
    factory: F,
    ct: CancellationToken,
) -> Result<()>
where
    H: rmcp::handler::server::ServerHandler,
    F: Fn() -> H + Send + Sync + 'static,
{
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
