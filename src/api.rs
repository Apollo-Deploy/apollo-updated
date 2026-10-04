use crate::{
    audit,
    disk::Store,
    history,
    settings::Settings,
    state::PackageState,
    supervisor::{Supervisor, SystemdSupervisor},
    tuf_client,
};
use anyhow::Context;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use serde::{Deserialize, Serialize};
use std::{os::fd::AsRawFd, os::unix::net::UnixListener};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Semaphore,
    time::{Duration, timeout},
};

const MAX_REQUEST: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub command: String,
    pub package: Option<String>,
}

#[derive(Debug, Serialize)]
struct Reply {
    ok: bool,
    result: serde_json::Value,
    error: Option<String>,
}

pub async fn serve(settings: Settings, store: Store, listener: UnixListener) -> anyhow::Result<()> {
    let bound = tokio::net::UnixListener::from_std(listener)?;
    let slots = std::sync::Arc::new(Semaphore::new(64));
    loop {
        let (stream, _) = bound.accept().await?;
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let settings = settings.clone();
        let root = store.root().to_path_buf();
        tokio::spawn(async move {
            let _slot = slot;
            let _ = handle(stream, &settings, &root).await;
        });
    }
}

async fn handle(
    mut stream: UnixStream,
    settings: &Settings,
    root: &std::path::Path,
) -> anyhow::Result<()> {
    let creds = getsockopt(&stream, PeerCredentials).context("read Unix peer credentials")?;
    // Both credential options read the credentials attached to this connected socket. Capture
    // supplementary groups before awaiting client input; never resolve SO_PEERCRED's PID via procfs.
    let groups = peer_groups(&stream).ok();
    let mut buffer = vec![0_u8; MAX_REQUEST + 1];
    let mut used = 0;
    loop {
        let read = timeout(Duration::from_secs(10), stream.read(&mut buffer[used..]))
            .await
            .context("client idle timeout")??;
        if read == 0 {
            break;
        }
        used += read;
        if used > MAX_REQUEST {
            anyhow::bail!("request too large");
        }
        if buffer[..used].contains(&b'\n') {
            break;
        }
    }
    let request: Request = serde_json::from_slice(&buffer[..used])?;
    let mutation = matches!(
        request.command.as_str(),
        "update" | "update_all" | "rollback" | "gc" | "verify" | "check"
    );
    if mutation
        && creds.uid() != 0
        && creds.gid() != settings.allowed_group
        && !groups
            .as_ref()
            .is_some_and(|groups| groups.contains(&(settings.allowed_group as libc::gid_t)))
    {
        audit::write(&request.command, request.package.as_deref(), "denied");
        anyhow::bail!("mutating method requires root or the configured updater group");
    }
    let result = dispatch(request, settings, root).await;
    match result {
        Ok(value) => {
            let reply = Reply {
                ok: true,
                result: value,
                error: None,
            };
            stream.write_all(&serde_json::to_vec(&reply)?).await?;
            stream.write_all(b"\n").await?;
        }
        Err(error) => {
            audit::write("request", None, "failed");
            let reply = Reply {
                ok: false,
                result: serde_json::Value::Null,
                error: Some(format!("{error:#}")),
            };
            stream.write_all(&serde_json::to_vec(&reply)?).await?;
            stream.write_all(b"\n").await?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn peer_groups<F: AsRawFd>(socket: &F) -> std::io::Result<Vec<libc::gid_t>> {
    const SO_PEERGROUPS: libc::c_int = 59;
    const MAX_GROUPS: usize = 65_536;
    const INLINE_GROUPS: usize = 256;

    fn group_count(bytes: libc::socklen_t, maximum: usize) -> std::io::Result<usize> {
        let bytes = usize::try_from(bytes).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid peer group length")
        })?;
        if bytes % std::mem::size_of::<libc::gid_t>() != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "misaligned peer group length",
            ));
        }
        let count = bytes / std::mem::size_of::<libc::gid_t>();
        if count > maximum {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "peer group count exceeds Linux limit",
            ));
        }
        Ok(count)
    }

    let mut inline = [0 as libc::gid_t; INLINE_GROUPS];
    let mut length = std::mem::size_of_val(&inline) as libc::socklen_t;
    // SAFETY: socket is a live borrowed FD; inline is writable for `length` bytes and correctly
    // aligned for gid_t; the kernel writes no more than that capacity and updates length.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_PEERGROUPS,
            inline.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result == 0 {
        let count = group_count(length, INLINE_GROUPS)?;
        return Ok(inline[..count].to_vec());
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ERANGE) {
        return Err(error);
    }
    let count = group_count(length, MAX_GROUPS)?;
    if count <= INLINE_GROUPS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "kernel returned an inconsistent peer group length",
        ));
    }

    let mut groups = vec![0 as libc::gid_t; count];
    let mut length = std::mem::size_of_val(groups.as_slice()) as libc::socklen_t;
    // SAFETY: socket is a live borrowed FD; groups is writable for `length` bytes and aligned for
    // gid_t; its allocation is capped above before being exposed to the kernel.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_PEERGROUPS,
            groups.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let count = group_count(length, groups.len())?;
    groups.truncate(count);
    Ok(groups)
}

