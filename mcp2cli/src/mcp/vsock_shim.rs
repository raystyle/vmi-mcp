// Windows stub: AF_VSOCK / AF_UNIX are unix-only. On Windows the shim
// never activates (target_from_env -> None) and callers fall through to
// the streamable-HTTP transport.
#[cfg(not(unix))]
pub struct DialTarget;
#[cfg(not(unix))]
pub type DialStream = std::net::TcpStream;
#[cfg(not(unix))]
pub fn target_from_env(_server: &str, _vsock_port: u32) -> Option<DialTarget> {
    None
}
#[cfg(not(unix))]
pub fn dial(_target: &DialTarget) -> anyhow::Result<DialStream> {
    anyhow::bail!("vsock shim is unix-only")
}
#[cfg(not(unix))]
pub fn single_shot(
    _request: &serde_json::Value,
    _stream: DialStream,
) -> anyhow::Result<serde_json::Value> {
    anyhow::bail!("vsock shim is unix-only")
}
#[cfg(not(unix))]
pub fn pipe_ndjson<R: std::io::Read + Send + 'static, W: std::io::Write + Send + 'static>(
    _reader: R,
    _writer: W,
    _stream: DialStream,
) -> anyhow::Result<()> {
    anyhow::bail!("vsock shim is unix-only")
}
