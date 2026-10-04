use crate::{
    contract::{ListenerMode, validate_contract},
    disk::Store,
    error::UpdateError,
    settings::AllowedPackage,
    state::{OperationKind, PackageState, Phase},
    supervisor::Supervisor,
    tuf_client::{self, VerifiedPackage},
};
use anyhow::{Context, ensure};
use std::{fs, os::fd::RawFd};

use super::rollback::{compensate, fail, persist};
use super::{commit_history, health};

pub(super) fn install_verified_with_policy(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    verified: VerifiedPackage,
    listener_fd: Option<RawFd>,
    use_retained_tree: bool,
    operation: &str,
) -> anyhow::Result<PackageState> {
    let contract = &verified.contract;
    let version = contract.version.to_string();
    validate_contract(contract, u64::MAX)?;
    ensure!(
        contract.package_id == package.package_id
            && contract.service_name == package.service_name
            && contract.architecture == package.architecture,
        "verified contract no longer matches the local package allowlist"
    );
    ensure!(
        package.allows_readiness(&contract.health.readiness),
        UpdateError::UnsafeManifest
    );
    let _fallback_space_reservation = if !use_retained_tree && verified._space_reservation.is_none()
    {
        Some(store.reserve_download_space(
            &package.package_id,
            contract.size,
            8 * 1024 * 1024 * 1024,
        )?)
    } else {
        None
    };
    if contract.listener == ListenerMode::ReusePortDrain {
        return Err(UpdateError::HandoffUnavailable.into());
    }
    let listener = match contract.listener {
        ListenerMode::InheritedFd => Some(match listener_fd {
            Some(fd) => crate::supervisor::ListenerSource::InheritedFd { fd },
            None => supervisor
                .listener_source(&package.service_name)?
                .ok_or(UpdateError::HandoffUnavailable)?,
        }),
        ListenerMode::None => {
            ensure!(
                package.allow_listenerless
                    && package
                        .generation
                        .as_ref()
                        .is_none_or(|policy| policy.socket_unit.is_none()),
                UpdateError::HandoffUnavailable
            );
            None
        }
        ListenerMode::ReusePortDrain => return Err(UpdateError::HandoffUnavailable.into()),
    };
    if !supervisor.supports_reversible_handoff(&package.service_name)? {
        return Err(UpdateError::SupervisorCannotHandoff.into());
    }
    ensure!(
        verified
            .artifact_sha256
            .eq_ignore_ascii_case(&contract.sha256)
            && tuf_client::digest_file(&verified.artifact_path)?
                .eq_ignore_ascii_case(&contract.sha256)
            && fs::metadata(&verified.artifact_path)?.len() == contract.size,
        "verified artifact changed before promotion"
    );

    let mut state = store
        .load_state::<PackageState>(&package.package_id)?
        .unwrap_or_else(|| PackageState::initial(&package.package_id));
    state.operation_kind = if operation == "rollback" {
        OperationKind::Rollback
    } else {
        OperationKind::Update
    };
    let terminal_phase = matches!(
        state.phase,
        Phase::Idle | Phase::Committed | Phase::RolledBack
    ) || (state.phase == Phase::Failed
        && state.candidate_pid.is_none()
        && state.handoff_previous_pid.is_none()
        && state.handoff_previous_version.is_none());
    ensure!(
        terminal_phase && state.retiring_pid.is_none(),
        UpdateError::RecoveryRequired
    );
    let candidate_version = &contract.version;
    if use_retained_tree {
        ensure!(
            state.previous_version.as_deref() == Some(version.as_str()),
            UpdateError::RollbackDenied
        );
    } else if let Some(highest) = state.highest_verified_version.as_deref() {
        ensure!(
            candidate_version >= &semver::Version::parse(highest)?,
            UpdateError::RollbackDenied
        );
    }
    let active_pointer = store.read_pointer(&package.package_id, "active")?;
    let previous_pointer = store.read_pointer(&package.package_id, "previous")?;
    ensure!(
        state.active_version == active_pointer && state.previous_version == previous_pointer,
        "package pointers and durable state disagree"
    );
    let status = supervisor.status(&package.service_name)?;
    ensure!(
        status.current_main_pid == state.active_pid,
        "serving PID and durable package state disagree"
    );
    let previous_version = active_pointer;
    let previous_generation = match status.current_main_pid {
        Some(pid) => {
            let generation = status
                .handle_for_pid(pid)
                .context(UpdateError::StateCorrupt)?;
            let saved = state
                .active_generation
                .as_ref()
                .context(UpdateError::StateCorrupt)?;
            ensure!(
                generation.package_id == package.package_id
                    && Some(generation.version.as_str()) == previous_version.as_deref()
                    && saved == &generation,
                UpdateError::StateCorrupt
            );
            Some(generation)
        }
        None => {
            ensure!(state.active_generation.is_none(), UpdateError::StateCorrupt);
            None
        }
    };
    let previous_pid = previous_generation
        .as_ref()
        .map(|generation| generation.pid);
    if previous_generation.is_some() != previous_version.is_some() {
        return Err(UpdateError::StateCorrupt.into());
    }

    let operation_id = uuid::Uuid::new_v4().to_string();
    state.operation_id.clone_from(&operation_id);
    state.phase = Phase::Verified;
    state.candidate_version = Some(version.clone());
    state.candidate_digest = Some(verified.artifact_sha256.clone());
    if !use_retained_tree {
        state.highest_verified_version = Some(
            state
                .highest_verified_version
                .as_deref()
                .map(semver::Version::parse)
                .transpose()?
                .filter(|highest| highest > candidate_version)
                .map_or_else(|| version.clone(), |highest| highest.to_string()),
        );
    }
    state.handoff_previous_version.clone_from(&previous_version);
    state.handoff_previous_pid = previous_pid;
    state
        .handoff_previous_generation
        .clone_from(&previous_generation);
    state.active_generation.clone_from(&previous_generation);
    state.previous_generation.clone_from(&previous_generation);
    let candidate_generation_id = uuid::Uuid::new_v4().to_string();
    state.candidate_generation_id = Some(candidate_generation_id.clone());
    state.candidate_generation = None;
    state.candidate_pid = None;
    state.failure = None;
    persist(store, &mut state)?;

    let candidate_tree = if use_retained_tree {
        store
            .package_dir(&package.package_id)
            .join("versions")
            .join(&version)
    } else {
        match store.promote(
            &package.package_id,
            &version,
            &verified.artifact_path,
            &verified.artifact_sha256,
            &verified.manifest,
        ) {
            Ok(tree) => tree,
            Err(error) => {
                return fail(
                    store,
                    state,
                    contract,
                    &operation_id,
                    "promotion failed",
                    error,
                );
            }
        }
    };
    state.phase = Phase::Staged;
    if let Err(error) = persist(store, &mut state) {
        return fail(
            store,
            state,
            contract,
            &operation_id,
            "staged state could not be saved",
            error,
        );
    }

    // This phase is durable before the successor becomes an external process.
    state.phase = Phase::HandingOff;
    if let Err(error) = persist(store, &mut state) {
        return fail(
            store,
            state,
            contract,
            &operation_id,
            "handoff intent could not be saved",
            error,
        );
    }
    let candidate_generation = match supervisor.start_successor(
        &package.service_name,
        &package.package_id,
        &candidate_generation_id,
        &candidate_tree,
        listener.as_ref(),
    ) {
        Ok(generation) => generation,
        Err(error) => {
            return fail(
                store,
                state,
                contract,
                &operation_id,
                "successor failed to start",
                error,
            );
        }
    };
    let candidate_pid = candidate_generation.pid;
    state.candidate_pid = Some(candidate_pid);
    state.candidate_generation = Some(candidate_generation.clone());
    state.phase = Phase::VerifyingHealth;
    if let Err(error) = persist(store, &mut state) {
        return compensate(
            store,
            supervisor,
            state,
            contract,
            &operation_id,
            previous_generation.clone(),
            candidate_generation.clone(),
            false,
            "successor PID could not be saved",
            error,
        );
    }

    if let Err(error) = health::verify_health(supervisor, contract, &candidate_generation, package)
    {
        return compensate(
            store,
            supervisor,
            state,
            contract,
            &operation_id,
            previous_generation.clone(),
            candidate_generation.clone(),
            false,
            "successor failed health checks",
            error,
        );
    }
    // The successor takes the listener before the predecessor is gated. Both
    // generations can accept during this brief overlap; the predecessor remains
    // available until the successor passes its post-activation health contract.
    if let Err(error) = supervisor.activate_generation(&candidate_generation) {
        return compensate(
            store,
            supervisor,
            state,
            contract,
            &operation_id,
            previous_generation.clone(),
            candidate_generation.clone(),
            false,
            "successor activation failed",
            error,
        );
    }
    if let Err(error) = health::verify_health(supervisor, contract, &candidate_generation, package)
    {
        return compensate(
            store,
            supervisor,
            state,
            contract,
            &operation_id,
            previous_generation.clone(),
            candidate_generation.clone(),
            false,
            "successor failed health after activation",
            error,
        );
    }
    state.phase = Phase::Draining;
    if let Err(error) = persist(store, &mut state) {
        return compensate(
            store,
            supervisor,
            state,
            contract,
            &operation_id,
            previous_generation.clone(),
            candidate_generation.clone(),
            false,
            "drain intent could not be saved",
            error,
        );
    }
    if let Some(previous) = previous_generation.as_ref() {
        let drained = supervisor
            .drain_generation(previous, contract.drain_timeout_ms)
            .and_then(|gated| {
                if gated {
                    supervisor.wait_for_drain(
                        previous,
                        contract.drain_timeout_ms,
                        Some(&candidate_generation),
                    )
                } else {
                    Ok(false)
                }
            });
        let drained = match drained {
            Ok(drained) => drained,
            Err(error) => {
                return compensate(
                    store,
                    supervisor,
                    state,
                    contract,
                    &operation_id,
                    previous_generation.clone(),
                    candidate_generation.clone(),
                    false,
                    "predecessor drain failed",
                    error,
                );
            }
        };
        if !drained {
            return compensate(
                store,
                supervisor,
                state,
                contract,
                &operation_id,
                previous_generation.clone(),
                candidate_generation.clone(),
                false,
                "predecessor drain timed out",
                UpdateError::RecoveryFailed.into(),
            );
        }
    }
    state.phase = Phase::Committing;
    if let Err(error) = persist(store, &mut state) {
        return compensate(
            store,
            supervisor,
            state,
            contract,
            &operation_id,
            previous_generation.clone(),
            candidate_generation.clone(),
            false,
            "commit intent could not be saved",
            error,
        );
    }
    if let Err(error) =
        supervisor.commit_active(previous_generation.as_ref(), &candidate_generation)
    {
        return compensate(
            store,
            supervisor,
            state,
            contract,
            &operation_id,
            previous_generation.clone(),
            candidate_generation.clone(),
            false,
            "supervisor refused active-generation commit",
            error,
        );
    }

    let commit = (|| {
        if let Some(version) = previous_version.as_deref() {
            store.replace_pointer(&package.package_id, "previous", version)?;
        }
        store.replace_pointer(&package.package_id, "active", &version)?;
        let mut committed = state.clone();
        committed.previous_version.clone_from(&previous_version);
        committed.active_version = Some(version.clone());
        committed.active_pid = Some(candidate_pid);
        committed.retiring_pid = previous_pid;
        committed.active_generation = Some(candidate_generation.clone());
        committed
            .retiring_generation
            .clone_from(&previous_generation);
        committed.handoff_previous_version = None;
        committed.handoff_previous_pid = None;
        committed.handoff_previous_generation = None;
        committed.candidate_pid = None;
        committed.candidate_generation_id = None;
        committed.candidate_generation = None;
        committed.phase = Phase::Committed;
        committed.failure = None;
        persist(store, &mut committed)?;
        state = committed;
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = commit {
        return compensate(
            store,
            supervisor,
            state,
            contract,
            &operation_id,
            previous_generation.clone(),
            candidate_generation.clone(),
            true,
            "active pointer commit failed",
            error,
        );
    }
    let cleanup_pending = if let Some(previous) = previous_generation.as_ref() {
        if supervisor.stop_generation(previous).is_err() {
            true
        } else {
            let mut finalized = state.clone();
            finalized.retiring_pid = None;
            finalized.retiring_generation = None;
            if persist(store, &mut finalized).is_ok() {
                state = finalized;
                false
            } else {
                true
            }
        }
    } else {
        false
    };
    commit_history::append(
        store,
        operation_id,
        &package.package_id,
        version,
        verified.artifact_sha256,
        &verified.manifest,
        operation,
        cleanup_pending,
    )?;
    Ok(state)
}
