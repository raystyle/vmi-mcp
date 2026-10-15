# vmi-mcp

[English](README.md) | 简体中文

VM 通用用户级 MCP 工作台：服务本体出生在**交互会话**（唯一能让子进程拿到真窗口与用户 DPAPI 密钥的启动路径），任何会话经 streamable-HTTP 直连调用。一个 daemon、一个命令面，session 0/1 不再是调用方的问题。

## 工具面（v0.2）

| 工具 | 作用 |
| --- | --- |
| `pane_open` / `pane_read` / `pane_send` / `pane_resize` / `pane_kill` | 长驻 shell 面板（Windows ConPTY、Linux/macOS pty；多客户端按游标读 + 按键写入） |
| `shell` | 一次性执行并捕获输出；Windows `session=1` 交互桌面上下文、`session=0` 无头 SYSTEM（无密码 schtasks 通道） |
| `spawn` | 可见 GUI 启动（argv 数组，不经 shell 引号） |
| `status` | 心跳：pid、会话、用户、运行时长、在开面板数 |

## 运行

监听 `0.0.0.0:9881`，MCP 端点 `/mcp`（streamable HTTP）。Windows release 构建为静态 CRT、无窗口后台 daemon。任意 MCP 客户端可接；mcp2cli 命令面：

```
mcp2cli --url http://<guest-ip>:9881/mcp ls
mcp2cli --url http://<guest-ip>:9881/mcp shell --cmd whoami --session 0
```

Windows 出生通道：shell:startup 或 ONLOGON 计划任务直指 exe（Explorer 链拉起，交互会话、子窗可见）。回环调用照常可用；远程调用以 guest IP 为 Host 头，服务启动时自动枚举本机全部网卡 IP 放行（rmcp DNS-rebinding 防护），`VMI_MCP_ALLOWED_HOSTS` 环境变量可追加。

## 平台支持

Windows（ConPTY）与 Linux（pty）同一构建；macOS 走同一 portable-pty 路径。GitHub Actions 在打 tag（`v*`）时编全部目标：windows x86_64（静态 CRT）、linux x86_64/aarch64、macOS aarch64。

## 已知边界（v0.2）

- session-0 的落盘件在系统临时目录
- kill 只达直接子进程
- MCP 会话态在内存，daemon 重启丢活动会话（客户端重连即可）
- 降权路径（`runas /trustlevel`）为发射后不管，不捕获输出
