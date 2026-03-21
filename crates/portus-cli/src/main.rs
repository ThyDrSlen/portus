mod dashboard;

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use interprocess::local_socket::traits::tokio::Stream as _;
use clap::{Parser, Subcommand};
use portus_core::model::{Lease, LeaseState, Protocol};
use portus_core::protocol::{Request, Response};
use portus_core::registry::Registry;
use portus_core::port_check;
use portus_core::scan::{kill_processes_on_port, scan_ports};
use portus_core::{ipc, paths, transport};
use serde::Serialize;
use tokio::signal::unix::SignalKind;

#[derive(Parser)]
#[command(name = "portus", about = "Port collision prevention for developers")]
#[command(version, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Request a port for a service
    #[command(alias = "alloc")]
    Request {
        /// Project path (defaults to current directory)
        #[arg(short, long)]
        project: Option<String>,
        /// Service name
        #[arg(short, long)]
        service: String,
        /// Preferred port (auto-assigned if not specified)
        #[arg(long)]
        port: Option<u16>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Reassign automatically if the preferred port is unavailable
        #[arg(long)]
        auto_reassign: bool,
        /// Check availability without allocating
        #[arg(long)]
        dry_run: bool,
    },
    /// Confirm a port allocation (client bound successfully)
    Confirm {
        /// Lease ID
        #[arg(long)]
        lease_id: String,
        /// Session token
        #[arg(long)]
        token: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Release a port allocation
    Release {
        /// Lease ID
        #[arg(long)]
        lease_id: String,
        /// Session token
        #[arg(long)]
        token: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List active port allocations
    List {
        /// Filter by project path
        #[arg(short, long)]
        project: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show daemon status
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Scan listening ports and show managed matches
    Scan {
        /// Filter to a single port
        #[arg(long)]
        port: Option<u16>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Kill the process currently listening on a port
    Kill {
        /// Port to target
        #[arg(long)]
        port: u16,
        /// Signal to send (term or kill)
        #[arg(long, default_value = "term")]
        signal: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show what would be killed without sending signal
        #[arg(long)]
        dry_run: bool,
    },
    /// Interactive dashboard for daemon, leases, and listeners
    Dashboard,
    /// Run a command with an allocated port
    Run {
        /// Service name
        #[arg(short, long)]
        service: String,
        /// Preferred port
        #[arg(long)]
        port: Option<u16>,
        /// Project path (defaults to current directory)
        #[arg(short, long)]
        project: Option<String>,
        /// Environment variable name for the port (default: PORT)
        #[arg(long, default_value = "PORT")]
        env_var: String,
        /// Reassign automatically if the preferred port is unavailable
        #[arg(long)]
        auto_reassign: bool,
        /// Command and arguments to run
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Manage the portus daemon
    Daemon {
        /// Output as JSON (applies to `status` subcommand)
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon
    Start,
    /// Stop the daemon
    Stop,
    /// Show daemon status
    Status,
}

#[derive(Debug, Serialize)]
struct ScanRow {
    port: u16,
    pid: u32,
    protocol: Protocol,
    command: String,
    managed: bool,
    service: Option<String>,
    project: Option<String>,
    lease_state: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Request {
            project,
            service,
            port,
            json,
            auto_reassign,
            dry_run,
        } => {
            if dry_run {
                let port = port.context("--dry-run requires --port")?;
                let bindable = port_check::is_port_available(port, Protocol::Tcp);
                let leases = load_active_leases()?;
                let managed = leases.iter().find(|l| l.port == port && l.protocol == Protocol::Tcp);

                let (available, reason) = match (bindable, managed) {
                    (true, None) => (true, None),
                    (false, Some(lease)) => (
                        false,
                        Some(format!(
                            "allocated to service '{}' (lease {})",
                            lease.service_name, lease.lease_id
                        )),
                    ),
                    (false, None) => (false, Some("port is in use by another process".into())),
                    (true, Some(lease)) => (
                        false,
                        Some(format!(
                            "allocated to service '{}' (lease {}) but not yet bound",
                            lease.service_name, lease.lease_id
                        )),
                    ),
                };

                if json {
                    let mut obj = serde_json::json!({
                        "available": available,
                        "port": port,
                    });
                    if let Some(reason) = &reason {
                        obj["reason"] = serde_json::Value::String(reason.clone());
                    }
                    println!("{}", serde_json::to_string_pretty(&obj)?);
                } else if available {
                    println!("Port {} is available", port);
                } else {
                    println!("Port {} is not available: {}", port, reason.unwrap());
                }
            } else {
                let project = resolve_project(project)?;
                let requested_port = port;
                let response = send_request(Request::Allocate {
                    project,
                    service,
                    preferred_port: requested_port,
                    protocol: Protocol::Tcp,
                    auto_reassign,
                    pid: Some(std::process::id()),
                })
                .await?;

                match response {
                    Response::Allocated { lease } => {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&lease)?);
                        } else {
                            println!("✓ Allocated port {} for service '{}'", lease.port, lease.service_name);
                            println!("  Lease ID: {}", lease.lease_id);
                            println!("  Token:    {}", lease.session_token);
                            println!("  Expires:  {}", lease.expires_at);
                            if let Some(requested_port) = requested_port {
                                if requested_port != lease.port {
                                    println!("  Reassigned from requested port {}", requested_port);
                                }
                            }
                            println!();
                            println!(
                                "  Confirm after binding:  portus confirm --lease-id {} --token {}",
                                lease.lease_id, lease.session_token
                            );
                        }
                    }
                    Response::Error { code, message } => {
                        bail!("{}", format_daemon_error(&code, &message));
                    }
                    other => bail!("unexpected response: {:?}", other),
                }
            }
        }

        Commands::Confirm { lease_id, token, json } => {
            let response = send_request(Request::Confirm {
                lease_id: lease_id.clone(),
                session_token: token,
            })
            .await?;
            match response {
                Response::Confirmed { lease_id } => {
                    if json {
                        println!("{}", serde_json::json!({"confirmed": true, "lease_id": lease_id}));
                    } else {
                        println!("✓ Lease {} confirmed", lease_id);
                    }
                }
                Response::Error { code, message } => bail!("{}", format_daemon_error(&code, &message)),
                other => bail!("unexpected response: {:?}", other),
            }
        }

        Commands::Release { lease_id, token, json } => {
            let response = send_request(Request::Release {
                lease_id: lease_id.clone(),
                session_token: token,
            })
            .await?;
            match response {
                Response::Released { lease_id } => {
                    if json {
                        println!("{}", serde_json::json!({"released": true, "lease_id": lease_id}));
                    } else {
                        println!("✓ Lease {} released", lease_id);
                    }
                }
                Response::Error { code, message } => bail!("{}", format_daemon_error(&code, &message)),
                other => bail!("unexpected response: {:?}", other),
            }
        }

        Commands::List { project, json } => {
            let response = send_request(Request::List {
                project_filter: project,
            })
            .await?;
            match response {
                Response::LeaseList { leases } => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&leases)?);
                    } else if leases.is_empty() {
                        println!("No active port allocations.");
                    } else {
                        println!("{:<8} {:<12} {:<20} {:<10} {:<10}", "PORT", "SERVICE", "PROJECT", "STATE", "PROTOCOL");
                        println!("{}", "-".repeat(60));
                        for lease in &leases {
                            let project_short = if lease.project_path.len() > 18 {
                                format!("...{}", &lease.project_path[lease.project_path.len() - 15..])
                            } else {
                                lease.project_path.clone()
                            };
                            println!(
                                "{:<8} {:<12} {:<20} {:<10} {:?}",
                                lease.port,
                                lease.service_name,
                                project_short,
                                format!("{:?}", lease.state).to_lowercase(),
                                lease.protocol,
                            );
                        }
                        println!("
{} allocation(s)", leases.len());
                    }
                }
                Response::Error { code, message } => bail!("{}", format_daemon_error(&code, &message)),
                other => bail!("unexpected response: {:?}", other),
            }
        }

        Commands::Status { json } => {
            let response = send_request(Request::Status).await?;
            match response {
                Response::DaemonStatus {
                    pid,
                    uptime_secs,
                    active_leases,
                    socket_path,
                } => {
                    if json {
                        println!("{}", serde_json::json!({
                            "pid": pid,
                            "uptime_secs": uptime_secs,
                            "active_leases": active_leases,
                            "socket_path": socket_path,
                        }));
                    } else {
                        println!("Portus Daemon");
                        println!("  PID:           {}", pid);
                        println!("  Uptime:        {}s", uptime_secs);
                        println!("  Active leases: {}", active_leases);
                        println!("  Socket:        {}", socket_path);
                    }
                }
                Response::Error { code, message } => bail!("{}", format_daemon_error(&code, &message)),
                other => bail!("unexpected response: {:?}", other),
            }
        }

        Commands::Scan { port, json } => {
            let rows = build_scan_rows(port)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                match port {
                    Some(port) => println!("No listeners found on port {}.", port),
                    None => println!("No listeners found."),
                }
            } else {
                println!("{:<8} {:<8} {:<8} {:<14} {:<12} {}", "PORT", "PID", "PROTO", "MANAGED", "SERVICE", "COMMAND");
                println!("{}", "-".repeat(72));
                for row in rows {
                    println!(
                        "{:<8} {:<8} {:<8} {:<14} {:<12} {}",
                        row.port,
                        row.pid,
                        format!("{:?}", row.protocol).to_lowercase(),
                        if row.managed {
                            row.project
                                .as_ref()
                                .map(|project| shorten_project(project))
                                .unwrap_or_else(|| "yes".into())
                        } else {
                            "no".into()
                        },
                        row.service.unwrap_or_else(|| "-".into()),
                        row.command,
                    );
                }
            }
        }

        Commands::Kill { port, signal, json, dry_run } => {
            let signal = parse_signal(&signal)?;
            let rows = build_scan_rows(Some(port))?;
            if rows.is_empty() {
                bail!("no listening process found on port {}", port);
            }

            if dry_run {
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    println!("Would kill {} process(es) on port {} with {}:", rows.len(), port, signal);
                    for row in &rows {
                        let managed = if row.managed {
                            format!(
                                "managed by {}:{}",
                                row.project.as_deref().unwrap_or("unknown-project"),
                                row.service.as_deref().unwrap_or("unknown-service"),
                            )
                        } else {
                            "unmanaged".into()
                        };
                        println!(
                            "  Would kill: pid={} proto={:?} cmd={} ({})",
                            row.pid, row.protocol, row.command, managed
                        );
                    }
                }
            } else {
                let killed = kill_processes_on_port(port, signal)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    println!("✓ Sent {} to {} process(es) on port {}", signal, unique_pid_count(&killed), port);
                    for row in rows {
                        let managed = if row.managed {
                            format!(
                                "managed by {}:{}",
                                row.project.unwrap_or_else(|| "unknown-project".into()),
                                row.service.unwrap_or_else(|| "unknown-service".into())
                            )
                        } else {
                            "unmanaged".into()
                        };
                        println!(
                            "  pid={} proto={:?} cmd={} ({})",
                            row.pid, row.protocol, row.command, managed
                        );
                    }
                }
            }
        }

        Commands::Dashboard => {
            dashboard::run_dashboard()?;
        }

        Commands::Run {
            service,
            port,
            project,
            env_var,
            auto_reassign,
            command,
        } => {
            let project = resolve_project(project)?;
            let requested_port = port;
            let response = send_request(Request::Allocate {
                project,
                service: service.clone(),
                preferred_port: requested_port,
                protocol: Protocol::Tcp,
                auto_reassign,
                pid: Some(std::process::id()),
            })
            .await?;

            let lease = match response {
                Response::Allocated { lease } => lease,
                Response::Error { code, message } => bail!("{}", format_daemon_error(&code, &message)),
                other => bail!("unexpected response: {:?}", other),
            };

            eprintln!("✓ Allocated port {} for service '{}'", lease.port, service);
            if let Some(requested_port) = requested_port {
                if requested_port != lease.port {
                    eprintln!("✓ Reassigned requested port {} to {}", requested_port, lease.port);
                }
            }

            let (cmd, args) = command.split_first().context("no command specified")?;
            let mut child = tokio::process::Command::new(cmd)
                .args(args)
                .env(&env_var, lease.port.to_string())
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .with_context(|| format!("failed to spawn: {}", cmd))?;

            let child_pid = child.id().context("failed to determine child pid")?;
            wait_for_child_bind(&lease, child_pid, &mut child).await?;
            send_request(Request::Confirm {
                lease_id: lease.lease_id.clone(),
                session_token: lease.session_token.clone(),
            })
            .await?;
            eprintln!("✓ Confirmed port {} for service '{}'", lease.port, service);

            let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::oneshot::channel();
            let heartbeat_lease_id = lease.lease_id.clone();
            let heartbeat_token = lease.session_token.clone();
            let heartbeat_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(60));
                interval.tick().await;
                loop {
                    tokio::select! {
                        _ = &mut heartbeat_rx => break,
                        _ = interval.tick() => {
                            let _ = send_request(Request::Heartbeat {
                                lease_id: heartbeat_lease_id.clone(),
                                session_token: heartbeat_token.clone(),
                            }).await;
                        }
                    }
                }
            });

            let mut terminate = tokio::signal::unix::signal(SignalKind::terminate())
                .context("failed to install SIGTERM handler")?;

            let exit_code = tokio::select! {
                status = child.wait() => {
                    let status = status.context("failed to wait for child")?;

                    let _ = heartbeat_tx.send(());
                    let _ = heartbeat_task.await;
                    let _ = send_request(Request::Release {
                        lease_id: lease.lease_id.clone(),
                        session_token: lease.session_token.clone(),
                    })
                    .await;

                    eprintln!("✓ Released port {} for service '{}'", lease.port, service);
                    status.code().unwrap_or(1)
                }
                result = tokio::signal::ctrl_c() => {
                    result.context("failed to listen for SIGINT")?;

                    let _ = heartbeat_tx.send(());
                    let _ = heartbeat_task.await;
                    let _ = send_request(Request::Release {
                        lease_id: lease.lease_id.clone(),
                        session_token: lease.session_token.clone(),
                    })
                    .await;
                    let _ = child.kill().await;
                    let _ = child.wait().await;

                    eprintln!("✓ Released port {} for service '{}'", lease.port, service);
                    130
                }
                _ = terminate.recv() => {
                    let _ = heartbeat_tx.send(());
                    let _ = heartbeat_task.await;
                    let _ = send_request(Request::Release {
                        lease_id: lease.lease_id.clone(),
                        session_token: lease.session_token.clone(),
                    })
                    .await;
                    let _ = child.kill().await;
                    let _ = child.wait().await;

                    eprintln!("✓ Released port {} for service '{}'", lease.port, service);
                    143
                }
            };

            std::process::exit(exit_code);
        }

        Commands::Daemon { action, json } => match action {
            DaemonAction::Start => {
                let socket_path = paths::socket_path()?;
                if try_connect(&socket_path).await {
                    println!("Daemon is already running.");
                    return Ok(());
                }
                start_daemon().await?;
                println!("✓ Daemon started");
            }
            DaemonAction::Stop => {
                let response = send_request(Request::Shutdown).await?;
                match response {
                    Response::ShuttingDown => println!("✓ Daemon shutting down"),
                    other => bail!("unexpected response: {:?}", other),
                }
            }
            DaemonAction::Status => {
                let socket_path = paths::socket_path()?;
                let not_running = || {
                    if json {
                        println!("{}", serde_json::json!({"running": false}));
                    } else {
                        println!("✗ Daemon is not running");
                    }
                };
                if !try_connect(&socket_path).await {
                    not_running();
                } else {
                    match ipc::connect(&socket_path).await {
                        Ok(stream) => match send_on_stream(stream, Request::Status).await {
                            Ok(Response::DaemonStatus {
                                pid,
                                uptime_secs,
                                active_leases,
                                socket_path,
                            }) => {
                                if json {
                                    println!("{}", serde_json::json!({
                                        "running": true,
                                        "pid": pid,
                                        "uptime_secs": uptime_secs,
                                        "active_leases": active_leases,
                                        "socket_path": socket_path,
                                    }));
                                } else {
                                    println!(
                                        "✓ Daemon running (PID {}, uptime {}s, {} active leases)",
                                        pid, uptime_secs, active_leases
                                    );
                                }
                            }
                            _ => not_running(),
                        },
                        Err(_) => not_running(),
                    }
                }
            }
        },
    }

    Ok(())
}

