use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use portus_core::ipc;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::sleep;

struct DaemonHarness {
    _home: TempDir,
    child: Child,
    socket_path: PathBuf,
    home_path: PathBuf,
}

impl DaemonHarness {
    async fn start() -> Result<Self> {
        let home = tempfile::tempdir().context("failed to create temp home")?;
        let home_path = home.path().to_path_buf();
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
            home_path,
        };
        harness.wait_until_ready().await?;
        Ok(harness)
    }

    async fn wait_until_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
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

    fn home(&self) -> &Path {
        &self.home_path
    }

    fn shutdown_and_wait(&mut self) -> Result<()> {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        Ok(())
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

fn cli_binary_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_portus") {
        return Ok(PathBuf::from(path));
    }
    let current_exe = std::env::current_exe().context("failed to resolve test binary path")?;
    let debug_dir = current_exe
        .parent()
        .and_then(Path::parent)
        .context("failed to locate target debug directory")?;
    let cli_path = debug_dir.join(format!("portus{}", std::env::consts::EXE_SUFFIX));
    if cli_path.is_file() {
        Ok(cli_path)
    } else {
        bail!("portus binary not found at {}", cli_path.display())
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

struct McpProcess {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl McpProcess {
    fn spawn(home: &Path) -> Result<Self> {
        let mut child = tokio::process::Command::new(cli_binary_path()?)
            .args(["mcp", "serve"])
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn portus mcp serve")?;

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    async fn send(&mut self, msg: &serde_json::Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("failed to write to MCP stdin")?;
        self.stdin.flush().await.context("failed to flush MCP stdin")?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<serde_json::Value> {
        let mut line = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                result = self.stdout.read_line(&mut line) => {
                    let n = result.context("failed to read from MCP stdout")?;
                    if n == 0 {
                        bail!("MCP process closed stdout (EOF)");
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        line.clear();
                        continue;
                    }
                    let parsed: serde_json::Value = serde_json::from_str(trimmed)
                        .with_context(|| format!("stdout line is not valid JSON: {:?}", trimmed))?;
                    return Ok(parsed);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("timed out waiting for MCP response");
                }
            }
        }
    }

    async fn kill(&mut self) -> Result<()> {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        Ok(())
    }
}

#[tokio::test]
async fn mcp_stdio_protocol() -> Result<()> {
    // given: isolated daemon running
    let mut daemon = DaemonHarness::start().await?;
    let mut mcp = McpProcess::spawn(daemon.home())?;

    // when: send initialize
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "portus-test", "version": "0.1" }
        }
    }))
    .await?;

    // then: valid JSON-RPC with serverInfo
    let init_resp = mcp.recv().await?;
    assert_eq!(init_resp["jsonrpc"], "2.0");
    assert_eq!(init_resp["id"], 1);
    assert!(
        init_resp.get("result").is_some(),
        "initialize response should have `result`, got: {}",
        init_resp,
    );
    assert!(
        init_resp["result"]["serverInfo"].is_object(),
        "result should contain serverInfo, got: {}",
        init_resp["result"],
    );

    // when: send initialized notification
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }))
    .await?;

    // when: send tools/list
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    }))
    .await?;

    // then: 5 tools returned with correct names
    let list_resp = mcp.recv().await?;
    assert_eq!(list_resp["jsonrpc"], "2.0");
    assert_eq!(list_resp["id"], 2);
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools/list should return an array of tools");
    assert_eq!(
        tools.len(),
        5,
        "expected 5 tools, got: {:?}",
        tools.iter().map(|t| t["name"].as_str()).collect::<Vec<_>>(),
    );

    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"allocate_port"), "missing allocate_port");
    assert!(tool_names.contains(&"release_port"), "missing release_port");
    assert!(tool_names.contains(&"list_ports"), "missing list_ports");
    assert!(tool_names.contains(&"check_port"), "missing check_port");
    assert!(tool_names.contains(&"daemon_status"), "missing daemon_status");

    // when: call allocate_port
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "allocate_port",
            "arguments": { "service": "mcp-test" }
        }
    }))
    .await?;

    // then: response contains port, lease_id, token
    let alloc_resp = mcp.recv().await?;
    assert_eq!(alloc_resp["jsonrpc"], "2.0");
    assert_eq!(alloc_resp["id"], 3);
    let content = alloc_resp["result"]["content"]
        .as_array()
        .expect("tools/call should return content array");
    assert!(!content.is_empty(), "content array should not be empty");

    let text = content[0]["text"]
        .as_str()
        .expect("content[0] should have text field");
    let alloc_data: serde_json::Value =
        serde_json::from_str(text).context("allocate_port result text should be valid JSON")?;
    assert!(
        alloc_data["port"].as_u64().is_some(),
        "result should contain a port number, got: {}",
        alloc_data,
    );
    assert!(alloc_data["lease_id"].as_str().is_some());
    assert!(alloc_data["token"].as_str().is_some());

    let allocated_port = alloc_data["port"].as_u64().unwrap();
    let lease_id = alloc_data["lease_id"].as_str().unwrap();
    let token = alloc_data["token"].as_str().unwrap();

    // when: call list_ports
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "list_ports",
            "arguments": {}
        }
    }))
    .await?;

    // then: exactly 1 lease with our port
    let list_result = mcp.recv().await?;
    assert_eq!(list_result["id"], 4);
    let list_text = list_result["result"]["content"][0]["text"]
        .as_str()
        .expect("list_ports should return text");
    let list_data: serde_json::Value = serde_json::from_str(list_text)?;
    let leases_arr = list_data["leases"]
        .as_array()
        .expect("list_ports should return object with leases array");
    assert_eq!(leases_arr.len(), 1);
    assert_eq!(leases_arr[0]["port"].as_u64().unwrap(), allocated_port);

    // when: call daemon_status
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "daemon_status",
            "arguments": {}
        }
    }))
    .await?;

    // then: 1 active lease reported
    let status_resp = mcp.recv().await?;
    assert_eq!(status_resp["id"], 5);
    let status_text = status_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("daemon_status should return text");
    let status_data: serde_json::Value = serde_json::from_str(status_text)?;
    assert_eq!(status_data["active_leases"].as_u64().unwrap(), 1);

    // when: call release_port
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "release_port",
            "arguments": {
                "lease_id": lease_id,
                "token": token
            }
        }
    }))
    .await?;

    // then: released=true
    let release_resp = mcp.recv().await?;
    assert_eq!(release_resp["id"], 6);
    let release_text = release_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("release_port should return text");
    let release_data: serde_json::Value = serde_json::from_str(release_text)?;
    assert_eq!(release_data["released"], true);

    mcp.kill().await?;
    daemon.shutdown_and_wait()?;
    Ok(())
}

