# vmi-mcp

[English](README.md) | 简体中文

VM 通用用户级 MCP 工作台：服务本体出生在**交互会话**（唯一能让子进程拿到真窗口与用户 DPAPI 密钥的启动路径），任何会话经 streamable-HTTP 直连调用。一个 daemon、一个命令面，session 0/1 不再是调用方的问题。

## 工具面 v0.6

| 工具 | 作用 |
| --- | --- |
| `pane_open` / `pane_read` / `pane_send` / `pane_resize` / `pane_kill` | 长驻 shell 面板（Windows ConPTY、Linux/macOS pty；按游标多客户端读 + 按键写入；游标单调不回退，`data_hex` 给精确原始字节） |
| `pane_list` | 枚举在开面板（alive/cursor/idle/attached） |
| `pane_status` | 缓冲/空闲心跳：`alive`、`buffer_len`、`idle_ms`、`attached`；空闲稳定即命令跑完 |
| `pane_attach` / `pane_detach` | 裸终端 attach 模式：服务端停答 DSR 光标查询，改由 attach 客户端的真终端回复（全屏程序渲染正确）；detach 恢复无头回复 |
| `shell` | 默认即发即忘进面板 0（tmux 语义：发射后轮询 `pane_read` 取果）；`wait=true` 阻塞捕获输出；Windows `session=0` 走无密码 schtasks SYSTEM 通道 |
| `spawn` | 可见 GUI 启动（argv 数组，不经 shell 引号） |
| `status` | 心跳：pid、会话、用户、运行时长、在开面板数 |

## vmi-attach

随附客户端：把远端面板镜像到本地终端。输出走原始字节、按键实时转发、终端尺寸同步、DSR 由你的真终端回复。脱离键 `Ctrl-B q`（herdr 惯例）。

```
vmi-attach --url http://<guest-ip>:9881/mcp --pane 0
```

## vmi-tui

面板切换 TUI：焦点面板占满主区，状态栏列出全部面板（焦点标记、alive、空闲）。VT 流解析进每面板网格（vte）；DSR 查询由网格状态合成回复，全屏远端程序渲染正确。按键经 Ctrl-B 前缀路由（tmux 风）：n/p 切换、c 开、x 杀、0-9 直达、q 退出。`--self-test` 跑无头自验收。

```
vmi-tui --url http://<guest-ip>:9881/mcp [--pane N]
```

## 运行

监听 `0.0.0.0:9881`，MCP 端点 `/mcp`（streamable HTTP）。Windows release 构建为静态 CRT、无窗口后台 daemon。

任意 MCP 客户端可接；CLI 面是 [mcp2cli](https://github.com/mcp2cli/source-code)（Rust；上游只编 unix，Windows 构建需 hvperv-harness 侧携带的 cfg 门补丁）：

```
mcp2cli --url http://<guest-ip>:9881/mcp ls
mcp2cli --url http://<guest-ip>:9881/mcp shell --cmd whoami --session 0
```

Windows 出生通道：shell:startup 或 ONLOGON 计划任务直指 exe（Explorer 链拉起，交互会话、子窗可见）。回环调用照常可用；远程调用以 guest IP 为 Host 头，服务启动时自动枚举本机全部网卡 IP 放行（rmcp DNS-rebinding 防护），`VMI_MCP_ALLOWED_HOSTS` 环境变量可追加。

## 平台支持

Windows（ConPTY）、Linux（pty）、macOS（pty）同一条 portable-pty 构建路径，三平台均已实测。GitHub Actions 在打 tag（`v*`）时编全部目标：windows x86_64（静态 CRT）、linux x86_64/aarch64、macOS aarch64。两个二进制都出货：`vmi-mcp`（daemon）与 `vmi-attach`（attach 客户端）。

## 已知边界 v0.6

- session-0 的落盘件在系统临时目录
- kill 只达直接子进程
- MCP 会话态在内存，daemon 重启丢活动会话（客户端重连即可）
- 降权路径（`runas /trustlevel`）为发射后不管，不捕获输出
- vmi-attach 走 HTTP 轮询（活跃 50ms / 空闲 200ms）；它是镜像不是零延迟 pty，驱动操作无感，全动态画面可见延迟
