use std::sync::Arc;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;

use anyhow::{Context, Result};
use interprocess::local_socket::traits::tokio::Listener as _;
use interprocess::local_socket::traits::tokio::Stream as _;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use portus_core::protocol::{Request, Response};
use portus_core::registry::Registry;
use portus_core::transport;
use portus_core::{ipc, paths, Lease};

/// Heartbeat sweep interval (seconds).
const SWEEP_INTERVAL_SECS: u64 = 30;
/// Idle shutdown timeout: daemon exits if no active leases for this long.
const IDLE_TIMEOUT_SECS: u64 = 600; // 10 minutes
const STARTUP_GRACE_SECS: u64 = 30;

struct DaemonState {
    registry: Registry,
    start_time: Instant,
    shutdown_requested: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Ensure config directory exists
    let config_dir = paths::ensure_config_dir()?;
    info!(dir = %config_dir.display(), "config directory ready");

    // Write PID file
    let pid_path = paths::pid_path()?;
    std::fs::write(&pid_path, std::process::id().to_string())
        .with_context(|| format!("failed to write PID file: {}", pid_path.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(&pid_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set PID file permissions: {}", pid_path.display()))?;
    info!(pid = std::process::id(), "daemon starting");

    // Load or create registry
    let registry_path = paths::registry_path()?;
    let registry = Registry::load(&registry_path)?;

    let state = Arc::new(Mutex::new(DaemonState {
        registry,
        start_time: Instant::now(),
        shutdown_requested: false,
    }));

    // Set up Unix domain socket
    let socket_path = paths::socket_path()?;
    // Remove stale socket file if present
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).ok();
    }
    let listener = ipc::bind(&socket_path)
        .with_context(|| format!("failed to bind IPC listener at {}", socket_path.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set socket permissions: {}", socket_path.display()))?;
    info!(path = %socket_path.display(), "listening on socket");

    // Spawn heartbeat sweep task
    let sweep_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        let mut last_active = Instant::now();
        loop {
            interval.tick().await;
            let mut s = sweep_state.lock().await;
            if s.shutdown_requested {
                break;
            }
            match s.registry.expire_stale() {
                Ok(n) if n > 0 => info!(expired = n, "sweep expired stale leases"),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "sweep error"),
            }
            if s.start_time.elapsed().as_secs() >= STARTUP_GRACE_SECS {
                match s.registry.expire_dead_clients(pid_is_alive) {
                    Ok(n) if n > 0 => info!(expired = n, "sweep expired dead client leases"),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "dead-client sweep error"),
                }
            }
            // Idle shutdown check
            if s.registry.active_count() > 0 {
                last_active = Instant::now();
            } else if last_active.elapsed().as_secs() > IDLE_TIMEOUT_SECS {
                info!("idle timeout reached, shutting down");
                s.shutdown_requested = true;
                break;
            }
        }
    });

    // Accept connections
    loop {
        {
            let s = state.lock().await;
            if s.shutdown_requested {
                break;
            }
        }

        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok(stream) => {
                        let conn_state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, conn_state).await {
                                warn!(error = %e, "connection handler error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "accept error");
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, shutting down");
                break;
            }
        }
    }

    // Cleanup
    info!("daemon shutting down");
    std::fs::remove_file(&socket_path).ok();
    std::fs::remove_file(&pid_path).ok();
    Ok(())
}

async fn handle_connection(
    stream: interprocess::local_socket::tokio::Stream,
    state: Arc<Mutex<DaemonState>>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.split();

    loop {
        let request: Option<Request> = transport::recv_json(&mut reader).await?;
        let request = match request {
            Some(r) => r,
            None => {
                // Client disconnected
                return Ok(());
            }
        };

        let response = process_request(request, &state).await;
        transport::send_json(&mut writer, &response).await?;

        // Check if shutdown was requested
        if matches!(response, Response::ShuttingDown) {
            return Ok(());
        }
    }
}

async fn process_request(request: Request, state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut s = state.lock().await;

    match request {
        Request::Allocate {
            project,
            service,
            preferred_port,
            protocol,
            auto_reassign,
            pid,
        } => match s
            .registry
            .allocate(project, service, preferred_port, protocol, auto_reassign, pid)
        {
            Ok(lease) => Response::Allocated { lease },
            Err(e) => Response::error("allocation_failed", e.to_string()),
        },

        Request::Confirm {
            lease_id,
            session_token,
        } => match s.registry.confirm(&lease_id, &session_token) {
            Ok(()) => Response::Confirmed { lease_id },
            Err(e) => Response::error("confirm_failed", e.to_string()),
        },

        Request::Release {
            lease_id,
            session_token,
        } => match s.registry.release(&lease_id, &session_token) {
            Ok(()) => Response::Released { lease_id },
            Err(e) => Response::error("release_failed", e.to_string()),
        },

        Request::Heartbeat {
            lease_id,
            session_token,
        } => match s.registry.heartbeat(&lease_id, &session_token) {
            Ok(expires_at) => Response::HeartbeatAck {
                lease_id,
                expires_at,
            },
            Err(e) => Response::error("heartbeat_failed", e.to_string()),
        },

        Request::List { project_filter } => {
            let leases: Vec<Lease> = s
                .registry
                .list(project_filter.as_deref())
                .into_iter()
                .cloned()
                .collect();
            Response::LeaseList { leases }
        }

        Request::Status => {
            let uptime = s.start_time.elapsed().as_secs();
            let active_leases = s.registry.active_count();
            let socket_path = paths::socket_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            Response::DaemonStatus {
                pid: std::process::id(),
                uptime_secs: uptime,
                active_leases,
                socket_path,
            }
        }

        Request::Shutdown => {
            info!("shutdown requested via IPC");
            s.shutdown_requested = true;
            Response::ShuttingDown
        }
    }
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    let output = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status();
    matches!(output, Ok(status) if status.success())
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            !stdout.trim().is_empty() && !stdout.starts_with("INFO:")
        }
        _ => false,
    }
}