#[tokio::test]
async fn mcp_stdout_cleanliness() -> Result<()> {
    let mut daemon = DaemonHarness::start().await?;
    let mut mcp = McpProcess::spawn(daemon.home())?;

    // given: a completed MCP handshake
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "clean-test", "version": "0.1" }
        }
    }))
    .await?;
    let _ = mcp.recv().await?;

    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }))
    .await?;

    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    }))
    .await?;
    let _ = mcp.recv().await?;

    // when: stdin closes (triggers MCP server shutdown)
    drop(mcp.stdin);

    // then: every remaining stdout line is valid JSON (no log leaks)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let mut line = String::new();
        tokio::select! {
            result = mcp.stdout.read_line(&mut line) => {
                match result {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            assert!(
                                serde_json::from_str::<serde_json::Value>(trimmed).is_ok(),
                                "non-JSON line on stdout: {:?}",
                                trimmed,
                            );
                        }
                    }
                    Err(e) => bail!("error reading remaining stdout: {e}"),
                }
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }

    mcp.child.kill().await.ok();
    daemon.shutdown_and_wait()?;
    Ok(())
}

#[tokio::test]
async fn mcp_check_port_tool() -> Result<()> {
    let mut daemon = DaemonHarness::start().await?;
    let mut mcp = McpProcess::spawn(daemon.home())?;

    // given: initialized MCP session
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "check-test", "version": "0.1" }
        }
    }))
    .await?;
    let _ = mcp.recv().await?;

    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }))
    .await?;

    // when: check a high ephemeral port
    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "check_port",
            "arguments": { "port": 59123 }
        }
    }))
    .await?;

    // then: response has 'available' field
    let check_resp = mcp.recv().await?;
    assert_eq!(check_resp["id"], 2);
    let check_text = check_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("check_port should return text");
    let check_data: serde_json::Value = serde_json::from_str(check_text)?;
    assert!(
        check_data.get("available").is_some(),
        "check_port result should have 'available' field, got: {}",
        check_data,
    );

    mcp.kill().await?;
    daemon.shutdown_and_wait()?;
    Ok(())
}
