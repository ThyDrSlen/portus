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
    List { project_filter: Option<String> },
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
    Allocated { lease: Lease },
    /// Lease confirmed.
    Confirmed { lease_id: String },
    /// Lease released.
    Released { lease_id: String },
    /// Heartbeat acknowledged with next deadline.
    HeartbeatAck {
        lease_id: String,
        expires_at: String,
    },
    /// List of leases.
    LeaseList { leases: Vec<Lease> },
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
    Error { code: String, message: String },
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

impl Request {
    /// Validate string field lengths in this request.
    /// Returns an error describing the first invalid field found.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Request::Allocate {
                project, service, ..
            } => {
                if service.trim().is_empty() {
                    return Err("service name must not be empty".into());
                }
                if service.len() > crate::model::MAX_SERVICE_NAME_LEN {
                    return Err(format!(
                        "service name exceeds maximum length of {} bytes",
                        crate::model::MAX_SERVICE_NAME_LEN
                    ));
                }
                if project.len() > crate::model::MAX_PROJECT_PATH_LEN {
                    return Err(format!(
                        "project path exceeds maximum length of {} bytes",
                        crate::model::MAX_PROJECT_PATH_LEN
                    ));
                }
                Ok(())
            }
            Request::Confirm {
                lease_id,
                session_token,
            }
            | Request::Release {
                lease_id,
                session_token,
            }
            | Request::Heartbeat {
                lease_id,
                session_token,
            } => {
                if lease_id.trim().is_empty() {
                    return Err("lease_id must not be empty".into());
                }
                if session_token.trim().is_empty() {
                    return Err("session_token must not be empty".into());
                }
                Ok(())
            }
            Request::List { project_filter } => {
                if let Some(filter) = project_filter {
                    if filter.len() > crate::model::MAX_PROJECT_PATH_LEN {
                        return Err(format!(
                            "project filter exceeds maximum length of {} bytes",
                            crate::model::MAX_PROJECT_PATH_LEN
                        ));
                    }
                }
                Ok(())
            }
            Request::Status | Request::Shutdown => Ok(()),
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

    #[test]
    fn validate_rejects_empty_service() {
        let req = Request::Allocate {
            project: "/tmp/app".into(),
            service: "".into(),
            preferred_port: None,
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: None,
        };
        assert!(req.validate().is_err());
        assert!(req.validate().unwrap_err().contains("empty"));
    }

    #[test]
    fn validate_rejects_whitespace_only_service() {
        let req = Request::Allocate {
            project: "/tmp/app".into(),
            service: "   ".into(),
            preferred_port: None,
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: None,
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn validate_rejects_oversized_service() {
        let req = Request::Allocate {
            project: "/tmp/app".into(),
            service: "x".repeat(crate::model::MAX_SERVICE_NAME_LEN + 1),
            preferred_port: None,
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: None,
        };
        assert!(req.validate().is_err());
        assert!(req.validate().unwrap_err().contains("maximum length"));
    }

    #[test]
    fn validate_rejects_oversized_project() {
        let req = Request::Allocate {
            project: "p".repeat(crate::model::MAX_PROJECT_PATH_LEN + 1),
            service: "web".into(),
            preferred_port: None,
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: None,
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_allocate() {
        let req = Request::Allocate {
            project: "/home/user/app".into(),
            service: "web-server".into(),
            preferred_port: Some(3000),
            protocol: Protocol::Tcp,
            auto_reassign: false,
            pid: Some(1234),
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_lease_id() {
        let req = Request::Confirm {
            lease_id: "".into(),
            session_token: "tok-123".into(),
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn validate_accepts_status_and_shutdown() {
        assert!(Request::Status.validate().is_ok());
        assert!(Request::Shutdown.validate().is_ok());
    }
}
