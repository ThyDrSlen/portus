use serde::{Deserialize, Serialize};

use crate::model::{Lease, Protocol};

/// Unique message wrapper for request/response correlation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message<T> {
    pub id: String,
    pub payload: T,
}

/// Client-to-daemon requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Request a port allocation.
    Allocate {
        project: String,
        service: String,
        preferred_port: Option<u16>,
        #[serde(default)]
        protocol: Protocol,
        #[serde(default)]
        auto_reassign: bool,
        pid: Option<u32>,
    },
    /// Confirm that the client successfully bound the allocated port.
    Confirm {
        lease_id: String,
        session_token: String,
    },
    /// Release a port allocation.
    Release {
        lease_id: String,
        session_token: String,
    },
    /// Send a heartbeat to keep a lease alive.
    Heartbeat {
        lease_id: String,
        session_token: String,
    },
    /// List active leases, optionally filtered by project.
    List {
        project_filter: Option<String>,
    },
    /// Request daemon status.
    Status,
    /// Graceful daemon shutdown (used by CLI `daemon stop`).
    Shutdown,
}

/// Daemon-to-client responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Port successfully allocated.
    Allocated {
        lease: Lease,
    },
    /// Lease confirmed.
    Confirmed {
        lease_id: String,
    },
    /// Lease released.
    Released {
        lease_id: String,
    },
    /// Heartbeat acknowledged with next deadline.
    HeartbeatAck {
        lease_id: String,
        expires_at: String,
    },
    /// List of leases.
    LeaseList {
        leases: Vec<Lease>,
    },
    /// Daemon status information.
    DaemonStatus {
        pid: u32,
        uptime_secs: u64,
        active_leases: usize,
        socket_path: String,
    },
    /// Shutdown acknowledged.
    ShuttingDown,
    /// Error response.
    Error {
        code: String,
        message: String,
    },
}

impl Response {
    /// Convenience constructor for error responses.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serialization_roundtrip() {
        let req = Request::Allocate {
            project: "/home/user/myapp".into(),
            service: "web".into(),
            preferred_port: Some(3000),
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: Some(42),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Allocate {
                project,
                service,
                preferred_port,
                protocol,
                auto_reassign,
                pid,
            } => {
                assert!(!auto_reassign);
                assert_eq!(project, "/home/user/myapp");
                assert_eq!(service, "web");
                assert_eq!(preferred_port, Some(3000));
                assert_eq!(protocol, Protocol::Tcp);
                assert_eq!(pid, Some(42));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_serialization_roundtrip() {
        let resp = Response::error("port_in_use", "Port 3000 is already allocated");
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        match parsed {
            Response::Error { code, message } => {
                assert_eq!(code, "port_in_use");
                assert_eq!(message, "Port 3000 is already allocated");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn message_wrapper_roundtrip() {
        let msg = Message {
            id: "req-001".into(),
            payload: Request::Status,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message<Request> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "req-001");
        match parsed.payload {
            Request::Status => {}
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn list_request_with_no_filter() {
        let req = Request::List {
            project_filter: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"list\""));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::List { project_filter } => assert!(project_filter.is_none()),
            _ => panic!("wrong variant"),
        }
    }
}
