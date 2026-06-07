# HARC MCP Server

An MCP server that helps AI assistants write compiler-backed HARC code without confusing shipped syntax with roadmap docs.

## Setup

```bash
# 1. Build HARC
cargo build --release --bin harc

# 2. Install Python dependencies
cd mcp
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
```

## Usage With Codex

Register the server once:

```bash
codex mcp add harc \
  --env HARC_BIN=/path/to/harc-com/target/release/harc \
  --env ARCH_BIN=/path/to/arch-com/target/release/arch \
  --env HARC_MCP_WORKSPACE_ROOTS=/path/to/your/harc/project \
  -- /path/to/harc-com/mcp/.venv/bin/python3 \
     /path/to/harc-com/mcp/harc_mcp_server.py
```

Restart the Codex session after registration.

`HARC_MCP_WORKSPACE_ROOTS` is a colon-separated list of directories where the
MCP server may read `.harc` files and write generated output. Relative file
paths passed to tools resolve under the first matching workspace root, falling
back to the first listed root for new paths. The `harc-com` checkout is always
allowed so bundled docs and fixtures remain available.

## Available Resources

| Resource | Description |
|---|---|
| `harc://readme` | Current shipped status, CLI overview, and examples |
| `harc://specification` | Full HARC v1 specification; verify shipped support before relying on roadmap features |
| `harc://sim-cli` | Complete `harc sim` option reference |
| `harc://test-ergonomics` | Shipped and partial test/testbench ergonomics status |
| `harc://ral-support` | Register abstraction layer status and syntax |

## Available Tools

| Tool | Description |
|---|---|
| `get_harc_syntax` | Search shipped docs, parser source, and fixtures for syntax snippets |
| `harc_feature_status` | Report shipped/partial/proposed evidence for a query |
| `harc_examples` | Retrieve matching runnable fixtures and nearby DUT names |
| `harc_check` | Run `harc check` on `.harc` files |
| `harc_sim_emit_only` | Run `harc sim --emit-only` against SV or ARCH DUT backends |
| `harc_advise` | Query local HARC error-to-fix learning store |
| `list_harc_files` | List `.harc` files under an allowed workspace root |

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `HARC_BIN` | `target/release/harc` | Path to the HARC compiler binary |
| `HARC_MCP_WORKSPACE_ROOTS` | repo root | Additional colon-separated roots allowed for file operations |
| `ARCH_BIN` | unset | Optional path forwarded to `harc sim --dut` |

When `harc_sim_emit_only` is called without `outdir`, the server writes emitted
artifacts to a temporary directory instead of the `harc-com` checkout.
