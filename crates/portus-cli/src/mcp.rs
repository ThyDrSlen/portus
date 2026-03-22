use anyhow::{Context, Result, bail};
use portus_core::{
    model::{Lease, Protocol},
    port_check,
    protocol::{Request, Response},
};
use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};

/// MCP server implementation for Portus port allocation.
#[derive(Debug, Clone)]
pub(crate) struct PortusServer {
    tool_router: ToolRouter<Self>,
}

impl PortusServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for PortusServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PortusServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Portus prevents port collisions between dev servers and AI coding agents. Use allocate_port before starting any server, release_port when done. The daemon auto-starts on first use.")
    }
}

/// Parameters for the allocate_port MCP tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AllocatePortParams {
    /// Service name to allocate a port for.
    service: String,
    /// Preferred port number (optional).
    port: Option<u16>,
    /// Project path (optional).
    project: Option<String>,
    /// Whether to auto-reassign if preferred port is unavailable.
    auto_reassign: Option<bool>,
}

/// Parameters for the release_port MCP tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReleasePortParams {
    /// Lease ID to release.
    lease_id: String,
    /// Session token for authentication.
    token: String,
}

/// Parameters for the list_ports MCP tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListPortsParams {
    /// Project path filter (optional).
    project: Option<String>,
}

/// Parameters for the check_port MCP tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CheckPortParams {
    /// Port number to check.
    port: u16,
}

/// Result of the allocate_port MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct AllocatePortResult {
    /// Allocated port number.
    port: u16,
    /// Lease ID for the allocation.
    lease_id: String,
    /// Session token for future operations.
    token: String,
    /// Expiration time in RFC3339 format.
    expires_at: String,
}

/// Result of the release_port MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ReleasePortResult {
    /// Whether the release was successful.
    released: bool,
    /// Lease ID that was released.
    lease_id: String,
}

/// Information about a port lease.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct LeaseInfo {
    /// Unique lease identifier.
    lease_id: String,
    /// Project path associated with the lease.
    project_path: String,
    /// Service name for the lease.
    service_name: String,
    /// Allocated port number.
    port: u16,
    /// Protocol (tcp or udp).
    protocol: String,
    /// Current lease state.
    state: String,
    /// Client process ID (if available).
    client_pid: Option<u32>,
    /// Session token for operations.
    token: String,
    /// When the lease was granted.
    granted_at: String,
    /// When the lease was confirmed (if confirmed).
    confirmed_at: Option<String>,
    /// Last heartbeat timestamp (if any).
    last_heartbeat_at: Option<String>,
    /// When the lease expires.
    expires_at: String,
}

/// Result of the list_ports MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ListPortsResult {
    /// List of active leases.
    leases: Vec<LeaseInfo>,
}

/// Result of the check_port MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct CheckPortResult {
    /// Whether the port is available.
    available: bool,
    /// Lease holding the port (if any).
    holder: Option<LeaseInfo>,
    /// Reason if port is unavailable.
    reason: Option<String>,
}

/// Result of the daemon_status MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct DaemonStatusResult {
    /// Daemon process ID.
    pid: u32,
    /// Daemon uptime in seconds.
    uptime: u64,
    /// Number of active leases.
    active_leases: usize,
}

#[tool_router(router = tool_router)]
impl PortusServer {
    #[tool(description = "Reserve a TCP port for a service before starting a dev server. Returns a lease with port number, lease_id, and token. If the preferred port is taken, set auto_reassign=true to get the next available port. The daemon auto-starts on first call.")]
    async fn allocate_port(
        &self,
        Parameters(params): Parameters<AllocatePortParams>,
    ) -> Result<Json<AllocatePortResult>, String> {
        let project = crate::resolve_project(params.project).map_err(|err| err.to_string())?;
        let response = crate::send_request(Request::Allocate {
            project,
            service: params.service,
            preferred_port: params.port,
            protocol: Protocol::Tcp,
            auto_reassign: params.auto_reassign.unwrap_or(false),
            pid: Some(std::process::id()),
        })
        .await
        .map_err(|err| err.to_string())?;

        match response {
            Response::Allocated { lease } => Ok(Json(AllocatePortResult {
                port: lease.port,
                lease_id: lease.lease_id,
                token: lease.session_token,
                expires_at: lease.expires_at.to_rfc3339(),
            })),
            Response::Error { code, message } => Err(crate::format_daemon_error(&code, &message)),
            other => Err(format!("unexpected response: {:?}", other)),
        }
    }

