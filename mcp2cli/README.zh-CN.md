# mcp2cli（vendor 版，已应用 Windows 移植补丁）

[English](README.md) | 简体中文

上游：https://github.com/mcp2cli/source-code @ v0.1.10（Apache-2.0）。本树是**裁剪后的上游快照**（上游的文档站、nx node 工具链与发布工具已裁；保留 Rust 源码、tests 与配置）加 `0001-windows-port.patch` 原地应用。

## 为什么 vendor 在这里

vmi-mcp 的 CLI 面就是 mcp2cli。上游只编 unix；Windows 移植以本补丁形式存在（待时机提回上游）：

- `src/mcp/vsock_shim_unix.rs`（新增）：unix vsock 传输拆出 cfg 门
- vsock shim、runtime sinks 与 daemon 路径的 cfg(windows)/cfg(unix) 门
- windows-msvc 上 `cargo build --release` 干净通过所需的少量 host/runtime 修复

## 重建（Windows）

```
cargo build --release
```

## 补丁再生成

在上游 v0.1.10 的干净检出上：

```
git -c core.autocrlf=false diff HEAD --ignore-cr-at-eol > 0001-windows-port.patch
```

本树 = 上游 + 补丁；对上游 HEAD 的 `git diff` 即补丁内容（裁剪掉的目录不涉补丁，补丁只改 `src/`）。
