# mcp2cli (vendored, Windows-port patch applied)

Upstream: https://github.com/mcp2cli/source-code @ v0.1.10
(Apache-2.0). This tree is the upstream snapshot plus our
`0001-windows-port.patch` already applied in place.

## Why vendored here

vmi-mcp's CLI face is mcp2cli. The upstream builds unix-only; the
Windows port lives as this patch until it can go upstream:

- `src/mcp/vsock_shim_unix.rs` (new): the unix vsock transport split
  out behind cfg gates
- cfg(windows)/cfg(unix) gates across the vsock shim, runtime sinks
  and daemon paths
- minor host/runtime fixes needed for a clean `cargo build --release`
  on windows-msvc

## Rebuild (Windows)

```
cargo build --release
```

## Regenerate the patch

From a clean upstream checkout at v0.1.10:

```
git -c core.autocrlf=false diff HEAD --ignore-cr-at-eol > 0001-windows-port.patch
```

This tree = upstream + patch; `git diff` against upstream HEAD shows
exactly the patch content.
