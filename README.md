# Portus

**Port collision prevention daemon for developers and AI coding agents.**

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

---

## The Problem

Every dev machine has the same fight: three services all want port 3000, and whoever starts last loses. It gets worse when AI coding agents enter the picture. Claude Code, Cursor, and similar tools spin up parallel workspaces that each try to bind the same ports, causing cascading failures that are hard to diagnose and annoying to fix manually. This is a [known issue](https://github.com/anthropics/claude-code/issues/34385) with no clean solution in the tooling itself.

## The Solution

Portus is a background daemon that brokers port allocations atomically. Every service asks Portus for a port before binding. Portus checks availability, records the lease, and hands back a guaranteed-unique port. No more races. No more "address already in use."

```bash
# Before: chaos
node server.js  # Error: listen EADDRINUSE :::3000

# After: clean
portus request --service web --port 3000
# ✓ Allocated port 3000 for service 'web'
#   Lease ID: lease_01jx...
#   Token:    tok_01jx...
```

---

## Quick Start

```bash
# Install from source
cargo install --path crates/portus-cli

# Request a port (daemon auto-starts on first use)
portus request --service web --port 3000

# Run a command with an allocated port injected as $PORT
portus run --service api -- node server.js

# See what's allocated
portus list

# Interactive dashboard
portus dashboard
```

---

## MCP Setup (for AI Agents)

This is the primary reason Portus exists. Add it to your MCP config and every AI agent in your workspace gets native port allocation without any manual coordination.

**Claude Code** (`~/.claude.json` or `.mcp.json` in your project):

```json
{
  "mcpServers": {
    "portus": {
      "command": "portus",
      "args": ["mcp", "serve"]
    }
  }
}
```

**Cursor / other MCP clients**: same config, different config file location per client.

Once connected, the agent has access to five tools:

| Tool | Description |
|------|-------------|
| `allocate_port` | Allocate a managed TCP port for a service. Returns `port`, `lease_id`, `token`, `expires_at`. |
| `release_port` | Release a lease by `lease_id` and `token`. |
| `list_ports` | List all active leases, optionally filtered by project path. |
| `check_port` | Check whether a specific port is available and who holds it. |
| `daemon_status` | Get daemon PID, uptime, and active lease count. |

The agent calls `allocate_port` before starting a server, gets back a port number, and calls `release_port` when done. No shell scripts, no `.env` hacks, no conflicts between parallel agents.

---

## Features

- **Lease-based allocation** with configurable expiry and auto-cleanup of stale leases
- **MCP server** (`portus mcp serve`) for native AI agent integration via stdio
- **TUI dashboard** (`portus dashboard`) for real-time monitoring of allocations and listeners
- **Port scanning** (`portus scan`) showing which listeners are Portus-managed vs. unmanaged
- **Process killing** (`portus kill --port 3000`) with `--dry-run` preview
- **`portus run` wrapper** that allocates, injects `$PORT`, confirms on bind, and releases on exit
- **Signal-safe cleanup**: SIGTERM and SIGINT both trigger lease release before exit
- **JSON output** on most non-interactive commands via `--json`
- **`--dry-run`** on `request` and `kill` for safe previews
- **Auto-start**: daemon starts automatically on first CLI use, no service manager needed
- **Cross-platform**: macOS, Linux, and Windows (via `interprocess`)

---

## CLI Reference

| Command | Description |
|---------|-------------|
| `portus request` | Allocate a port for a service (`--port`, `--auto-reassign`, `--dry-run`) |
| `portus confirm` | Confirm the client bound the allocated port |
| `portus release` | Release a port allocation by lease ID and token |
| `portus list` | List active allocations |
| `portus status` | Show daemon PID, uptime, and lease count |
| `portus run` | Run a command with an allocated port injected as `$PORT` |
| `portus scan` | Scan listening ports, showing managed vs. unmanaged |
| `portus kill` | Kill the process on a port (`--signal term\|kill`, `--dry-run`) |
| `portus dashboard` | Interactive TUI dashboard |
| `portus daemon start\|stop\|status` | Manage the daemon directly |
| `portus mcp serve` | Start the MCP server for AI agent integration |

Most non-interactive commands accept `--json` for machine-readable output.

---

## Architecture

```
┌─────────┐  local socket ┌──────────┐     ┌──────────────┐
│ portus  │◄──────────────►│ portusd  │────►│ registry.toml│
│  (CLI)  │  len-prefix    │ (daemon) │     └──────────────┘
└─────────┘      JSON      └──────────┘
                                │
                           ┌────┴────┐
                           │ Sweeper │
                           │ (expiry)│
                           └─────────┘
```

- **IPC**: local socket with length-prefixed JSON framing (Unix sockets on Unix, named-pipe-compatible on Windows)
- **Leases**: time-bounded with bind confirmation and heartbeat renewal for wrapped services
- **Crash recovery**: registry persisted atomically; stale leases expire after a 60s grace period
- **Security**: scoped to OS user via filesystem permissions on the socket
- **Idle shutdown**: daemon exits after 10 minutes with no active leases

---

## Comparison

| Feature | Portus | Manual (.env) | portbroker | lsof + kill |
|---------|:------:|:-------------:|:----------:|:-----------:|
| MCP server | ✓ | ✗ | ✗ | ✗ |
| Lease auto-expiry | ✓ | ✗ | ✓ | ✗ |
| TUI dashboard | ✓ | ✗ | ✗ | ✗ |
| JSON output | ✓ | ✗ | ✗ | ✗ |
| Multi-agent aware | ✓ | ✗ | ✓ | ✗ |
| Zero-config | ✓ | ✗ | ✗ | ✓ |
| Signal-safe run | ✓ | ✗ | ✗ | ✗ |

---

## Configuration

Portus stores everything under `~/.config/portus/` by default.

| Path | Purpose |
|------|---------|
| `~/.config/portus/registry.toml` | Persisted lease registry |
| `~/.config/portus/portus.sock` | IPC socket |

Auto-assigned ports (when no `--port` is specified) come from the range **10000-19999**.

---

## License

MIT
