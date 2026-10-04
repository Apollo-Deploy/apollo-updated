use crate::{
    disk::Store,
    error::UpdateError,
    settings::AllowedPackage,
    state::{PackageState, Phase},
    supervisor::{GenerationHandle, Supervisor, SupervisorStatus},
};
use anyhow::ensure;

#[path = "rollback/restore.rs"]
mod restore;
use restore::restore_retained_previous;

use super::{
    append_terminal_history, discover_candidate, discover_previous, ensure_known_generations,
    load_candidate, persist, replace_pointer,
};

pub(super) fn recover_rollback(
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
    let candidate = discover_candidate(&state, &status)?;
    let previous = discover_previous(&state, &status)?;
    ensure_known_generations(&status, &state, candidate.as_ref(), previous.as_ref())?;

    state.candidate_generation = candidate.clone();
    state.candidate_pid = candidate.as_ref().map(|generation| generation.pid);
    state.handoff_previous_generation = previous.clone();
    state.handoff_previous_pid = previous.as_ref().map(|generation| generation.pid);
    state.phase = Phase::RollingBack;
    state.failure = Some("interrupted update is being rolled back".into());
    persist(store, &mut state)?;
    let timeout_ms = load_candidate(store, package, &state)
        .map(|(contract, _)| contract.drain_timeout_ms)
        .unwrap_or(5_000);
    let retiring = state.retiring_generation.clone();
    ensure!(
        state.retiring_pid.is_none_or(|pid| {
            retiring
                .as_ref()
                .is_some_and(|generation| generation.pid == pid)
        }),
        UpdateError::RecoveryRequired
    );

    let target_version = state.handoff_previous_version.clone();
    let previous_live = match previous.as_ref() {
        Some(generation) => supervisor.process_is_alive(generation)?,
        None => false,
    };
    let previous_healthy = if previous_live {
        let version = target_version
            .as_deref()
            .ok_or(UpdateError::RecoveryRequired)?;
        ensure!(
            previous
                .as_ref()
                .is_some_and(|generation| generation.version == version),
            UpdateError::RecoveryRequired
        );
        let retained =
            crate::lifecycle::operator_rollback::load_known_good_version(store, package, version)?;
        crate::lifecycle::health::verify_health(
            supervisor,
            &retained.contract,
            previous.as_ref().expect("checked above"),
            package,
        )
        .is_ok()
    } else {
        false
    };

    if target_version.is_some() && !previous_live {
        let candidate_healthy = match candidate.as_ref() {
            Some(candidate) => {
                candidate_is_healthy(store, supervisor, package, &state, &status, candidate)?
            }
            None => false,
        };
        if candidate_healthy {
            state.phase = Phase::Committing;
            state.failure = Some(
                "the retained generation is unavailable; finalizing the healthy verified candidate"
                    .into(),
            );
            persist(store, &mut state)?;
            return super::commit::recover_commit(store, supervisor, package, state);
        }
        return restore_retained_previous(
            store, supervisor, package, state, status, candidate, None,
        );
    }

    ensure!(
        target_version.is_none() || previous.is_some(),
        UpdateError::RecoveryRequired
    );
    if let Some(previous) = previous.as_ref() {
        ensure!(previous_live, UpdateError::RecoveryRequired);
        if !previous_healthy {
            return restore_retained_previous(
                store,
                supervisor,
                package,
                state,
                status,
                candidate,
                Some(previous.clone()),
            );
        }
        // Restore known-good traffic before gating a possibly active candidate.
        supervisor.resume_generation(previous)?;
    } else {
        ensure!(
            status.current_main_pid.is_none(),
            UpdateError::RecoveryRequired
        );
    }

    if let Some(candidate) = candidate.as_ref()
        && supervisor.process_is_alive(candidate)?
    {
        ensure!(
            supervisor.drain_generation(candidate, timeout_ms)?,
            UpdateError::RecoveryRequired
        );
        let drained = supervisor.wait_for_drain(candidate, timeout_ms, previous.as_ref())?;
        if !drained {
            return Err(UpdateError::RecoveryRequired.into());
        }
    }
    let retiring_to_stop = if let Some(retiring) = retiring.as_ref()
        && Some(retiring) != previous.as_ref()
        && Some(retiring) != candidate.as_ref()
        && supervisor.process_is_alive(retiring)?
    {
        ensure!(
            supervisor.drain_generation(retiring, timeout_ms)?,
            UpdateError::RecoveryRequired
        );
        ensure!(
            supervisor.wait_for_drain(retiring, timeout_ms, previous.as_ref())?,
            UpdateError::RecoveryRequired
        );
        Some(retiring.clone())
    } else {
        None
    };
    if let Some(previous) = previous.as_ref()
        && previous_live
        && status.current_main_pid != Some(previous.pid)
    {
        let current = current_generation(&status)?;
        ensure!(
            current.as_ref().is_none_or(|current| {
                Some(current) == candidate.as_ref()
                    || Some(current) == state.retiring_generation.as_ref()
            }),
            UpdateError::RecoveryRequired
        );
        supervisor.commit_active(current.as_ref(), previous)?;
    }
    if let Some(candidate) = candidate.as_ref()
        && supervisor.process_is_alive(candidate)?
    {
        supervisor.stop_generation(candidate)?;
    }
    if let Some(retiring) = retiring_to_stop {
        supervisor.stop_generation(&retiring)?;
    }

    replace_pointer(
        store,
        &package.package_id,
        "active",
        target_version.as_deref(),
    )?;
    replace_pointer(
        store,
        &package.package_id,
        "previous",
        state.previous_version.as_deref(),
    )?;
    state.active_version = target_version;
    state.active_pid = previous.as_ref().map(|generation| generation.pid);
    state.active_generation = previous.clone();
    state.candidate_pid = None;
    state.candidate_generation = None;
    state.candidate_generation_id = None;
    state.retiring_pid = None;
    state.retiring_generation = None;
    state.handoff_previous_version = None;
    state.handoff_previous_pid = None;
    state.handoff_previous_generation = None;
    state.phase = Phase::RolledBack;
    state.failure = Some("interrupted lifecycle transaction was restored".into());
    persist(store, &mut state)?;
    append_terminal_history(store, &state)
}

fn current_generation(status: &SupervisorStatus) -> anyhow::Result<Option<GenerationHandle>> {
    status
        .current_main_pid
        .map(|pid| {
            status
                .handle_for_pid(pid)
                .ok_or(UpdateError::RecoveryRequired.into())
        })
        .transpose()
}

fn candidate_is_healthy(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    state: &PackageState,
    status: &SupervisorStatus,
    candidate: &GenerationHandle,
) -> anyhow::Result<bool> {
    if !supervisor.process_is_alive(candidate)?
        || status
            .generation(candidate)
            .is_none_or(|generation| !generation.healthy)
    {
        return Ok(false);
    }
    let Some(version) = state.candidate_version.as_deref() else {
        return Ok(false);
    };
    if candidate.package_id != package.package_id || candidate.version != version {
        return Ok(false);
    }
    let (contract, _) = match load_candidate(store, package, state) {
        Ok(contract) => contract,
        Err(_) => return Ok(false),
    };
    Ok(crate::lifecycle::health::verify_health(supervisor, &contract, candidate, package).is_ok())
}
