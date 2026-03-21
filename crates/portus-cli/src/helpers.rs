use anyhow::Result;
use portus_core::model::{Lease, LeaseState};
use portus_core::paths;
use portus_core::registry::Registry;

/// Load all active (Pending or Active) leases from the registry.
pub fn load_active_leases() -> Result<Vec<Lease>> {
    let registry_path = paths::registry_path()?;
    if !registry_path.exists() {
        return Ok(Vec::new());
    }

    let registry = Registry::load(&registry_path)?;
    let mut leases: Vec<Lease> = registry
        .list(None)
        .into_iter()
        .filter(|lease| matches!(lease.state, LeaseState::Pending | LeaseState::Active))
        .cloned()
        .collect();
    leases.sort_by(|a, b| {
        a.port
            .cmp(&b.port)
            .then_with(|| a.service_name.cmp(&b.service_name))
    });
    Ok(leases)
}
