# vmi-mcp

English | [简体中文](README.zh-CN.md)

Universal user-level MCP workbench for VMs: born in the interactive
session (the only launch path whose children get real windows and the
user's DPAPI secrets), spoken to over streamable-HTTP from any session.
One daemon, one command face, session 0/1 stops being the caller's
problem.

## Tools v0.6

| tool | what |
| --- | --- |
| `pane_open` / `pane_read` / `pane_send` / `pane_resize` / `pane_kill` | long-lived shell panes (ConPTY on Windows, pty on Linux/macOS; cursor-based multiplexed read + send; cursors are monotonic, `data_hex` carries exact raw bytes) |
| `pane_list` | enumerate open panes with alive/cursor/idle/attached |
| `pane_status` | buffer/idle heartbeat: `alive`, `buffer_len`, `idle_ms`, `attached`; stable idle means the command finished |
| `pane_attach` / `pane_detach` | raw-terminal attach mode: the server stops answering DSR cursor queries so the attach client's real terminal replies (full-screen apps render correctly); detach restores headless replies |
| `shell` | fire-and-forget into pane 0 by default (tmux semantics: fire, poll `pane_read`); `wait=true` blocks with captured output; Windows `session=0` headless SYSTEM via the passwordless schtasks channel |
| `spawn` | visible GUI launch (argv, no shell quoting) |
| `status` | heartbeat: pid, session, user, uptime, open panes |

## vmi-attach

The companion client mirrors a remote pane onto the local terminal:
raw bytes out, keystrokes in, terminal-size sync, DSR answered by your
real terminal. Detach with `Ctrl-B q` (herdr convention).

```
vmi-attach --url http://<guest-ip>:9881/mcp --pane 0
```

## vmi-tui

The pane-switching TUI: one focused pane fills the screen, a status
bar lists every pane (focus mark, alive, idle). The VT stream is
parsed into a per-pane grid (vte); DSR queries are answered from the
grid state, so full-screen remote apps render correctly. Keys route
to the focused pane through the Ctrl-B prefix (tmux-style): n/p
next/prev, c open, x kill, 0-9 jump, q quit. `--self-test` runs a
headless verification pass.

```
vmi-tui --url http://<guest-ip>:9881/mcp [--pane N]
```

## Run

Listens on `0.0.0.0:9881`, MCP endpoint `/mcp` (streamable HTTP).
Release Windows builds are static-CRT and windowless (background
daemon).

Any MCP client works. The CLI face is mcp2cli (Rust). Upstream
targets unix only; the Windows port patch is vendored in this repo at
`mcp2cli/` (upstream v0.1.10 + `0001-windows-port.patch`, with its own
README).

```
mcp2cli --url http://<guest-ip>:9881/mcp ls
mcp2cli --url http://<guest-ip>:9881/mcp shell --cmd whoami --session 0
```

Windows birth canal: shell:startup / ONLOGON scheduled task pointing at
the exe (Explorer-launched => interactive session, visible children).
Loopback callers keep working; remote callers present the guest IP,
which must be listed in `allowed_hosts` (rmcp DNS-rebinding guard).

## Platform support

Windows (ConPTY), Linux (pty), macOS (pty): one portable-pty build
path, verified on all three. GitHub Actions builds all targets on tags
(`v*`): windows x86_64 (static CRT), linux x86_64/aarch64, macOS
aarch64. Both binaries ship: `vmi-mcp` (daemon) and `vmi-attach`
(attach client).

## Known limits v0.6

- session-0 scratch files live in the system temp dir
- kill reaches the direct child only
- MCP session state is in-memory; daemon restart drops live sessions
- de-elevation path (`runas /trustlevel`) is fire-and-forget
- vmi-attach polls over HTTP (50ms active / 200ms idle); it is a
  mirror, not a zero-latency pty; fine for driving, visible on
  full-motion video
