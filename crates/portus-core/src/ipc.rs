use std::path::Path;

use anyhow::{Context, Result};
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
#[cfg(windows)]
use interprocess::os::windows::{local_socket::ListenerOptionsExt as _, security_descriptor::SecurityDescriptor};
#[cfg(windows)]
use widestring::U16CString;
use interprocess::local_socket::{tokio::{Listener, Stream}, ConnectOptions, ListenerOptions};

#[cfg(unix)]
fn socket_name(path: &Path) -> Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::prelude::*;

    path.to_fs_name::<GenericFilePath>()
        .context("failed to convert socket path to file-path local socket name")
        .map(|name| name.into_owned())
}

#[cfg(windows)]
fn socket_name(path: &Path) -> Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::prelude::*;

    path.display()
        .to_string()
        .to_ns_name::<GenericNamespaced>()
        .context("failed to convert pipe path to namespaced local socket name")
        .map(|name| name.into_owned())
}

/// Bind a local socket listener at the given path.
pub fn bind(path: &Path) -> Result<Listener> {
    let name = socket_name(path)?;
    let options = ListenerOptions::new().name(name);
    #[cfg(windows)]
    let options = {
        let sddl = U16CString::from_str("D:P(A;;GA;;;OW)")
            .context("failed to build pipe security descriptor string")?;
        let sd = SecurityDescriptor::deserialize(sddl.as_ucstr())
            .context("failed to deserialize pipe security descriptor")?;
        options.security_descriptor(sd)
    };
    options
        .create_tokio()
        .context("failed to bind local socket listener")
}

/// Connect to a local socket listener at the given path.
pub async fn connect(path: &Path) -> Result<Stream> {
    let name = socket_name(path)?;
    ConnectOptions::new()
        .name(name)
        .connect_tokio()
        .await
        .context("failed to connect to local socket")
}
