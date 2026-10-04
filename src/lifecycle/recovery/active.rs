use crate::{
    contract::ListenerMode,
    disk::Store,
    error::UpdateError,
    settings::AllowedPackage,
    state::PackageState,
    supervisor::{GenerationHandle, Supervisor, SupervisorStatus},
};
use anyhow::{Context, ensure};

use super::{append_terminal_history, handle_for_generation_id, persist};

pub(super) fn recover_active(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    mut state: PackageState,
) -> anyhow::Result<()> {
    let version = state
        .active_version
        .clone()
        .context(UpdateError::RecoveryRequired)?;
    ensure!(
        store
            .read_pointer(&package.package_id, "active")?
            .as_deref()
            == Some(version.as_str())
            && store.read_pointer(&package.package_id, "previous")? == state.previous_version,
        UpdateError::RecoveryRequired
    );
    let release =
        crate::lifecycle::operator_rollback::load_known_good_version(store, package, &version)?;
    ensure!(
        supervisor.supports_reversible_handoff(&package.service_name)?,
        UpdateError::RecoveryRequired
    );
    let active = state
        .active_generation
        .clone()
        .context(UpdateError::RecoveryRequired)?;
    ensure!(
        active.service == package.service_name
            && active.package_id == package.package_id
            && active.version == version
            && active.digest.eq_ignore_ascii_case(&release.artifact_sha256),
        UpdateError::RecoveryRequired
    );
    let previous = state.previous_generation.clone();
    ensure!(
        state.previous_version.as_ref().is_none_or(|version| {
            previous.as_ref().is_some_and(|generation| {
                generation.service == package.service_name
                    && generation.package_id == package.package_id
                    && &generation.version == version
            })
        }),
        UpdateError::RecoveryRequired
    );

    let mut status = supervisor.status(&package.service_name)?;
    let active_live =
        status.generation(&active).is_some() && supervisor.process_is_alive(&active)?;
    let previous_live = match previous.as_ref() {
        Some(generation) => {
            status.generation(generation).is_some() && supervisor.process_is_alive(generation)?
        }
        None => false,
    };
    let recovery = resolve_recovery_generation(&state, &status, &release.artifact_sha256)?;
    ensure_known(
        &status,
        [Some(&active), previous.as_ref(), recovery.as_ref()],
    )?;

    if active_live {
        crate::lifecycle::health::verify_health(supervisor, &release.contract, &active, package)?;
        let active_status = status
            .generation(&active)
            .context(UpdateError::RecoveryRequired)?;
        if previous_live {
            let old = previous.as_ref().context(UpdateError::RecoveryRequired)?;
            let old_status = status
                .generation(old)
                .context(UpdateError::RecoveryRequired)?;
            ensure!(!old_status.serving, UpdateError::RecoveryRequired);
            if status.current_main_pid == Some(active.pid) && active_status.serving {
                drain_and_wait(supervisor, old, &active, release.contract.drain_timeout_ms)?;
                supervisor.stop_generation(old)?;
            } else {
                ensure!(
                    !active_status.serving
                        && (status.current_main_pid == Some(old.pid)
                            || status.current_main_pid.is_none()),
                    UpdateError::RecoveryRequired
                );
                finish_handoff(supervisor, &release.contract, Some(old), &active, package)?;
            }
        } else {
            ensure!(
                status.current_main_pid == Some(active.pid) && active_status.serving,
                UpdateError::RecoveryRequired
            );
        }
        state.active_pid = Some(active.pid);
        state.active_generation = Some(active);
        state.recovery_generation_id = None;
        state.recovery_generation = None;
        persist(store, &mut state)?;
        return append_terminal_history(store, &state);
    }

    ensure!(
        status.current_main_pid.is_none()
            || Some(status.current_main_pid.unwrap()) == previous.as_ref().map(|value| value.pid)
            || Some(status.current_main_pid.unwrap()) == recovery.as_ref().map(|value| value.pid),
        UpdateError::RecoveryRequired
    );
    let previous_live_handle = if previous_live {
        previous.as_ref()
    } else {
        None
    };
    let recovery_id = state
        .recovery_generation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if state.recovery_generation_id.as_deref() != Some(recovery_id.as_str()) {
        state.recovery_generation_id = Some(recovery_id.clone());
        state.recovery_generation = None;
        persist(store, &mut state)?;
    }

    let candidate = if let Some(generation) = recovery.as_ref() {
        generation.clone()
    } else if let Some(generation) = handle_for_generation_id(&status, &recovery_id)? {
        validate_recovery_handle(&generation, package, &version, &release.artifact_sha256)?;
        generation
    } else {
        let listener = match release.contract.listener {
            ListenerMode::InheritedFd => Some(
                supervisor
                    .listener_source(&package.service_name)?
                    .ok_or(UpdateError::HandoffUnavailable)?,
            ),
            ListenerMode::None => {
                ensure!(package.allow_listenerless, UpdateError::HandoffUnavailable);
                None
            }
            ListenerMode::ReusePortDrain => return Err(UpdateError::HandoffUnavailable.into()),
        };
        let tree = store
            .package_dir(&package.package_id)
            .join("versions")
            .join(&version);
        supervisor.start_successor(
            &package.service_name,
            &package.package_id,
            &recovery_id,
            &tree,
            listener.as_ref(),
        )?
    };
    validate_recovery_handle(&candidate, package, &version, &release.artifact_sha256)?;
    state.recovery_generation = Some(candidate.clone());
    persist(store, &mut state)?;
    crate::lifecycle::health::verify_health(supervisor, &release.contract, &candidate, package)?;
    status = supervisor.status(&package.service_name)?;
    ensure_known(
        &status,
        [Some(&active), previous.as_ref(), Some(&candidate)],
    )?;
    finish_handoff(
        supervisor,
        &release.contract,
        previous_live_handle,
        &candidate,
        package,
    )?;
    if !active_live {
        let _ = supervisor.stop_generation(&active);
    }
    state.active_pid = Some(candidate.pid);
    state.active_generation = Some(candidate.clone());
    state.recovery_generation_id = None;
    state.recovery_generation = None;
    persist(store, &mut state)?;
    append_terminal_history(store, &state)
}

