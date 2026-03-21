use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// State of a port lease through its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Port allocated by daemon, client has not yet confirmed it bound successfully.
    Pending,
    /// Client confirmed it bound the port; lease is live.
    Active,
    /// Client explicitly released the lease.
    Released,
    /// Lease expired due to missed heartbeats or timeout.
    Expired,
}

/// Network protocol for the port allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

/// A port lease representing a single allocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub lease_id: String,
    pub project_path: String,
    pub service_name: String,
    pub port: u16,
    pub protocol: Protocol,
    pub state: LeaseState,
    pub client_pid: Option<u32>,
    pub session_token: String,
    pub granted_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
}

impl Lease {
    /// Create a new pending lease for the given port.
    pub fn new(
        project_path: String,
        service_name: String,
        port: u16,
        protocol: Protocol,
        client_pid: Option<u32>,
        lease_duration_secs: i64,
    ) -> Self {
        let now = Utc::now();
        Self {
            lease_id: Uuid::new_v4().to_string(),
            project_path,
            service_name,
            port,
            protocol,
            state: LeaseState::Pending,
            client_pid,
            session_token: Uuid::new_v4().to_string(),
            granted_at: now,
            confirmed_at: None,
            last_heartbeat_at: None,
            expires_at: now + chrono::Duration::seconds(lease_duration_secs),
        }
    }

    /// Check whether this lease has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }

    /// Mark the lease as confirmed (client successfully bound the port).
    pub fn confirm(&mut self) {
        let now = Utc::now();
        self.state = LeaseState::Active;
        self.confirmed_at = Some(now);
        self.last_heartbeat_at = Some(now);
        // Extend expiry on confirm
        self.expires_at = now + chrono::Duration::seconds(300);
    }

    /// Record a heartbeat, extending the lease.
    pub fn heartbeat(&mut self, extension_secs: i64) {
        let now = Utc::now();
        self.last_heartbeat_at = Some(now);
        self.expires_at = now + chrono::Duration::seconds(extension_secs);
    }

    /// Mark the lease as released.
    pub fn release(&mut self) {
        self.state = LeaseState::Released;
    }

    /// Mark the lease as expired.
    pub fn expire(&mut self) {
        self.state = LeaseState::Expired;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_lease_is_pending() {
        let lease = Lease::new(
            "/tmp/project".into(),
            "web".into(),
            3000,
            Protocol::Tcp,
            Some(1234),
            60,
        );
        assert_eq!(lease.state, LeaseState::Pending);
        assert!(!lease.is_expired());
        assert!(lease.confirmed_at.is_none());
    }

    #[test]
    fn confirm_transitions_to_active() {
        let mut lease = Lease::new(
            "/tmp/project".into(),
            "web".into(),
            3000,
            Protocol::Tcp,
            None,
            60,
        );
        lease.confirm();
        assert_eq!(lease.state, LeaseState::Active);
        assert!(lease.confirmed_at.is_some());
    }

    #[test]
    fn release_transitions_state() {
        let mut lease = Lease::new(
            "/tmp/project".into(),
            "web".into(),
            3000,
            Protocol::Tcp,
            None,
            60,
        );
        lease.confirm();
        lease.release();
        assert_eq!(lease.state, LeaseState::Released);
    }

    #[test]
    fn heartbeat_extends_expiry() {
        let mut lease = Lease::new(
            "/tmp/project".into(),
            "web".into(),
            3000,
            Protocol::Tcp,
            None,
            1, // 1 second lease
        );
        let old_expiry = lease.expires_at;
        // Small sleep not needed — heartbeat resets from now
        lease.heartbeat(300);
        assert!(lease.expires_at > old_expiry);
    }
}
