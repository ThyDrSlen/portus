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
            .with_instructions("Allocate and manage local development ports with Portus")
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AllocatePortParams {
    service: String,
    port: Option<u16>,
    project: Option<String>,
    auto_reassign: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReleasePortParams {
    lease_id: String,
    token: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListPortsParams {
    project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CheckPortParams {
    port: u16,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct AllocatePortResult {
    port: u16,
    lease_id: String,
    token: String,
    expires_at: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ReleasePortResult {
    released: bool,
    lease_id: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct LeaseInfo {
    lease_id: String,
    project_path: String,
    service_name: String,
    port: u16,
    protocol: String,
    state: String,
    client_pid: Option<u32>,
    token: String,
    granted_at: String,
    confirmed_at: Option<String>,
    last_heartbeat_at: Option<String>,
    expires_at: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct CheckPortResult {
    available: bool,
    holder: Option<LeaseInfo>,
    reason: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct DaemonStatusResult {
    pid: u32,
    uptime: u64,
    active_leases: usize,
}

#[tool_router(router = tool_router)]
impl PortusServer {
    #[tool(description = "Allocate a managed TCP port for a service")]
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

    #[tool(description = "Release a managed TCP port lease")]
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

    #[tool(description = "List active managed port leases", annotations(read_only_hint = true))]
    async fn list_ports(
        &self,
        Parameters(params): Parameters<ListPortsParams>,
    ) -> Result<Json<Vec<LeaseInfo>>, String> {
        let project_filter = params
            .project
            .map(|project| crate::resolve_project(Some(project)).map_err(|err| err.to_string()))
            .transpose()?;
        let response = crate::send_request(Request::List { project_filter })
            .await
            .map_err(|err| err.to_string())?;

        match response {
            Response::LeaseList { leases } => Ok(Json(leases.iter().map(LeaseInfo::from).collect())),
            Response::Error { code, message } => Err(crate::format_daemon_error(&code, &message)),
            other => Err(format!("unexpected response: {:?}", other)),
        }
    }

    #[tool(description = "Check whether a TCP port is available", annotations(read_only_hint = true))]
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

    #[tool(description = "Show Portus daemon status", annotations(read_only_hint = true))]
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
