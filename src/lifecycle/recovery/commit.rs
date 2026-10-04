use crate::{
    disk::Store,
    error::UpdateError,
    settings::AllowedPackage,
    state::{PackageState, Phase},
    supervisor::Supervisor,
};
use anyhow::{Context, ensure};

use super::{
    append_history_once, append_terminal_history, discover_candidate, discover_previous,
    ensure_known_generations, load_candidate, persist, replace_pointer,
};

pub(super) fn recover_commit(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    mut state: PackageState,
) -> anyhow::Result<()> {
    ensure!(
        supervisor.supports_reversible_handoff(&package.service_name)?,
        UpdateError::RecoveryRequired
    );
    let (contract, manifest_sha256) = match load_candidate(store, package, &state) {
        Ok(candidate) => candidate,
        Err(_) => return super::rollback::recover_rollback(store, supervisor, package, state),
    };
    let status = supervisor.status(&package.service_name)?;
    let candidate = discover_candidate(&state, &status)?;
    let previous = discover_previous(&state, &status)?;
    ensure_known_generations(&status, &state, candidate.as_ref(), previous.as_ref())?;
    let Some(candidate) = candidate else {
        return super::rollback::recover_rollback(store, supervisor, package, state);
    };
    state.candidate_pid = Some(candidate.pid);
    state.candidate_generation = Some(candidate.clone());
    state.handoff_previous_pid = previous.as_ref().map(|generation| generation.pid);
    state.handoff_previous_generation = previous.clone();
    let Some(candidate_status) = status.generation(&candidate) else {
        return super::rollback::recover_rollback(store, supervisor, package, state);
    };
    let candidate_healthy = candidate_status.healthy
        && crate::lifecycle::health::verify_health(supervisor, &contract, &candidate, package)
            .is_ok();
    if !candidate_healthy {
        return super::rollback::recover_rollback(store, supervisor, package, state);
    }

    ensure!(
        status.current_main_pid.is_none_or(|pid| {
            Some(pid) == previous.as_ref().map(|generation| generation.pid) || pid == candidate.pid
        }),
        UpdateError::RecoveryRequired
    );
    // Make the verified successor the listener owner before gating its predecessor.
    if !candidate_status.serving {
        supervisor.activate_generation(&candidate)?;
    }
    crate::lifecycle::health::verify_health(supervisor, &contract, &candidate, package)?;
    if let Some(previous) = previous.as_ref() {
        if let Some(previous_status) = status.generation(previous) {
            if previous_status.serving {
                ensure!(
                    supervisor.drain_generation(previous, contract.drain_timeout_ms)?,
                    UpdateError::RecoveryRequired
                );
            }
            ensure!(
                supervisor.wait_for_drain(previous, contract.drain_timeout_ms, Some(&candidate))?,
                UpdateError::RecoveryRequired
            );
        } else {
            ensure!(
                !supervisor.process_is_alive(previous)?,
                UpdateError::RecoveryRequired
            );
        }
    }
    if status.current_main_pid != Some(candidate.pid) {
        supervisor.commit_active(previous.as_ref(), &candidate)?;
    }
    crate::lifecycle::health::verify_health(supervisor, &contract, &candidate, package)?;

    let candidate_version = state
        .candidate_version
        .clone()
        .context(UpdateError::RecoveryRequired)?;
    replace_pointer(
        store,
        &package.package_id,
        "previous",
        state.handoff_previous_version.as_deref(),
    )?;
    replace_pointer(
        store,
        &package.package_id,
        "active",
        Some(&candidate_version),
    )?;
    state
        .previous_version
        .clone_from(&state.handoff_previous_version);
    state.previous_generation.clone_from(&previous);
    state.active_version = Some(candidate_version.clone());
    state.active_pid = Some(candidate.pid);
    state.active_generation = Some(candidate.clone());
    state.retiring_pid = previous.as_ref().map(|generation| generation.pid);
    state.retiring_generation = previous.clone();
    state.candidate_pid = None;
    state.candidate_generation_id = None;
    state.candidate_generation = None;
    state.handoff_previous_version = None;
    state.handoff_previous_pid = None;
    state.handoff_previous_generation = None;
    state.phase = Phase::Committed;
    state.failure = None;
    persist(store, &mut state)?;
    finish_retirement(
        store,
        supervisor,
        package,
        &mut state,
        contract.drain_timeout_ms,
    )?;
    append_history_once(store, &state, "committed", Some(&manifest_sha256))
}

pub(super) fn finish_committed(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    mut state: PackageState,
) -> anyhow::Result<()> {
    ensure!(
        supervisor.supports_reversible_handoff(&package.service_name)?,
        UpdateError::RecoveryRequired
    );
    let status = supervisor.status(&package.service_name)?;
    let active = state
        .active_generation
        .clone()
        .context(UpdateError::RecoveryRequired)?;
    ensure!(
        active.service == package.service_name
            && active.package_id == package.package_id
            && state.active_version.as_deref() == Some(active.version.as_str())
            && status.generation(&active).is_some()
            && status.current_main_pid == Some(active.pid)
            && status
                .generation(&active)
                .is_some_and(|generation| generation.serving)
            && supervisor
                .health_of_generation(&active, 1000)
                .unwrap_or(false),
        UpdateError::RecoveryRequired
    );
    state.active_pid = Some(active.pid);
    replace_pointer(
        store,
        &package.package_id,
        "active",
        state.active_version.as_deref(),
    )?;
    replace_pointer(
        store,
        &package.package_id,
        "previous",
        state.previous_version.as_deref(),
    )?;
    let timeout_ms = load_candidate(store, package, &state)
        .map(|(contract, _)| contract.drain_timeout_ms)
        .unwrap_or(5_000);
    finish_retirement(store, supervisor, package, &mut state, timeout_ms)?;
    append_terminal_history(store, &state)
}

fn finish_retirement(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    state: &mut PackageState,
    timeout_ms: u32,
) -> anyhow::Result<()> {
    if state.retiring_pid.is_none() && state.retiring_generation.is_none() {
        return Ok(());
    }
    let retiring = state
        .retiring_generation
        .clone()
        .context(UpdateError::RecoveryRequired)?;
    let active = state
        .active_generation
        .clone()
        .context(UpdateError::RecoveryRequired)?;
    ensure!(
        state.retiring_pid.is_none_or(|pid| pid == retiring.pid)
            && state.active_pid == Some(active.pid)
            && retiring != active,
        UpdateError::RecoveryRequired
    );
    let status = supervisor.status(&package.service_name)?;
    ensure!(
        status.generation(&active).is_some()
            && status.current_main_pid == Some(active.pid)
            && status.generations.iter().all(|generation| {
                generation
                    .handle(&status.service)
                    .is_some_and(|handle| handle == active || handle == retiring)
            }),
        UpdateError::RecoveryRequired
    );
    if status.generation(&retiring).is_some() {
        if status
            .generation(&retiring)
            .is_some_and(|generation| generation.serving)
        {
            ensure!(
                supervisor.drain_generation(&retiring, timeout_ms)?,
                UpdateError::RecoveryRequired
            );
        }
        ensure!(
            supervisor.wait_for_drain(&retiring, timeout_ms, Some(&active))?,
            UpdateError::RecoveryRequired
        );
        supervisor.stop_generation(&retiring)?;
    } else {
        ensure!(
            !supervisor.process_is_alive(&retiring)?,
            UpdateError::RecoveryRequired
        );
    }
    state.retiring_pid = None;
    state.retiring_generation = None;
    persist(store, state)
}
