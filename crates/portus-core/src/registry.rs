use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use crate::model::{Lease, LeaseState, Protocol};
use crate::port_check;

/// Default lease duration for new allocations (seconds).
const DEFAULT_LEASE_SECS: i64 = 300; // 5 minutes
/// Default heartbeat extension (seconds).
const HEARTBEAT_EXTENSION_SECS: i64 = 300;
/// Grace period for crash recovery: recently-expired Active leases get a second chance (seconds).
const GRACE_PERIOD_SECS: i64 = 60;

/// In-memory registry of port leases, backed by an optional TOML file.
#[derive(Debug)]
pub struct Registry {
    leases: HashMap<String, Lease>,
    /// None = in-memory only (tests), Some = persisted to disk.
    path: Option<PathBuf>,
}

/// Serializable wrapper for the TOML file format.
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct RegistryFile {
    #[serde(default)]
    leases: HashMap<String, Lease>,
}

impl Registry {
    /// Create a new registry backed by the given file path.
    /// Loads existing data if the file exists.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut leases = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read registry at {}", path.display()))?;
            let file: RegistryFile =
                toml::from_str(&content).with_context(|| "failed to parse registry TOML")?;
            info!(count = file.leases.len(), "loaded registry");
            file.leases
        } else {
            debug!(path = %path.display(), "no existing registry, starting fresh");
            HashMap::new()
        };

        let now = chrono::Utc::now();
        let grace = chrono::Duration::seconds(GRACE_PERIOD_SECS);
        let mut recovered = 0u32;
        let mut force_expired = 0u32;
        for lease in leases.values_mut() {
            if lease.state == LeaseState::Active && now > lease.expires_at {
                if (now - lease.expires_at) <= grace {
                    lease.state = LeaseState::Pending;
                    recovered += 1;
                } else {
                    lease.state = LeaseState::Expired;
                    force_expired += 1;
                }
            }
        }
        if recovered > 0 || force_expired > 0 {
            info!(recovered, force_expired, "crash recovery grace applied");
        }

        let reg = Self {
            leases,
            path: Some(path),
        };
        if recovered > 0 || force_expired > 0 {
            reg.save()?;
        }
        Ok(reg)
    }

    /// Create an empty in-memory registry (for testing). Does not persist to disk.
    pub fn in_memory() -> Self {
        Self {
            leases: HashMap::new(),
            path: None,
        }
    }

    /// Persist the registry to disk atomically (write tmp + rename).
    /// No-op for in-memory registries.
    pub fn save(&self) -> Result<()> {
        let path = match &self.path {
            Some(p) => p,
            None => return Ok(()), // in-memory: skip
        };

        let file = RegistryFile {
            leases: self.leases.clone(),
        };
        let content = toml::to_string_pretty(&file).context("failed to serialize registry")?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let tmp_path = path.with_extension("toml.tmp");
        let mut tmp_file = File::create(&tmp_path)
            .with_context(|| format!("failed to create temp registry {}", tmp_path.display()))?;
        tmp_file
            .write_all(content.as_bytes())
            .with_context(|| format!("failed to write temp registry {}", tmp_path.display()))?;
        tmp_file
            .sync_all()
            .with_context(|| format!("failed to sync temp registry {}", tmp_path.display()))?;
        #[cfg(unix)]
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)).with_context(
            || format!("failed to set registry permissions: {}", tmp_path.display()),
        )?;
        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to rename {} -> {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        // Sync parent directory to ensure rename is durable
        if let Some(parent) = path.parent() {
            let parent_dir = File::open(parent)
                .with_context(|| format!("failed to open parent directory {}", parent.display()))?;
            parent_dir
                .sync_all()
                .with_context(|| format!("failed to sync parent directory {}", parent.display()))?;
        }
        debug!(path = %path.display(), "registry saved");
        Ok(())
    }

    /// Allocate a port. If a preferred port is given and available, use it.
    /// Otherwise, auto-assign from the default range.
    /// Returns the new lease on success.
    pub fn allocate(
        &mut self,
        project: String,
        service: String,
        preferred_port: Option<u16>,
        protocol: Protocol,
        auto_reassign: bool,
        pid: Option<u32>,
    ) -> Result<Lease> {
        // Check if this project+service already has an active/pending lease
        if let Some(existing) = self.find_active_by_service(&project, &service) {
            bail!(
                "service '{}' in project '{}' already has an active lease on port {} (lease: {})",
                service,
                project,
                existing.port,
                existing.lease_id
            );
        }

        let port = if let Some(p) = preferred_port {
            let registry_conflict = self.find_active_by_port(p, protocol);
            let system_available = port_check::is_port_available(p, protocol);

            if registry_conflict.is_none() && system_available {
                p
            } else if auto_reassign {
                self.find_auto_port(protocol)?
            } else if let Some(holder) = registry_conflict {
                bail!(
                    "port {} is already allocated to service '{}' in '{}' (lease: {})",
                    p,
                    holder.service_name,
                    holder.project_path,
                    holder.lease_id
                );
            } else {
                bail!(
                    "port {} is in use by another process (not managed by portus)",
                    p
                );
            }
        } else {
            self.find_auto_port(protocol)?
        };

        let lease = Lease::new(project, service, port, protocol, pid, DEFAULT_LEASE_SECS);
        info!(
            lease_id = %lease.lease_id,
            port = lease.port,
            service = %lease.service_name,
            "allocated port"
        );
        self.leases.insert(lease.lease_id.clone(), lease.clone());
        self.save()?;
        Ok(lease)
    }

    fn find_auto_port(&self, protocol: Protocol) -> Result<u16> {
        let allocated_ports: HashSet<u16> = self.active_leases().map(|lease| lease.port).collect();
        let used_ports = port_check::get_used_ports();

        port_check::find_available_port_fast(
            port_check::AUTO_PORT_RANGE.clone(),
            protocol,
            &used_ports,
            &allocated_ports,
        )
        .or_else(|| port_check::find_available_port(port_check::AUTO_PORT_RANGE.clone(), protocol))
        .context("no available ports in auto-assignment range")
    }

    /// Confirm a lease (client successfully bound the port).
    pub fn confirm(&mut self, lease_id: &str, session_token: &str) -> Result<()> {
        let lease = self.leases.get_mut(lease_id).context("lease not found")?;
        if lease.session_token != session_token {
            bail!("invalid session token");
        }
        if lease.state != LeaseState::Pending {
            bail!("lease is not in pending state (current: {:?})", lease.state);
        }
        lease.confirm();
        info!(lease_id, "lease confirmed");
        self.save()
    }

    /// Release a lease.
    pub fn release(&mut self, lease_id: &str, session_token: &str) -> Result<()> {
        let lease = self.leases.get_mut(lease_id).context("lease not found")?;
        if lease.session_token != session_token {
            bail!("invalid session token");
        }
        lease.release();
        info!(lease_id, port = lease.port, "lease released");
        self.save()
    }

    /// Process a heartbeat for a lease.
    pub fn heartbeat(&mut self, lease_id: &str, session_token: &str) -> Result<String> {
        let lease = self.leases.get_mut(lease_id).context("lease not found")?;
        if lease.session_token != session_token {
            bail!("invalid session token");
        }
        if lease.state != LeaseState::Active {
            bail!("lease is not active (current: {:?})", lease.state);
        }
        lease.heartbeat(HEARTBEAT_EXTENSION_SECS);
        let expires = lease.expires_at.to_rfc3339();
        self.save()?;
        Ok(expires)
    }

    /// Expire all stale leases (pending or active past their expiry).
    /// Returns the number of leases expired.
    pub fn expire_stale(&mut self) -> Result<usize> {
        let mut expired_count = 0;
        let ids: Vec<String> = self.leases.keys().cloned().collect();
        for id in ids {
            let should_expire = {
                let lease = self
                    .leases
                    .get(&id)
                    .with_context(|| format!("lease {id} disappeared during stale expiry check"))?;
                matches!(lease.state, LeaseState::Pending | LeaseState::Active)
                    && lease.is_expired()
            };
            if should_expire {
                let lease = self
                    .leases
                    .get_mut(&id)
                    .with_context(|| format!("lease {id} disappeared before stale expiry"))?;
                warn!(
                    lease_id = %id,
                    port = lease.port,
                    service = %lease.service_name,
                    "expiring stale lease"
                );
                lease.expire();
                expired_count += 1;
            }
        }
        if expired_count > 0 {
            self.save()?;
        }
        Ok(expired_count)
    }

    /// Expire active or pending leases whose client PID is no longer alive.
    pub fn expire_dead_clients<F>(&mut self, mut pid_is_alive: F) -> Result<usize>
    where
        F: FnMut(u32) -> bool,
    {
        let mut expired_count = 0;
        let ids: Vec<String> = self.leases.keys().cloned().collect();
        for id in ids {
            let Some(lease) = self.leases.get(&id) else {
                continue;
            };
            let should_expire = matches!(lease.state, LeaseState::Pending | LeaseState::Active)
                && lease.client_pid.is_some_and(|pid| !pid_is_alive(pid));
            if should_expire {
                let lease = self
                    .leases
                    .get_mut(&id)
                    .with_context(|| format!("lease {id} disappeared before dead-client expiry"))?;
                warn!(lease_id = %id, port = lease.port, service = %lease.service_name, "expiring dead client lease");
                lease.expire();
                expired_count += 1;
            }
        }
        if expired_count > 0 {
            self.save()?;
        }
        Ok(expired_count)
    }

    /// Remove all released and expired leases (garbage collection).
    pub fn gc(&mut self) -> Result<usize> {
        let before = self.leases.len();
        self.leases
            .retain(|_, l| matches!(l.state, LeaseState::Pending | LeaseState::Active));
        let removed = before - self.leases.len();
        if removed > 0 {
            self.save()?;
        }
        Ok(removed)
    }

    /// Find an active/pending lease by port and protocol.
    pub fn find_active_by_port(&self, port: u16, protocol: Protocol) -> Option<&Lease> {
        self.active_leases()
            .find(|l| l.port == port && l.protocol == protocol)
    }

    /// Find an active/pending lease by project + service name.
    pub fn find_active_by_service(&self, project: &str, service: &str) -> Option<&Lease> {
        self.active_leases()
            .find(|l| l.project_path == project && l.service_name == service)
    }

    /// Iterator over all active/pending leases.
    pub fn active_leases(&self) -> impl Iterator<Item = &Lease> {
        self.leases
            .values()
            .filter(|l| matches!(l.state, LeaseState::Pending | LeaseState::Active))
    }

    /// List active/pending leases, optionally filtered by project path prefix.
    pub fn list(&self, project_filter: Option<&str>) -> Vec<&Lease> {
        self.active_leases()
            .filter(|l| {
                if let Some(prefix) = project_filter {
                    l.project_path.starts_with(prefix)
                } else {
                    true
                }
            })
            .collect()
    }

    /// Total active lease count.
    pub fn active_count(&self) -> usize {
        self.active_leases().count()
    }

    /// Get the file path of this registry (if persisted).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry() -> Registry {
        Registry::in_memory()
    }

    #[test]
    fn allocate_and_find() {
        let mut reg = test_registry();
        let lease = reg
            .allocate(
                "/tmp/myapp".into(),
                "web".into(),
                Some(9876),
                Protocol::Tcp,
                false,
                Some(1234),
            )
            .unwrap();
        assert_eq!(lease.port, 9876);
        assert_eq!(lease.state, LeaseState::Pending);

        let found = reg.find_active_by_port(9876, Protocol::Tcp);
        assert!(found.is_some());
        assert_eq!(found.unwrap().lease_id, lease.lease_id);
    }

    #[test]
    fn duplicate_service_rejected() {
        let mut reg = test_registry();
        reg.allocate(
            "/tmp/myapp".into(),
            "web".into(),
            Some(9877),
            Protocol::Tcp,
            false,
            None,
        )
        .unwrap();
        let result = reg.allocate(
            "/tmp/myapp".into(),
            "web".into(),
            Some(9878),
            Protocol::Tcp,
            false,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn duplicate_port_rejected() {
        let mut reg = test_registry();
        reg.allocate(
            "/tmp/app1".into(),
            "web".into(),
            Some(9879),
            Protocol::Tcp,
            false,
            None,
        )
        .unwrap();
        let result = reg.allocate(
            "/tmp/app2".into(),
            "api".into(),
            Some(9879),
            Protocol::Tcp,
            false,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn confirm_and_release_lifecycle() {
        let mut reg = test_registry();
        let lease = reg
            .allocate(
                "/tmp/myapp".into(),
                "web".into(),
                Some(9880),
                Protocol::Tcp,
                false,
                None,
            )
            .unwrap();
        let token = lease.session_token.clone();
        let id = lease.lease_id.clone();

        reg.confirm(&id, &token).unwrap();
        assert_eq!(reg.leases.get(&id).unwrap().state, LeaseState::Active);

        reg.release(&id, &token).unwrap();
        assert_eq!(reg.leases.get(&id).unwrap().state, LeaseState::Released);
    }

    #[test]
    fn wrong_token_rejected() {
        let mut reg = test_registry();
        let lease = reg
            .allocate(
                "/tmp/myapp".into(),
                "web".into(),
                Some(9881),
                Protocol::Tcp,
                false,
                None,
            )
            .unwrap();
        let result = reg.confirm(&lease.lease_id, "wrong-token");
        assert!(result.is_err());
    }

    #[test]
    fn auto_assign_port() {
        let mut reg = test_registry();
        let lease = reg
            .allocate(
                "/tmp/myapp".into(),
                "web".into(),
                None,
                Protocol::Tcp,
                false,
                None,
            )
            .unwrap();
        assert!(lease.port >= 10000 && lease.port <= 19999);
    }

    #[test]
    fn gc_removes_released() {
        let mut reg = test_registry();
        let lease = reg
            .allocate(
                "/tmp/myapp".into(),
                "web".into(),
                Some(9882),
                Protocol::Tcp,
                false,
                None,
            )
            .unwrap();
        reg.release(&lease.lease_id, &lease.session_token).unwrap();
        let removed = reg.gc().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(reg.leases.len(), 0);
    }

    #[test]
    fn list_with_filter() {
        let mut reg = test_registry();
        reg.allocate(
            "/home/user/app1".into(),
            "web".into(),
            Some(9883),
            Protocol::Tcp,
            false,
            None,
        )
        .unwrap();
        reg.allocate(
            "/home/user/app2".into(),
            "api".into(),
            Some(9884),
            Protocol::Tcp,
            false,
            None,
        )
        .unwrap();
        reg.allocate(
            "/other/path".into(),
            "db".into(),
            Some(9885),
            Protocol::Tcp,
            false,
            None,
        )
        .unwrap();

        let all = reg.list(None);
        assert_eq!(all.len(), 3);

        let filtered = reg.list(Some("/home/user"));
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn persists_every_state_change_without_tmp_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state-registry.toml");
        let tmp_path = path.with_extension("toml.tmp");
        let mut reg = Registry::load(&path).unwrap();

        let lease = reg
            .allocate(
                "/tmp/stateful".into(),
                "svc".into(),
                Some(9886),
                Protocol::Tcp,
                false,
                Some(4242),
            )
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("state = \"pending\""));
        assert!(!tmp_path.exists());

        reg.confirm(&lease.lease_id, &lease.session_token).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("state = \"active\""));
        assert!(content.contains("confirmed_at"));
        assert!(!tmp_path.exists());

        reg.heartbeat(&lease.lease_id, &lease.session_token)
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("last_heartbeat_at"));
        assert!(!tmp_path.exists());

        reg.release(&lease.lease_id, &lease.session_token).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("state = \"released\""));
        assert!(!tmp_path.exists());
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-registry.toml");

        // Create and populate
        let lease_id;
        let token;
        {
            let mut reg = Registry::load(&path).unwrap();
            let lease = reg
                .allocate(
                    "/test/project".into(),
                    "api".into(),
                    Some(7777),
                    Protocol::Tcp,
                    false,
                    None,
                )
                .unwrap();
            lease_id = lease.lease_id.clone();
            token = lease.session_token.clone();
        }

        // Reload and verify
        {
            let reg = Registry::load(&path).unwrap();
            assert_eq!(reg.active_count(), 1);
            let lease = reg.find_active_by_port(7777, Protocol::Tcp).unwrap();
            assert_eq!(lease.lease_id, lease_id);
            assert_eq!(lease.session_token, token);
            assert_eq!(lease.service_name, "api");
        }
    }

    #[test]
    fn auto_reassign_on_registry_conflict() {
        let mut reg = test_registry();
        // Allocate port 9886 for service A with auto_reassign: false
        let lease_a = reg
            .allocate(
                "/tmp/app_a".into(),
                "svc_a".into(),
                Some(9886),
                Protocol::Tcp,
                false,
                None,
            )
            .unwrap();
        assert_eq!(lease_a.port, 9886);

        // Try to allocate port 9886 for service B with auto_reassign: true
        // Should succeed with a port from the auto range (>= 10000)
        let lease_b = reg
            .allocate(
                "/tmp/app_b".into(),
                "svc_b".into(),
                Some(9886),
                Protocol::Tcp,
                true,
                None,
            )
            .unwrap();
        assert!(lease_b.port >= 10000 && lease_b.port <= 19999);
        assert_ne!(lease_b.port, 9886);
    }

    #[test]
    fn auto_reassign_false_rejects_conflict() {
        let mut reg = test_registry();
        // Allocate port 9887 for service A with auto_reassign: false
        let _lease_a = reg
            .allocate(
                "/tmp/app_a".into(),
                "svc_a".into(),
                Some(9887),
                Protocol::Tcp,
                false,
                None,
            )
            .unwrap();

        // Try to allocate port 9887 for service B with auto_reassign: false
        // Should fail with "already allocated" error
        let result = reg.allocate(
            "/tmp/app_b".into(),
            "svc_b".into(),
            Some(9887),
            Protocol::Tcp,
            false,
            None,
        );
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("already allocated"));
    }

    #[test]
    fn recovery_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-registry.toml");

        let lease_id;
        {
            let mut reg = Registry::load(&path).unwrap();
            let lease = reg
                .allocate(
                    "/test/project".into(),
                    "web".into(),
                    Some(8080),
                    Protocol::Tcp,
                    false,
                    None,
                )
                .unwrap();
            reg.confirm(&lease.lease_id, &lease.session_token).unwrap();
            lease_id = lease.lease_id.clone();

            let entry = reg.leases.get_mut(&lease_id).unwrap();
            entry.expires_at = chrono::Utc::now() - chrono::Duration::seconds(30);
            reg.save().unwrap();
        }

        let reg = Registry::load(&path).unwrap();
        let lease = reg.leases.get(&lease_id).unwrap();
        assert_eq!(lease.state, LeaseState::Pending);
    }

    #[test]
    fn recovery_beyond_grace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-registry.toml");

        let lease_id;
        {
            let mut reg = Registry::load(&path).unwrap();
            let lease = reg
                .allocate(
                    "/test/project".into(),
                    "api".into(),
                    Some(9090),
                    Protocol::Tcp,
                    false,
                    None,
                )
                .unwrap();
            reg.confirm(&lease.lease_id, &lease.session_token).unwrap();
            lease_id = lease.lease_id.clone();

            let entry = reg.leases.get_mut(&lease_id).unwrap();
            entry.expires_at = chrono::Utc::now() - chrono::Duration::seconds(120);
            reg.save().unwrap();
        }

        let reg = Registry::load(&path).unwrap();
        let lease = reg.leases.get(&lease_id).unwrap();
        assert_eq!(lease.state, LeaseState::Expired);
    }
}