    #[tool(description = "Release a previously allocated port lease. Requires the lease_id and token from allocate_port. Call this when stopping a dev server to free the port for other services.")]
    async fn release_port(
        &self,
        Parameters(params): Parameters<ReleasePortParams>,
    ) -> Result<Json<ReleasePortResult>, String> {
        let response = crate::send_request(Request::Release {
            lease_id: params.lease_id.clone(),
            session_token: params.token,
        })
        .await
        .map_err(|err| err.to_string())?;

        match response {
            Response::Released { lease_id } => Ok(Json(ReleasePortResult {
                released: true,
                lease_id,
            })),
            Response::Error { code, message } => Err(crate::format_daemon_error(&code, &message)),
            other => Err(format!("unexpected response: {:?}", other)),
        }
    }

    #[tool(description = "List all active port allocations across all services and projects. Use the project filter to scope results to a specific project directory. Returns lease details including port, service name, state, and expiry.", annotations(read_only_hint = true))]
    async fn list_ports(
        &self,
        Parameters(params): Parameters<ListPortsParams>,
    ) -> Result<Json<ListPortsResult>, String> {
        let project_filter = params
            .project
            .map(|project| crate::resolve_project(Some(project)).map_err(|err| err.to_string()))
            .transpose()?;
        let response = crate::send_request(Request::List { project_filter })
            .await
            .map_err(|err| err.to_string())?;

        match response {
            Response::LeaseList { leases } => Ok(Json(ListPortsResult {
                leases: leases.iter().map(LeaseInfo::from).collect(),
            })),
            Response::Error { code, message } => Err(crate::format_daemon_error(&code, &message)),
            other => Err(format!("unexpected response: {:?}", other)),
        }
    }

    #[tool(description = "Check if a specific TCP port is available for use. Returns availability status and, if taken, details about which service holds the lease. Use this before allocate_port to preview availability without reserving.", annotations(read_only_hint = true))]
    async fn check_port(
        &self,
        Parameters(params): Parameters<CheckPortParams>,
    ) -> Result<Json<CheckPortResult>, String> {
        let response = crate::send_request(Request::List {
            project_filter: None,
        })
        .await
        .map_err(|err| err.to_string())?;

        let leases = match response {
            Response::LeaseList { leases } => leases,
            Response::Error { code, message } => return Err(crate::format_daemon_error(&code, &message)),
            other => return Err(format!("unexpected response: {:?}", other)),
        };

        let holder = leases
            .iter()
            .find(|lease| lease.port == params.port && lease.protocol == Protocol::Tcp);
        let bindable = port_check::is_port_available(params.port, Protocol::Tcp);

        let (available, reason) = match (bindable, holder) {
            (true, None) => (true, None),
            (false, Some(lease)) => (
                false,
                Some(format!(
                    "allocated to service '{}' (lease {})",
                    lease.service_name, lease.lease_id
                )),
            ),
            (false, None) => (false, Some("port is in use by another process".to_string())),
            (true, Some(lease)) => (
                false,
                Some(format!(
                    "allocated to service '{}' (lease {}) but not yet bound",
                    lease.service_name, lease.lease_id
                )),
            ),
        };

        Ok(Json(CheckPortResult {
            available,
            holder: holder.map(LeaseInfo::from),
            reason,
        }))
    }

