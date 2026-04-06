use std::collections::HashSet;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Protocol;

/// Maximum length for process command names from lsof/netstat output.
const MAX_COMMAND_LEN: usize = 256;

/// Information about a process listening on a port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortProcess {
    /// The port number.
    pub port: u16,
    /// The process ID.
    pub pid: u32,
    /// The command name of the process.
    pub command: String,
    /// The protocol (TCP or UDP).
    pub protocol: Protocol,
}

/// Scan for listening processes on a specific port or all ports.
/// Returns a sorted list of processes listening on the given port(s).
pub fn scan_ports(port: Option<u16>) -> Result<Vec<PortProcess>> {
    #[cfg(unix)]
    {
        let mut listeners = scan_protocol(Protocol::Tcp, port)?;
        listeners.sort_by(|a, b| {
            a.port
                .cmp(&b.port)
                .then_with(|| a.pid.cmp(&b.pid))
                .then_with(|| format!("{:?}", a.protocol).cmp(&format!("{:?}", b.protocol)))
        });
        Ok(listeners)
    }

    #[cfg(windows)]
    {
        scan_windows(port)
    }
}

/// Kill processes listening on a specific port using the given signal.
/// Returns the list of processes that were killed.
pub fn kill_processes_on_port(port: u16, signal: &str) -> Result<Vec<PortProcess>> {
    #[cfg(unix)]
    {
        let listeners = scan_ports(Some(port))?;
        if listeners.is_empty() {
            bail!("no listening process found on port {}", port);
        }

        let mut seen_pids = HashSet::new();
        for pid in listeners.iter().map(|listener| listener.pid) {
            if !seen_pids.insert(pid) {
                continue;
            }

            let status = Command::new("kill")
                .arg(format!("-{}", signal))
                .arg(pid.to_string())
                .status()
                .with_context(|| format!("failed to invoke kill for pid {}", pid))?;

            if !status.success() {
                bail!("kill returned non-zero status for pid {}", pid);
            }
        }

        Ok(listeners)
    }

    #[cfg(windows)]
    {
        let listeners = scan_ports(Some(port))?;
        if listeners.is_empty() {
            bail!("no listening process found on port {}", port);
        }

        let mut seen_pids = HashSet::new();
        for pid in listeners.iter().map(|listener| listener.pid) {
            if !seen_pids.insert(pid) {
                continue;
            }

            let mut command = Command::new("taskkill");
            command.arg("/PID").arg(pid.to_string());
            if signal.eq_ignore_ascii_case("KILL") {
                command.arg("/F");
            }
            let status = command
                .status()
                .with_context(|| format!("failed to invoke taskkill for pid {}", pid))?;
            if !status.success() {
                bail!("taskkill returned non-zero status for pid {}", pid);
            }
        }

        Ok(listeners)
    }
}

#[cfg(unix)]
fn scan_protocol(protocol: Protocol, port: Option<u16>) -> Result<Vec<PortProcess>> {
    let mut command = Command::new("lsof");
    command.arg("-nP");

    match protocol {
        Protocol::Tcp => {
            if let Some(port) = port {
                command.arg(format!("-iTCP:{}", port));
            } else {
                command.arg("-iTCP");
            }
            command.arg("-sTCP:LISTEN");
        }
        Protocol::Udp => {
            if let Some(port) = port {
                command.arg(format!("-iUDP:{}", port));
            } else {
                command.arg("-iUDP");
            }
        }
    }

    let output = command.output().context("failed to execute lsof")?;
    if output.status.success() {
        return Ok(parse_lsof_output(
            &String::from_utf8_lossy(&output.stdout),
            protocol,
        ));
    }

    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("lsof failed: {}", stderr.trim())
}

#[cfg(unix)]
fn parse_lsof_output(output: &str, protocol: Protocol) -> Vec<PortProcess> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    for line in output.lines().skip(1) {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 2 {
            continue;
        }

        let Ok(pid) = columns[1].parse::<u32>() else {
            continue;
        };
        let Some(port) = extract_port(line) else {
            continue;
        };

        let entry = PortProcess {
            port,
            pid,
            command: truncate_command(columns[0]),
            protocol,
        };

        if seen.insert((entry.port, entry.pid, format!("{:?}", entry.protocol))) {
            entries.push(entry);
        }
    }

    entries
}

#[cfg(unix)]
fn extract_port(line: &str) -> Option<u16> {
    line.split_whitespace()
        .rev()
        .find_map(extract_port_from_token)
}

fn extract_port_from_token(token: &str) -> Option<u16> {
    let token = token.trim_end_matches(')');
    let token = token.split("->").next().unwrap_or(token);
    let digits: String = token
        .chars()
        .rev()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    if digits.is_empty() {
        return None;
    }

    let prefix = &token[..token.len().saturating_sub(digits.len())];
    if !prefix.contains(':') {
        return None;
    }

    digits.parse().ok()
}

