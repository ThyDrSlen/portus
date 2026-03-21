# Portus

**Port collision prevention daemon for developers.**

Portus is a background daemon that brokers all port allocations atomically on your dev machine. No more "address already in use" — every service gets a guaranteed-unique port.

## How It Works

1. **Daemon** (`portusd`) runs in the background, listening on a local IPC socket
2. **CLI** (`portus`) talks to the daemon to allocate, confirm, and release ports
3. **Registry** (`~/.config/portus/registry.toml`) persists all allocations with lease-based expiry

## Quick Start

```bash
# Build
cargo build --release

# Start the daemon (auto-starts on first CLI use too)
portus daemon start

# Request a port for your service
portus request --service web --port 3000

# Or let portus pick one automatically / reassign if needed
portus request --service api --auto-reassign

# Run a command with an allocated port injected as $PORT
portus run --service web -- node server.js

# List all allocations
portus list

# Check daemon status
portus status
```

## Commands

| Command | Description |
|---------|-------------|
| `portus request` | Request a port for a service (`alloc` remains an alias) |
| `portus confirm` | Confirm the client bound the port |
| `portus release` | Release a port allocation |
| `portus list` | List active port allocations |
| `portus status` | Show daemon status |
| `portus scan` | Inspect listeners and whether Portus manages them |
| `portus kill` | Terminate the process listening on a port |
| `portus dashboard` | Open the interactive monitoring dashboard |
| `portus run` | Run a command with an allocated port |
| `portus daemon start\|stop\|status` | Manage the daemon |

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

- **IPC**: local socket abstraction with length-prefixed JSON framing (Unix sockets on Unix, named-pipe-compatible pathing on Windows)
- **Leases**: time-bounded with bind confirmation and heartbeat renewal for wrapped services
- **Crash recovery**: registry persisted atomically; stale and dead-client leases expire automatically after a startup grace period
- **Security**: Scoped to OS user via filesystem permissions on the socket

## Design Decisions

- **Lease-based, not bind-based**: The daemon doesn't hold ports open. It tracks allocations and checks bindability on request. Small race window accepted in exchange for simplicity.
- **Auto-start**: The daemon starts automatically on first CLI invocation. No service manager integration required.
- **Session tokens**: Each lease gets a unique token. Only the holder can confirm/release/heartbeat.
- **Idle shutdown**: Daemon exits after 10 minutes with no active leases.

## License

MIT