#[cfg(not(target_os = "linux"))]
fn peer_groups<F: AsRawFd>(_socket: &F) -> std::io::Result<Vec<libc::gid_t>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SO_PEERGROUPS is Linux-specific",
    ))
}

#[cfg(test)]
mod tests {
    use super::peer_groups;

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_groups_come_from_the_connected_socket_credentials() {
        let (peer, _server) = std::os::unix::net::UnixStream::pair().unwrap();
        let actual = peer_groups(&peer).unwrap();
        let expected = nix::unistd::getgroups()
            .unwrap()
            .into_iter()
            .map(|group| group.as_raw())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
}

async fn dispatch(
    request: Request,
    settings: &Settings,
    root: &std::path::Path,
) -> anyhow::Result<serde_json::Value> {
    let store = Store::open(root)?;
    match request.command.as_str() {
        "status" => {
            let packages = settings
                .packages
                .iter()
                .map(|package| package_diagnostics(&store, package))
                .collect::<Vec<_>>();
            Ok(serde_json::json!({"packages":packages}))
        }
        "history" => {
            let package = required_package(request.package)?;
            Ok(serde_json::to_value(history::read(root, &package)?)?)
        }
        "doctor" => {
            let supervisor = SystemdSupervisor;
            let packages = settings
                .packages
                .iter()
                .map(|package| {
                    let mut details = package_diagnostics(&store, package);
                    let status = supervisor.status(&package.service_name);
                    details["supervisor_reachable"] = serde_json::json!(status.is_ok());
                    details["listener_owner"] = serde_json::json!(
                        status.as_ref().ok().and_then(|value| value.listener_owner)
                    );
                    details["listener_ownership_confirmed"] = serde_json::json!(
                        status
                            .as_ref()
                            .is_ok_and(|value| value.listener_owner.is_some())
                    );
                    details["supervisor"] = serde_json::to_value(status.as_ref().ok())?;
                    Ok::<_, anyhow::Error>(details)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let available = fs2::available_space(root)?;
            let trust_anchor = tuf_client::inspect_trust_anchor(&settings.trusted_root_bytes);
            Ok(serde_json::json!({
                "packages":packages,
                "disk_available":available,
                "trusted_root_present":settings.trusted_root.is_file(),
                "trusted_root_expiry":trust_anchor.as_ref().ok(),
                "trusted_root_expired":trust_anchor.as_ref().ok().map(|status| status.expired),
                "trusted_root_error":trust_anchor.err().map(|error| format!("{error:#}"))
            }))
        }
        "update" | "rollback" | "update_all" => {
            if request.command == "update_all" {
                anyhow::bail!(crate::error::UpdateError::SupervisorCannotHandoff);
            }
            let package = required_package(request.package)?;
            let allow = settings.package(&package).context("unknown package id")?;
            let lock = store.lock_package(&package)?;
            let _guard = lock;
            let supervisor = SystemdSupervisor;
            crate::lifecycle::recover_package(&store, &supervisor, allow)?;
            if !supervisor.supports_reversible_handoff(&allow.service_name)? {
                anyhow::bail!(crate::error::UpdateError::SupervisorCannotHandoff);
            }
            if request.command == "rollback" {
                let state = crate::lifecycle::rollback_previous(&store, &supervisor, allow, None)?;
                return Ok(serde_json::json!({ "state": state }));
            }
            let state = store
                .load_state::<PackageState>(&package)?
                .unwrap_or_else(|| PackageState::initial(&package));
            let repository = tuf_client::load(settings, &store).await?;
            let verified =
                tuf_client::verify_target(&repository, settings, &store, allow, &state).await?;
            let state =
                crate::lifecycle::install_verified(&store, &supervisor, allow, verified, None)?;
            Ok(serde_json::json!({ "state": state }))
        }
        "check" | "verify" => {
            let package = required_package(request.package)?;
            let allow = settings.package(&package).context("unknown package id")?;
            let lock = store.lock_package(&package)?;
            let _guard = lock;
            crate::lifecycle::recover_package(&store, &SystemdSupervisor, allow)?;
            let mut state = store
                .load_state::<PackageState>(&package)?
                .unwrap_or_else(|| PackageState::initial(&package));
            let repository = tuf_client::load(settings, &store).await?;
            let verified =
                tuf_client::verify_target(&repository, settings, &store, allow, &state).await?;
            std::fs::remove_file(&verified.artifact_path)?;
            store.discard_staged(&package, &verified.contract.version.to_string())?;
            if request.command == "verify" {
                state.highest_verified_version = Some(verified.contract.version.to_string());
                state.candidate_version = Some(verified.contract.version.to_string());
                state.candidate_digest = Some(verified.artifact_sha256.clone());
                state.touch();
                store.save_json(&store.state_path(&package), &state)?;
            }
            Ok(
                serde_json::json!({"package_id":package,"version":verified.contract.version,"sha256":verified.artifact_sha256,"target":verified.target_name}),
            )
        }
        "gc" => {
            anyhow::bail!("garbage collection requires a fully configured supervisor reference set")
        }
        _ => anyhow::bail!("unknown command"),
    }
}

fn package_diagnostics(
    store: &Store,
    package: &crate::settings::AllowedPackage,
) -> serde_json::Value {
    let state = store.load_state::<PackageState>(&package.package_id);
    let active = store.read_pointer(&package.package_id, "active");
    let previous = store.read_pointer(&package.package_id, "previous");
    let state_consistent = match (&state, &active, &previous) {
        (Ok(Some(state)), Ok(active), Ok(previous)) => {
            state.active_version.as_ref() == active.as_ref()
                && state.previous_version.as_ref() == previous.as_ref()
        }
        (Ok(None), Ok(None), Ok(None)) => true,
        _ => false,
    };
    serde_json::json!({
        "package_id":package.package_id,
        "state":state.as_ref().ok().and_then(|value| value.as_ref()),
        "state_error":state.err().map(|error| format!("{error:#}")),
        "state_consistent":state_consistent,
        "active":active.as_ref().ok(),
        "active_pointer_error":active.err().map(|error| format!("{error:#}")),
        "previous":previous.as_ref().ok(),
        "previous_pointer_error":previous.err().map(|error| format!("{error:#}"))
    })
}

fn required_package(package: Option<String>) -> anyhow::Result<String> {
    let package = package.context("package id required")?;
    if package.is_empty()
        || !package
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
    {
        anyhow::bail!("invalid package id");
    }
    Ok(package)
}

pub fn activated_socket() -> anyhow::Result<UnixListener> {
    let pid = std::env::var("LISTEN_PID")?.parse::<u32>()?;
    let count = std::env::var("LISTEN_FDS")?.parse::<u32>()?;
    if pid != std::process::id() || count != 1 {
        anyhow::bail!("expected one systemd socket-activated listener");
    }
    // SAFETY: systemd passes exactly one owned listening descriptor at fd 3 after LISTEN_PID/FDS validation.
    #[allow(unsafe_code)]
    let stream = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
    stream.set_nonblocking(true)?;
    Ok(stream)
}

use std::os::fd::FromRawFd;