    #[tool(description = "Check if the Portus daemon is running and healthy. Returns PID, uptime, and count of active leases. The daemon auto-starts on first tool call, so this is mainly for diagnostics.", annotations(read_only_hint = true))]
    async fn daemon_status(&self) -> Result<Json<DaemonStatusResult>, String> {
        let response = crate::send_request(Request::Status)
            .await
            .map_err(|err| err.to_string())?;

        match response {
            Response::DaemonStatus {
                pid,
                uptime_secs,
                active_leases,
                ..
            } => Ok(Json(DaemonStatusResult {
                pid,
                uptime: uptime_secs,
                active_leases,
            })),
            Response::Error { code, message } => Err(crate::format_daemon_error(&code, &message)),
            other => Err(format!("unexpected response: {:?}", other)),
        }
    }
}

impl From<&Lease> for LeaseInfo {
    fn from(lease: &Lease) -> Self {
        Self {
            lease_id: lease.lease_id.clone(),
            project_path: lease.project_path.clone(),
            service_name: lease.service_name.clone(),
            port: lease.port,
            protocol: format!("{:?}", lease.protocol).to_ascii_lowercase(),
            state: format!("{:?}", lease.state).to_ascii_lowercase(),
            client_pid: lease.client_pid,
            token: lease.session_token.clone(),
            granted_at: lease.granted_at.to_rfc3339(),
            confirmed_at: lease.confirmed_at.map(|value| value.to_rfc3339()),
            last_heartbeat_at: lease.last_heartbeat_at.map(|value| value.to_rfc3339()),
            expires_at: lease.expires_at.to_rfc3339(),
        }
    }
}

/// Start the MCP server on stdio.
pub(crate) async fn serve_stdio() -> Result<()> {
    let server = PortusServer::new()
        .serve(stdio())
        .await
        .context("failed to start MCP server")?;

    let quit_reason = server.waiting().await.context("MCP server task failed")?;
    if !matches!(quit_reason, rmcp::service::QuitReason::Closed) {
        bail!("MCP server exited unexpectedly: {:?}", quit_reason);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_router_has_five_tools() {
        let router = PortusServer::tool_router();
        let tools = router.list_all();
        assert_eq!(tools.len(), 5, "got: {:?}", tools.iter().map(|t| &t.name).collect::<Vec<_>>());
    }

    #[test]
    fn tool_names_match_expected() {
        let router = PortusServer::tool_router();
        let tools = router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| &*t.name).collect();
        for expected in ["allocate_port", "release_port", "list_ports", "check_port", "daemon_status"] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
    }

    #[test]
    fn all_tools_have_descriptions() {
        let router = PortusServer::tool_router();
        for tool in router.list_all() {
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "tool '{}' should have a non-empty description",
                tool.name,
            );
        }
    }

    #[test]
    fn all_tools_have_input_schemas() {
        let router = PortusServer::tool_router();
        for tool in router.list_all() {
            let schema = &tool.input_schema;
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "tool '{}' input_schema type should be 'object'",
                tool.name,
            );
        }
    }

    #[test]
    fn allocate_port_schema_requires_service() {
        let router = PortusServer::tool_router();
        let tools = router.list_all();
        let alloc = tools.iter().find(|t| t.name == "allocate_port").unwrap();
        let props = alloc.input_schema.get("properties")
            .and_then(|v| v.as_object())
            .expect("allocate_port should have properties");
        assert!(props.contains_key("service"), "allocate_port should have 'service' property");
        let required = alloc.input_schema.get("required")
            .and_then(|v| v.as_array())
            .expect("allocate_port should have required array");
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"service"), "allocate_port should require 'service'");
    }

    #[test]
    fn server_info_enables_tools() {
        let server = PortusServer::new();
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some(), "server should advertise tools capability");
    }

    #[test]
    fn read_only_tools_have_annotations() {
        let router = PortusServer::tool_router();
        let tools = router.list_all();
        let read_only_tools = ["list_ports", "check_port", "daemon_status"];
        for tool in &tools {
            if read_only_tools.contains(&&*tool.name) {
                let annotations = tool.annotations.as_ref().unwrap_or_else(|| {
                    panic!("tool '{}' should have annotations", tool.name)
                });
                assert_eq!(
                    annotations.read_only_hint,
                    Some(true),
                    "tool '{}' should have read_only_hint=true",
                    tool.name,
                );
            }
        }
    }
}
