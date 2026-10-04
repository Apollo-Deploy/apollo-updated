use crate::{
    contract::ListenerMode,
    disk::Store,
    error::UpdateError,
    settings::AllowedPackage,
    state::{PackageState, Phase},
    supervisor::{GenerationHandle, Supervisor, SupervisorStatus},
};
use anyhow::{Context, ensure};

use super::{super::ensure_known_generations, super::load_candidate, candidate_is_healthy};
use crate::lifecycle::recovery::persist;

pub(super) fn restore_retained_previous(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    mut state: PackageState,
    mut status: SupervisorStatus,
    candidate: Option<GenerationHandle>,
    mut previous: Option<GenerationHandle>,
) -> anyhow::Result<()> {
    let version = state
        .handoff_previous_version
        .clone()
        .context(UpdateError::RecoveryRequired)?;
    let retained =
        crate::lifecycle::operator_rollback::load_known_good_version(store, package, &version)?;
    let timeout_ms = retained.contract.drain_timeout_ms;
    let candidate_timeout_ms = load_candidate(store, package, &state)
        .map(|(contract, _)| contract.drain_timeout_ms)
        .unwrap_or(timeout_ms);
    let listener = match retained.contract.listener {
        ListenerMode::InheritedFd => Some(
            supervisor
                .listener_source(&package.service_name)?
                .ok_or(UpdateError::RecoveryRequired)?,
        ),
        ListenerMode::None => {
            ensure!(package.allow_listenerless, UpdateError::RecoveryRequired);
            None
        }
        ListenerMode::ReusePortDrain => return Err(UpdateError::RecoveryRequired.into()),
    };
    ensure_current_is_known(&status, [&candidate, &previous, &state.retiring_generation])?;

    let recovery_id = state
        .recovery_generation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if state.recovery_generation_id.as_deref() != Some(recovery_id.as_str()) {
        state.recovery_generation_id = Some(recovery_id.clone());
        state.recovery_generation = None;
        persist(store, &mut state)?;
    }
    let mut restored = resolve_recovery_generation(&state, &status, &recovery_id)?;
    if let Some(generation) = restored.as_ref()
        && !supervisor.process_is_alive(generation)?
    {
        restored = None;
        state.recovery_generation = None;
        persist(store, &mut state)?;
    }

    if restored.is_none() && candidate.is_some() && previous.is_some() {
        let candidate_handle = candidate.as_ref().ok_or(UpdateError::RecoveryRequired)?;
        let previous_handle = previous.as_ref().ok_or(UpdateError::RecoveryRequired)?;
        ensure!(
            candidate_is_healthy(
                store,
                supervisor,
                package,
                &state,
                &status,
                candidate_handle,
            )?,
            UpdateError::RecoveryRequired
        );
        let needs_commit = status.current_main_pid != Some(candidate_handle.pid);
        let current = if needs_commit {
            let current = current_generation(&status)?.or_else(|| Some(previous_handle.clone()));
            ensure!(
                current.is_none() || current.as_ref() == Some(previous_handle),
                UpdateError::RecoveryRequired
            );
            if !status
                .generation(candidate_handle)
                .is_some_and(|generation| generation.serving)
            {
                supervisor.activate_generation(candidate_handle)?;
            }
            current
        } else {
            None
        };
        if supervisor.process_is_alive(previous_handle)? {
            ensure!(
                supervisor.drain_generation(previous_handle, timeout_ms)?,
                UpdateError::RecoveryRequired
            );
            ensure!(
                supervisor.wait_for_drain(previous_handle, timeout_ms, Some(candidate_handle),)?,
                UpdateError::RecoveryRequired
            );
        }
        if let Some(current) = current.as_ref() {
            supervisor.commit_active(Some(current), candidate_handle)?;
        }
        if supervisor.process_is_alive(previous_handle)? {
            supervisor.stop_generation(previous_handle)?;
        }
        previous = None;
    }

    let restored = if let Some(restored) = restored {
        restored
    } else {
        let tree = store
            .package_dir(&package.package_id)
            .join("versions")
            .join(&version);
        let restored = supervisor.start_successor(
            &package.service_name,
            &package.package_id,
            &recovery_id,
            &tree,
            listener.as_ref(),
        )?;
        ensure!(
            restored.generation_id == recovery_id
                && restored.service == package.service_name
                && restored.package_id == package.package_id
                && restored.version == version
                && restored
                    .digest
                    .eq_ignore_ascii_case(&retained.artifact_sha256),
            UpdateError::RecoveryRequired
        );
        state.recovery_generation = Some(restored.clone());
        persist(store, &mut state)?;
        restored
    };
    state.recovery_generation = Some(restored.clone());
    persist(store, &mut state)?;
    crate::lifecycle::health::verify_health(supervisor, &retained.contract, &restored, package)?;

    status = supervisor.status(&package.service_name)?;
    ensure_known_generations(&status, &state, candidate.as_ref(), previous.as_ref())?;
    let current = current_generation(&status)?;
    ensure!(
        current.as_ref().is_none_or(|current| {
            current == &restored
                || Some(current) == candidate.as_ref()
                || Some(current) == previous.as_ref()
        }),
        UpdateError::RecoveryRequired
    );
    let needs_commit = status.current_main_pid != Some(restored.pid);
    if needs_commit {
        if !status
            .generation(&restored)
            .is_some_and(|generation| generation.serving)
        {
            supervisor.activate_generation(&restored)?;
        }
        crate::lifecycle::health::verify_health(
            supervisor,
            &retained.contract,
            &restored,
            package,
        )?;
    }
    crate::lifecycle::health::verify_health(supervisor, &retained.contract, &restored, package)?;

    let mut gated_generations = Vec::new();
    for old in [candidate.as_ref(), previous.as_ref()]
        .into_iter()
        .flatten()
    {
        if old == &restored || !supervisor.process_is_alive(old)? {
            continue;
        }
        let old_timeout = if Some(old) == candidate.as_ref() {
            candidate_timeout_ms
        } else {
            timeout_ms
        };
        ensure!(
            supervisor.drain_generation(old, old_timeout)?,
            UpdateError::RecoveryRequired
        );
        gated_generations.push((old.clone(), old_timeout));
    }
    if let Some(retiring) = state.retiring_generation.as_ref()
        && Some(retiring) != candidate.as_ref()
        && Some(retiring) != previous.as_ref()
        && retiring != &restored
        && supervisor.process_is_alive(retiring)?
    {
        ensure!(
            supervisor.drain_generation(retiring, timeout_ms)?,
            UpdateError::RecoveryRequired
        );
        gated_generations.push((retiring.clone(), timeout_ms));
    }
    for (old, old_timeout) in &gated_generations {
        ensure!(
            supervisor.wait_for_drain(old, *old_timeout, Some(&restored))?,
            UpdateError::RecoveryRequired
        );
    }
    if needs_commit {
        supervisor.commit_active(current.as_ref(), &restored)?;
    }
    crate::lifecycle::recovery::replace_pointer(
        store,
        &package.package_id,
        "active",
        Some(&version),
    )?;
    state.active_version = Some(version.clone());
    state.active_pid = Some(restored.pid);
    state.active_generation = Some(restored.clone());
    persist(store, &mut state)?;
    for (old, _) in gated_generations {
        supervisor.stop_generation(&old)?;
    }

    crate::lifecycle::recovery::replace_pointer(
        store,
        &package.package_id,
        "active",
        Some(&version),
    )?;
    crate::lifecycle::recovery::replace_pointer(
        store,
        &package.package_id,
        "previous",
        state.previous_version.as_deref(),
    )?;
    state.active_version = Some(version);
    state.active_pid = Some(restored.pid);
    state.active_generation = Some(restored);
    state.previous_generation = None;
    state.candidate_pid = None;
    state.candidate_generation_id = None;
    state.candidate_generation = None;
    state.handoff_previous_pid = None;
    state.handoff_previous_generation = None;
    state.handoff_previous_version = None;
    state.retiring_pid = None;
    state.retiring_generation = None;
    state.recovery_generation_id = None;
    state.recovery_generation = None;
    state.phase = Phase::RolledBack;
    state.failure = Some("interrupted lifecycle transaction restored the retained release".into());
    persist(store, &mut state)?;
    crate::lifecycle::recovery::append_terminal_history(store, &state)
}

