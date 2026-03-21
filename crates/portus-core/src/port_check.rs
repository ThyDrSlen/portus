use std::net::{SocketAddr, TcpListener, UdpSocket};

use crate::model::Protocol;

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
pub fn find_available_port(range: std::ops::RangeInclusive<u16>, protocol: Protocol) -> Option<u16> {
    range.into_iter().find(|&p| is_port_available(p, protocol))
}

/// Default port range for auto-assignment: 10000–19999
/// (avoids well-known ports and common dev tool ranges)
pub const AUTO_PORT_RANGE: std::ops::RangeInclusive<u16> = 10000..=19999;

#[cfg(test)]
mod tests {
    use super::*;

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
}