fn resolve_recovery_generation(
    state: &PackageState,
    status: &SupervisorStatus,
    digest: &str,
) -> anyhow::Result<Option<GenerationHandle>> {
    let Some(id) = state.recovery_generation_id.as_deref() else {
        ensure!(
            state.recovery_generation.is_none(),
            UpdateError::RecoveryRequired
        );
        return Ok(None);
    };
    if let Some(saved) = state.recovery_generation.as_ref() {
        ensure!(saved.generation_id == id, UpdateError::RecoveryRequired);
        if let Some(actual) = handle_for_generation_id(status, id)? {
            ensure!(actual == *saved, UpdateError::RecoveryRequired);
        }
        return Ok(Some(saved.clone()));
    }
    let Some(actual) = handle_for_generation_id(status, id)? else {
        return Ok(None);
    };
    ensure!(
        actual.package_id == state.package_id
            && state.active_version.as_deref() == Some(actual.version.as_str())
            && actual.digest.eq_ignore_ascii_case(digest),
        UpdateError::RecoveryRequired
    );
    Ok(Some(actual))
}

fn validate_recovery_handle(
    generation: &GenerationHandle,
    package: &AllowedPackage,
    version: &str,
    digest: &str,
) -> anyhow::Result<()> {
    ensure!(
        generation.service == package.service_name
            && generation.package_id == package.package_id
            && generation.version == version
            && generation.digest.eq_ignore_ascii_case(digest),
        UpdateError::RecoveryRequired
    );
    Ok(())
}

fn ensure_known<'a>(
    status: &SupervisorStatus,
    handles: impl IntoIterator<Item = Option<&'a GenerationHandle>>,
) -> anyhow::Result<()> {
    let handles: Vec<_> = handles.into_iter().flatten().collect();
    ensure!(
        status.generations.iter().all(|generation| {
            generation
                .handle(&status.service)
                .is_some_and(|handle| handles.iter().any(|known| **known == handle))
        }),
        UpdateError::RecoveryRequired
    );
    if let Some(pid) = status.current_main_pid {
        let current = status
            .handle_for_pid(pid)
            .context(UpdateError::RecoveryRequired)?;
        ensure!(
            handles.iter().any(|known| **known == current),
            UpdateError::RecoveryRequired
        );
    }
    Ok(())
}

fn finish_handoff(
    supervisor: &dyn Supervisor,
    contract: &crate::contract::PackageContract,
    previous: Option<&GenerationHandle>,
    candidate: &GenerationHandle,
    package: &AllowedPackage,
) -> anyhow::Result<()> {
    let status = supervisor.status(&candidate.service)?;
    if !status
        .generation(candidate)
        .is_some_and(|generation| generation.serving)
    {
        supervisor.activate_generation(candidate)?;
    }
    crate::lifecycle::health::verify_health(supervisor, contract, candidate, package)?;
    if let Some(previous) = previous {
        drain_and_wait(supervisor, previous, candidate, contract.drain_timeout_ms)?;
    }
    supervisor.commit_active(previous, candidate)?;
    if let Some(previous) = previous {
        supervisor.stop_generation(previous)?;
    }
    Ok(())
}

fn drain_and_wait(
    supervisor: &dyn Supervisor,
    previous: &GenerationHandle,
    candidate: &GenerationHandle,
    timeout_ms: u32,
) -> anyhow::Result<()> {
    ensure!(
        supervisor.drain_generation(previous, timeout_ms)?
            && supervisor.wait_for_drain(previous, timeout_ms, Some(candidate))?,
        UpdateError::RecoveryFailed
    );
    Ok(())
}
