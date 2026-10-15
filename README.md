# vmi-mcp

Universal user-level MCP workbench for VMs: born in the interactive
session (the only launch path whose children get real windows and the
user's DPAPI secrets), spoken to over streamable-HTTP from any session.
One daemon, one command face, session 0/1 stops being the caller's
problem.

## Tools (v0.2)

| tool | what |
| --- | --- |
| `pane_open` / `pane_read` / `pane_send` / `pane_resize` / `pane_kill` | long-lived shell panes (ConPTY on Windows, pty on Linux; cursor-based multiplexed read + send) |
| `shell` | one-shot captured execution; Windows `session=1` interactive desktop vs `session=0` headless SYSTEM (passwordless schtasks channel) |
| `spawn` | visible GUI launch (argv, no shell quoting) |
| `status` | heartbeat: pid, session, user, uptime, open panes |

## Run

Listens on `0.0.0.0:9881`, MCP endpoint `/mcp` (streamable HTTP).
Release Windows builds are static-CRT and windowless (background
daemon).

Any MCP client works. The CLI face is
[mcp2cli](https://github.com/mcp2cli/source-code) (Rust; upstream
targets unix, Windows builds need the cfg gates we carry in
hvperv-harness):

```
mcp2cli --url http://<guest-ip>:9881/mcp ls
mcp2cli --url http://<guest-ip>:9881/mcp shell --cmd whoami --session 0
```

Windows birth canal: shell:startup / ONLOGON scheduled task pointing at
the exe (Explorer-launched => interactive session, visible children).
Loopback callers keep working; remote callers present the guest IP,
which must be listed in `allowed_hosts` (rmcp DNS-rebinding guard).

## Platform support

Windows (ConPTY) and Linux (pty) in one build; macOS planned on the
same portable-pty path. GitHub Actions builds all targets on tags
(`v*`): windows x86_64 (static CRT), linux x86_64/aarch64, macOS
aarch64.

## Known limits (v0.2)

- session-0 scratch files live in the system temp dir
- kill reaches the direct child only
- MCP session state is in-memory; daemon restart drops live sessions
- de-elevation path (`runas /trustlevel`) is fire-and-forget
