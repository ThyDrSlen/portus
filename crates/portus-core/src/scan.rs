use std::collections::HashSet;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Protocol;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortProcess {
    pub port: u16,
    pub pid: u32,
    pub command: String,
    pub protocol: Protocol,
}

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
        return Ok(parse_lsof_output(&String::from_utf8_lossy(&output.stdout), protocol));
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
            command: columns[0].to_string(),
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
        .find_map(|token| extract_port_from_token(token))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listen_line_port() {
        let line = "Python 123 user 4u IPv4 0x123 0t0 TCP 127.0.0.1:43123 (LISTEN)";
        assert_eq!(extract_port(line), Some(43123));
    }

}

#[cfg(windows)]
fn scan_windows(port: Option<u16>) -> Result<Vec<PortProcess>> {
    let mut listeners = Vec::new();
    listeners.extend(parse_netstat_output(run_netstat("tcp")?, Protocol::Tcp, port)?);
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
fn parse_netstat_output(output: String, protocol: Protocol, target_port: Option<u16>) -> Result<Vec<PortProcess>> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    for line in output.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 4 {
            continue;
        }
        if !columns[0].eq_ignore_ascii_case(match protocol { Protocol::Tcp => "TCP", Protocol::Udp => "UDP" }) {
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
        let entry = PortProcess { port, pid, command, protocol };
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
    Ok(line.trim_matches('"').split("\",\"").next().unwrap_or_default().to_string())
}