fn resolve_project(project: Option<String>) -> Result<String> {
    match project {
        Some(p) => Ok(p),
        None => {
            let cwd = std::env::current_dir().context("cannot get current directory")?;
            Ok(cwd.display().to_string())
        }
    }
}

fn parse_signal(signal: &str) -> Result<&'static str> {
    match signal.to_ascii_lowercase().as_str() {
        "term" | "sigterm" => Ok("TERM"),
        "kill" | "sigkill" => Ok("KILL"),
        other => bail!("unknown signal: '{}' (use 'term' or 'kill')", other),
    }
}

fn build_scan_rows(port: Option<u16>) -> Result<Vec<ScanRow>> {
    let listeners = scan_ports(port)?;
    let leases = load_active_leases()?;
    let mut rows = Vec::new();

    for listener in listeners {
        let managed_lease = leases
            .iter()
            .find(|lease| lease.port == listener.port && lease.protocol == listener.protocol);

        rows.push(ScanRow {
            port: listener.port,
            pid: listener.pid,
            protocol: listener.protocol,
            command: listener.command,
            managed: managed_lease.is_some(),
            service: managed_lease.map(|lease| lease.service_name.clone()),
            project: managed_lease.map(|lease| lease.project_path.clone()),
            lease_state: managed_lease.map(|lease| format!("{:?}", lease.state).to_lowercase()),
        });
    }

    Ok(rows)
}