fn resolve_recovery_generation(
    state: &PackageState,
    status: &SupervisorStatus,
    generation_id: &str,
) -> anyhow::Result<Option<GenerationHandle>> {
    if let Some(saved) = state.recovery_generation.as_ref() {
        ensure!(
            state.recovery_generation_id.as_deref() == Some(generation_id)
                && saved.generation_id == generation_id
                && saved.service == status.service
                && saved.package_id == state.package_id,
            UpdateError::RecoveryRequired
        );
        if let Some(actual) =
            crate::lifecycle::recovery::handle_for_generation_id(status, generation_id)?
        {
            ensure!(actual == *saved, UpdateError::RecoveryRequired);
        }
        return Ok(Some(saved.clone()));
    }
    let Some(actual) = crate::lifecycle::recovery::handle_for_generation_id(status, generation_id)?
    else {
        return Ok(None);
    };
    Ok(Some(actual))
}

fn current_generation(status: &SupervisorStatus) -> anyhow::Result<Option<GenerationHandle>> {
    status
        .current_main_pid
        .map(|pid| {
            status
                .handle_for_pid(pid)
                .ok_or_else(|| anyhow::anyhow!(UpdateError::RecoveryRequired))
        })
        .transpose()
}

fn ensure_current_is_known<'a>(
    status: &SupervisorStatus,
    known: impl IntoIterator<Item = &'a Option<GenerationHandle>>,
) -> anyhow::Result<()> {
    let Some(current) = current_generation(status)? else {
        return Ok(());
    };
    ensure!(
        known
            .into_iter()
            .any(|entry| entry.as_ref() == Some(&current)),
        UpdateError::RecoveryRequired
    );
    Ok(())
}
