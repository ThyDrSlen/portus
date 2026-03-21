use anyhow::{Context, Result};
use std::path::PathBuf;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Base configuration directory: ~/.config/portus/
pub fn config_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("cannot determine home directory")?;
    Ok(base.home_dir().join(".config").join("portus"))
}

/// Path to the registry file: ~/.config/portus/registry.toml
pub fn registry_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("registry.toml"))
}

/// Path to the daemon socket/pipe name marker: ~/.config/portus/portus.sock
#[cfg(unix)]
pub fn socket_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("portus.sock"))
}

/// Path to the daemon pipe name marker: ~/.config/portus/portus.pipe
#[cfg(windows)]
pub fn socket_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("portus.pipe"))
}

/// Path to the daemon PID file: ~/.config/portus/portus.pid
pub fn pid_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("portus.pid"))
}

/// Ensure the config directory exists.
pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config dir: {}", dir.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to set config dir permissions: {}", dir.display()))?;
    Ok(dir)
}