fn load_active_leases() -> Result<Vec<Lease>> {
    let registry_path = paths::registry_path()?;
    if !registry_path.exists() {
        return Ok(Vec::new());
    }

    let registry = Registry::load(&registry_path)?;
    let mut leases: Vec<Lease> = registry
        .list(None)
        .into_iter()
        .filter(|lease| matches!(lease.state, LeaseState::Pending | LeaseState::Active))
        .cloned()
        .collect();
    leases.sort_by(|a, b| a.port.cmp(&b.port).then_with(|| a.service_name.cmp(&b.service_name)));
    Ok(leases)
}

fn is_child_listening(port: u16, pid: u32, protocol: Protocol) -> Result<bool> {
    let listeners = scan_ports(Some(port))?;
    Ok(listeners
        .into_iter()
        .any(|listener| listener.pid == pid && listener.protocol == protocol))
}

async fn wait_for_child_bind(
    lease: &Lease,
    child_pid: u32,
    child: &mut tokio::process::Child,
) -> Result<()> {
    loop {
        if let Some(status) = child.try_wait().context("failed to poll child process")? {
            bail!(
                "child exited before binding port {} (exit: {:?})",
                lease.port,
                status.code()
            );
        }

        if is_child_listening(lease.port, child_pid, lease.protocol)? {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn unique_pid_count(processes: &[portus_core::scan::PortProcess]) -> usize {
    use std::collections::HashSet;

    processes.iter().map(|process| process.pid).collect::<HashSet<_>>().len()
}

fn shorten_project(project: &str) -> String {
    const MAX_LEN: usize = 14;
    if project.len() <= MAX_LEN {
        project.to_string()
    } else {
        format!("...{}", &project[project.len() - (MAX_LEN - 3)..])
    }
}

async fn try_connect(socket_path: &std::path::Path) -> bool {
    ipc::connect(socket_path).await.is_ok()
}

async fn send_request(request: Request) -> Result<Response> {
    let socket_path = paths::socket_path()?;

    let stream = match ipc::connect(&socket_path).await {
        Ok(s) => s,
        Err(_) => {
            eprintln!("Starting portus daemon...");
            start_daemon().await?;
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if let Ok(s) = ipc::connect(&socket_path).await {
                    return send_on_stream(s, request).await;
                }
            }
            bail!("daemon did not start in time");
        }
    };

    send_on_stream(stream, request).await
}

async fn send_on_stream(stream: interprocess::local_socket::tokio::Stream, request: Request) -> Result<Response> {
    let (mut reader, mut writer) = stream.split();
    transport::send_json(&mut writer, &request).await?;
    let response: Response = transport::recv_json(&mut reader)
        .await?
        .context("daemon closed connection without responding")?;
    Ok(response)
}

async fn start_daemon() -> Result<()> {
    paths::ensure_config_dir()?;

    let daemon_bin = {
        let self_path = std::env::current_exe().context("cannot find self path")?;
        let dir = self_path.parent().unwrap();
        let candidate = dir.join("portusd");
        if candidate.exists() {
            candidate
        } else {
            which_portusd().unwrap_or(candidate)
        }
    };

    std::process::Command::new(&daemon_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start daemon: {}", daemon_bin.display()))?;

    Ok(())
}

/// Strips the `[code]` prefix and appends actionable suggestions based on error content.
fn format_daemon_error(code: &str, message: &str) -> String {
    let suggestion = if message.contains("already allocated") {
        let port_hint = extract_port_from_message(message)
            .map(|p| format!(" --port {}", p))
            .unwrap_or_default();
        format!(
            "\n  Try: portus release --lease-id <id> --token <token>\n       portus list{} to find the existing lease\n       or use --auto-reassign to pick another port",
            port_hint,
        )
    } else if message.contains("in use by another process") {
        let port_hint = extract_port_from_message(message)
            .map(|p| format!(" --port {}", p))
            .unwrap_or_default();
        format!(
            "\n  Try: portus kill{} to terminate the blocking process\n       or use --auto-reassign to pick another port",
            port_hint,
        )
    } else if message.contains("invalid") && message.contains("token") {
        "\n  Try: portus list --json to find the correct lease-id and token".to_string()
    } else if message.contains("not found")
        && (code == "confirm_failed" || code == "release_failed" || code == "heartbeat_failed")
    {
        "\n  Try: portus list to see active leases".to_string()
    } else {
        String::new()
    };

    let display_msg = uppercase_first(message);
    format!("{}{}", display_msg, suggestion)
}

fn extract_port_from_message(message: &str) -> Option<u16> {
    let idx = message.find("port ")?;
    let after = idx + 5;
    let end = message[after..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| after + i)
        .unwrap_or(message.len());
    message[after..end].parse().ok()
}

fn uppercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn which_portusd() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join("portusd");
            candidate.exists().then_some(candidate)
        })
    })
}