/// Truncate a command name to MAX_COMMAND_LEN, appending "…" if truncated.
fn truncate_command(s: &str) -> String {
    if s.len() <= MAX_COMMAND_LEN {
        s.to_string()
    } else {
        let mut truncated = s[..MAX_COMMAND_LEN].to_string();
        while truncated.len() > MAX_COMMAND_LEN - '…'.len_utf8() {
            truncated.pop();
        }
        truncated.push('…');
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listen_line_port() {
        let line = "Python 123 user 4u IPv4 0x123 0t0 TCP 127.0.0.1:43123 (LISTEN)";
        assert_eq!(extract_port(line), Some(43123));
    }

    #[test]
    fn extract_port_from_ipv6_listen() {
        // lsof format for IPv6: TCP [::1]:8080 (LISTEN)
        let line = "node   456 user 12u IPv6 0xabc 0t0 TCP [::1]:8080 (LISTEN)";
        assert_eq!(extract_port(line), Some(8080));
    }

    #[test]
    fn extract_port_from_wildcard_listen() {
        let line = "node   789 user 12u IPv4 0xdef 0t0 TCP *:3000 (LISTEN)";
        assert_eq!(extract_port(line), Some(3000));
    }

    #[test]
    fn extract_port_returns_none_for_no_port() {
        assert_eq!(extract_port_from_token("no-colon-here"), None);
    }

    #[test]
    fn extract_port_returns_none_for_non_numeric() {
        assert_eq!(extract_port_from_token("host:abc"), None);
    }

    #[test]
    fn extract_port_handles_arrow_notation() {
        // lsof sometimes shows "host:port->host:port"
        assert_eq!(
            extract_port_from_token("127.0.0.1:9090->10.0.0.1:443"),
            Some(9090)
        );
    }

    #[test]
    fn truncate_command_within_limit() {
        assert_eq!(truncate_command("node"), "node");
    }

    #[test]
    fn truncate_command_at_limit() {
        let exactly = "a".repeat(MAX_COMMAND_LEN);
        assert_eq!(truncate_command(&exactly), exactly);
    }

    #[test]
    fn truncate_command_over_limit() {
        let long = "b".repeat(MAX_COMMAND_LEN + 100);
        let result = truncate_command(&long);
        assert_eq!(result.len(), MAX_COMMAND_LEN);
        assert!(result.ends_with('…'));
    }
}

#[cfg(windows)]
fn scan_windows(port: Option<u16>) -> Result<Vec<PortProcess>> {
    let mut listeners = Vec::new();
    listeners.extend(parse_netstat_output(
        run_netstat("tcp")?,
        Protocol::Tcp,
        port,
    )?);
    listeners.sort_by(|a, b| {
        a.port
            .cmp(&b.port)
            .then_with(|| a.pid.cmp(&b.pid))
            .then_with(|| format!("{:?}", a.protocol).cmp(&format!("{:?}", b.protocol)))
    });
    Ok(listeners)
}

#[cfg(windows)]
fn run_netstat(protocol: &str) -> Result<String> {
    let output = Command::new("netstat")
        .args(["-ano", "-p", protocol])
        .output()
        .with_context(|| format!("failed to execute netstat for {}", protocol))?;
    if !output.status.success() {
        bail!("netstat returned non-zero status for {}", protocol);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(windows)]
fn parse_netstat_output(
    output: String,
    protocol: Protocol,
    target_port: Option<u16>,
) -> Result<Vec<PortProcess>> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    for line in output.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 4 {
            continue;
        }
        if !columns[0].eq_ignore_ascii_case(match protocol {
            Protocol::Tcp => "TCP",
            Protocol::Udp => "UDP",
        }) {
            continue;
        }

        let local = columns[1];
        let Some(port) = extract_port_from_token(local) else {
            continue;
        };
        if target_port.is_some() && target_port != Some(port) {
            continue;
        }

        let pid_column = columns.last().copied().unwrap_or_default();
        let Ok(pid) = pid_column.parse::<u32>() else {
            continue;
        };
        let command = windows_process_name(pid).unwrap_or_else(|_| format!("pid-{}", pid));
        let entry = PortProcess {
            port,
            pid,
            command: truncate_command(&command),
            protocol,
        };
        if seen.insert((entry.port, entry.pid, format!("{:?}", entry.protocol))) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

#[cfg(windows)]
fn windows_process_name(pid: u32) -> Result<String> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
        .output()
        .with_context(|| format!("failed to execute tasklist for pid {}", pid))?;
    if !output.status.success() {
        bail!("tasklist returned non-zero status for pid {}", pid);
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if line.is_empty() || line.starts_with("INFO:") {
        bail!("process not found for pid {}", pid);
    }
    Ok(line
        .trim_matches('"')
        .split("\",\"")
        .next()
        .unwrap_or_default()
        .to_string())
}
