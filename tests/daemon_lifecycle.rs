use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use interprocess::local_socket::traits::tokio::Stream as _;
use portus_core::{ipc, transport, LeaseState, Protocol, Request, Response};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::sleep;

struct DaemonHarness {
    _home: TempDir,
    child: Child,
    socket_path: PathBuf,
}

impl DaemonHarness {
    async fn start() -> Result<Self> {
        let home = tempfile::tempdir().context("failed to create temp home")?;
        let socket_path = home.path().join(".config/portus").join(socket_file_name());
        let child = Command::new(daemon_binary_path()?)
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn portusd")?;

        let mut harness = Self {
            _home: home,
            child,
            socket_path,
        };
        harness.wait_until_ready().await?;
        Ok(harness)
    }

    async fn wait_until_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match ipc::connect(&self.socket_path).await {
                Ok(stream) => {
                    drop(stream);
                    return Ok(());
                }
                Err(err) => {
                    if let Some(status) = self.child.try_wait().context("failed to poll portusd")? {
                        bail!("portusd exited before becoming ready: {status}");
                    }
                    if Instant::now() >= deadline {
                        return Err(err).with_context(|| {
                            format!(
                                "timed out waiting for daemon socket at {}",
                                self.socket_path.display()
                            )
                        });
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    async fn connect(&self) -> Result<interprocess::local_socket::tokio::Stream> {
        ipc::connect(&self.socket_path)
            .await
            .with_context(|| format!("failed to connect to {}", self.socket_path.display()))
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn wait_for_exit(&mut self) -> Result<()> {
        let status = self.child.wait().context("failed waiting for portusd exit")?;
        if status.success() {
            Ok(())
        } else {
            bail!("portusd exited with status {status}")
        }
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn daemon_binary_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_portusd") {
        return Ok(PathBuf::from(path));
    }

    let current_exe = std::env::current_exe().context("failed to resolve test binary path")?;
    let debug_dir = current_exe
        .parent()
        .and_then(Path::parent)
        .context("failed to locate target debug directory")?;
    let daemon_path = debug_dir.join(format!("portusd{}", std::env::consts::EXE_SUFFIX));
    if daemon_path.is_file() {
        Ok(daemon_path)
    } else {
        bail!("portusd binary not found at {}", daemon_path.display())
    }
}

fn socket_file_name() -> &'static str {
    #[cfg(unix)]
    {
        "portus.sock"
    }

    #[cfg(windows)]
    {
        "portus.pipe"
    }
}

async fn send_request<R, W>(reader: &mut R, writer: &mut W, request: Request) -> Result<Response>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    transport::send_json(writer, &request).await?;
    transport::recv_json(reader)
        .await?
        .context("daemon closed the IPC stream")
}

#[tokio::test]
async fn full_lifecycle() -> Result<()> {
    let mut daemon = DaemonHarness::start().await?;
    let stream = daemon.connect().await?;
    let (mut reader, mut writer) = stream.split();

    let allocated = send_request(
        &mut reader,
        &mut writer,
        Request::Allocate {
            project: "/tmp/portus/full_lifecycle".into(),
            service: "web".into(),
            preferred_port: None,
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: Some(std::process::id()),
        },
    )
    .await?;

    let lease = match allocated {
        Response::Allocated { lease } => {
            assert_eq!(lease.state, LeaseState::Pending);
            lease
        }
        other => bail!("expected allocated response, got {other:?}"),
    };

    let confirmed = send_request(
        &mut reader,
        &mut writer,
        Request::Confirm {
            lease_id: lease.lease_id.clone(),
            session_token: lease.session_token.clone(),
        },
    )
    .await?;
    assert!(matches!(
        confirmed,
        Response::Confirmed { ref lease_id } if lease_id == &lease.lease_id
    ));

    let heartbeat = send_request(
        &mut reader,
        &mut writer,
        Request::Heartbeat {
            lease_id: lease.lease_id.clone(),
            session_token: lease.session_token.clone(),
        },
    )
    .await?;
    match heartbeat {
        Response::HeartbeatAck {
            lease_id,
            expires_at,
        } => {
            assert_eq!(lease_id, lease.lease_id);
            assert!(!expires_at.is_empty());
        }
        other => bail!("expected heartbeat ack, got {other:?}"),
    }

    let listed = send_request(
        &mut reader,
        &mut writer,
        Request::List {
            project_filter: None,
        },
    )
    .await?;
    match listed {
        Response::LeaseList { leases } => {
            assert_eq!(leases.len(), 1);
            assert_eq!(leases[0].lease_id, lease.lease_id);
            assert_eq!(leases[0].state, LeaseState::Active);
        }
        other => bail!("expected lease list, got {other:?}"),
    }

    let released = send_request(
        &mut reader,
        &mut writer,
        Request::Release {
            lease_id: lease.lease_id.clone(),
            session_token: lease.session_token.clone(),
        },
    )
    .await?;
    assert!(matches!(
        released,
        Response::Released { ref lease_id } if lease_id == &lease.lease_id
    ));

    let listed = send_request(
        &mut reader,
        &mut writer,
        Request::List {
            project_filter: None,
        },
    )
    .await?;
    match listed {
        Response::LeaseList { leases } => assert!(leases.is_empty()),
        other => bail!("expected empty lease list, got {other:?}"),
    }

    let status = send_request(&mut reader, &mut writer, Request::Status).await?;
    match status {
        Response::DaemonStatus {
            pid,
            active_leases,
            socket_path,
            ..
        } => {
            assert_eq!(pid, daemon.pid());
            assert_eq!(active_leases, 0);
            assert_eq!(socket_path, daemon.socket_path().display().to_string());
        }
        other => bail!("expected daemon status, got {other:?}"),
    }

    let shutdown = send_request(&mut reader, &mut writer, Request::Shutdown).await?;
    assert!(matches!(shutdown, Response::ShuttingDown));
    drop(reader);
    drop(writer);
    daemon.wait_for_exit()?;
    Ok(())
}

#[tokio::test]
async fn auto_reassign_integration() -> Result<()> {
    let mut daemon = DaemonHarness::start().await?;
    let stream = daemon.connect().await?;
    let (mut reader, mut writer) = stream.split();

    // Service A: allocate port 9900 with auto_reassign=false → expect exactly 9900
    let resp_a = send_request(
        &mut reader,
        &mut writer,
        Request::Allocate {
            project: "/tmp/portus/auto_reassign".into(),
            service: "service-a".into(),
            preferred_port: Some(9900),
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: Some(std::process::id()),
        },
    )
    .await?;
    let lease_a = match resp_a {
        Response::Allocated { lease } => {
            assert_eq!(lease.port, 9900, "service A should get exactly port 9900");
            lease
        }
        other => bail!("expected allocated for service A, got {other:?}"),
    };

    // Service B: allocate port 9900 with auto_reassign=true → expect port ≥ 10000
    let resp_b = send_request(
        &mut reader,
        &mut writer,
        Request::Allocate {
            project: "/tmp/portus/auto_reassign".into(),
            service: "service-b".into(),
            preferred_port: Some(9900),
            protocol: Protocol::Tcp,
            auto_reassign: true,
            pid: Some(std::process::id()),
        },
    )
    .await?;
    let lease_b = match resp_b {
        Response::Allocated { lease } => {
            assert!(
                lease.port >= 10000,
                "service B should be reassigned to auto range (≥10000), got {}",
                lease.port
            );
            lease
        }
        other => bail!("expected allocated for service B, got {other:?}"),
    };

    // Service C: allocate port 9900 with auto_reassign=false → expect Error
    let resp_c = send_request(
        &mut reader,
        &mut writer,
        Request::Allocate {
            project: "/tmp/portus/auto_reassign".into(),
            service: "service-c".into(),
            preferred_port: Some(9900),
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: Some(std::process::id()),
        },
    )
    .await?;
    match resp_c {
        Response::Error { code, message } => {
            assert_eq!(code, "allocation_failed");
            assert!(
                message.to_lowercase().contains("in use")
                    || message.to_lowercase().contains("already")
                    || message.to_lowercase().contains("conflict"),
                "error message should indicate port conflict, got: {message}"
            );
        }
        other => bail!("expected error for service C, got {other:?}"),
    }

    send_request(
        &mut reader,
        &mut writer,
        Request::Release {
            lease_id: lease_a.lease_id.clone(),
            session_token: lease_a.session_token.clone(),
        },
    )
    .await?;
    send_request(
        &mut reader,
        &mut writer,
        Request::Release {
            lease_id: lease_b.lease_id.clone(),
            session_token: lease_b.session_token.clone(),
        },
    )
    .await?;

    let shutdown = send_request(&mut reader, &mut writer, Request::Shutdown).await?;
    assert!(matches!(shutdown, Response::ShuttingDown));
    drop(reader);
    drop(writer);
    daemon.wait_for_exit()?;
    Ok(())
}

#[tokio::test]
async fn error_handling() -> Result<()> {
    let mut daemon = DaemonHarness::start().await?;
    let stream = daemon.connect().await?;
    let (mut reader, mut writer) = stream.split();

    let allocated = send_request(
        &mut reader,
        &mut writer,
        Request::Allocate {
            project: "/tmp/portus/error_handling".into(),
            service: "api".into(),
            preferred_port: None,
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: Some(std::process::id()),
        },
    )
    .await?;

    let lease = match allocated {
        Response::Allocated { lease } => lease,
        other => bail!("expected allocated response, got {other:?}"),
    };

    let wrong_token = send_request(
        &mut reader,
        &mut writer,
        Request::Confirm {
            lease_id: lease.lease_id.clone(),
            session_token: "wrong-token".into(),
        },
    )
    .await?;
    match wrong_token {
        Response::Error { code, message } => {
            assert_eq!(code, "confirm_failed");
            assert!(message.contains("invalid session token"));
        }
        other => bail!("expected token error, got {other:?}"),
    }

    let duplicate_service = send_request(
        &mut reader,
        &mut writer,
        Request::Allocate {
            project: "/tmp/portus/error_handling".into(),
            service: "api".into(),
            preferred_port: None,
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: Some(std::process::id()),
        },
    )
    .await?;
    match duplicate_service {
        Response::Error { code, message } => {
            assert_eq!(code, "allocation_failed");
            assert!(message.contains("already has an active lease"));
        }
        other => bail!("expected duplicate service error, got {other:?}"),
    }

    let released = send_request(
        &mut reader,
        &mut writer,
        Request::Release {
            lease_id: lease.lease_id.clone(),
            session_token: lease.session_token.clone(),
        },
    )
    .await?;
    assert!(matches!(
        released,
        Response::Released { ref lease_id } if lease_id == &lease.lease_id
    ));

    let shutdown = send_request(&mut reader, &mut writer, Request::Shutdown).await?;
    assert!(matches!(shutdown, Response::ShuttingDown));
    drop(reader);
    drop(writer);
    daemon.wait_for_exit()?;
    Ok(())
}
