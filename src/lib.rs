//! Workspace root crate for Portus.
//!
//! This crate exists solely to host workspace-level integration tests
//! in the `tests/` directory. All real functionality lives in:
//!
//! - [`portus-core`](../portus_core/index.html) — Core library (model, protocol, registry, IPC)
//! - [`portus-daemon`] — Background daemon (`portusd`)
//! - [`portus-cli`] — CLI and MCP server (`portus`)
