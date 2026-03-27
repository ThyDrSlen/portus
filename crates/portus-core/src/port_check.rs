use std::collections::HashSet;
use std::net::{SocketAddr, TcpListener, UdpSocket};

use crate::model::Protocol;

pub fn get_used_ports() -> HashSet<u16> {
    crate::scan::scan_ports(None)
        .map(|ports| ports.into_iter().map(|entry| entry.port).collect())
        .unwrap_or_default()
}

/// Check if a port is available for binding on localhost.
pub fn is_port_available(port: u16, protocol: Protocol) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match protocol {
        Protocol::Tcp => TcpListener::bind(addr).is_ok(),
        Protocol::Udp => UdpSocket::bind(addr).is_ok(),
    }
}

/// Find an available port in the given range.
/// Returns the first available port, or None if all are in use.
pub fn find_available_port(
    range: std::ops::RangeInclusive<u16>,
    protocol: Protocol,
) -> Option<u16> {
    range.into_iter().find(|&p| is_port_available(p, protocol))
}

pub fn find_available_port_fast(
    range: std::ops::RangeInclusive<u16>,
    protocol: Protocol,
    used_ports: &HashSet<u16>,
    allocated_ports: &HashSet<u16>,
) -> Option<u16> {
    range.into_iter().find(|&port| {
        !used_ports.contains(&port)
            && !allocated_ports.contains(&port)
            && is_port_available(port, protocol)
    })
}

/// Default port range for auto-assignment: 10000–19999
/// (avoids well-known ports and common dev tool ranges)
pub const AUTO_PORT_RANGE: std::ops::RangeInclusive<u16> = 10000..=19999;

#[cfg(test)]
mod tests {
    use super::*;

    fn find_consecutive_free_ports(count: usize) -> Vec<u16> {
        let mut ports = Vec::with_capacity(count);

        for start in 40000..=u16::MAX - count as u16 {
            let candidate: Vec<u16> = (start..start + count as u16).collect();
            if candidate
                .iter()
                .all(|&port| is_port_available(port, Protocol::Tcp))
            {
                ports = candidate;
                break;
            }
        }

        assert_eq!(ports.len(), count);
        ports
    }

    #[test]
    fn find_available_port_returns_some() {
        // There should be at least one free port in a wide range
        let port = find_available_port(40000..=40100, Protocol::Tcp);
        assert!(port.is_some());
    }

    #[test]
    fn occupied_port_detected() {
        // Bind a port, then verify is_port_available reports it as taken
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_port_available(port, Protocol::Tcp));
        drop(listener);
        // After dropping, it should be available again
        assert!(is_port_available(port, Protocol::Tcp));
    }

    #[test]
    fn fast_scan_skips_used_ports() {
        let ports = find_consecutive_free_ports(3);
        let used_ports = HashSet::from([ports[0], ports[1]]);
        let allocated_ports = HashSet::new();

        let port = find_available_port_fast(
            ports[0]..=ports[2],
            Protocol::Tcp,
            &used_ports,
            &allocated_ports,
        );

        assert_eq!(port, Some(ports[2]));
    }

    #[test]
    fn fast_scan_skips_allocated_ports() {
        let ports = find_consecutive_free_ports(3);
        let used_ports = HashSet::new();
        let allocated_ports = HashSet::from([ports[0], ports[1]]);

        let port = find_available_port_fast(
            ports[0]..=ports[2],
            Protocol::Tcp,
            &used_ports,
            &allocated_ports,
        );

        assert_eq!(port, Some(ports[2]));
    }

    #[test]
    fn fast_scan_falls_back_on_bind_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken_port = listener.local_addr().unwrap().port();
        let fallback_port = if taken_port == u16::MAX {
            taken_port - 1
        } else {
            taken_port + 1
        };
        let used_ports = HashSet::new();
        let allocated_ports = HashSet::new();

        let port = find_available_port_fast(
            taken_port..=fallback_port,
            Protocol::Tcp,
            &used_ports,
            &allocated_ports,
        );

        assert_eq!(port, Some(fallback_port));
    }
}
