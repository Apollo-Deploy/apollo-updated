use crate::{
    contract::{PackageContract, validate_contract},
    disk::Store,
    error::UpdateError,
    history::{self, HistoryEvent},
    settings::AllowedPackage,
    state::{OperationKind, PackageState, Phase},
    supervisor::{GenerationHandle, Supervisor, SupervisorStatus},
};
use anyhow::{Context, ensure};

#[path = "recovery/active.rs"]
mod active;
#[path = "recovery/commit.rs"]
mod commit;
#[path = "recovery/rollback.rs"]
mod rollback;
use sha2::Digest;
use std::{fs, path::Path};

pub(super) fn recover_package(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
) -> anyhow::Result<()> {
    let Some(state) = store.load_state::<PackageState>(&package.package_id)? else {
        return Ok(());
    };
    match state.phase {
        Phase::Committing => commit::recover_commit(store, supervisor, package, state),
        Phase::RollingBack
        | Phase::Verified
        | Phase::Staged
        | Phase::HandingOff
        | Phase::VerifyingHealth
        | Phase::Draining => rollback::recover_rollback(store, supervisor, package, state),
        Phase::Committed if state.retiring_pid.is_some() || state.retiring_generation.is_some() => {
            commit::finish_committed(store, supervisor, package, state)
        }
        Phase::Committed | Phase::RolledBack
            if state.recovery_generation_id.is_some() || state.recovery_generation.is_some() =>
        {
            active::recover_active(store, supervisor, package, state)
        }
        Phase::Committed | Phase::RolledBack if !has_live_generations(&state) => {
            active::recover_active(store, supervisor, package, state)
        }
        Phase::Failed if !has_live_generations(&state) => append_terminal_history(store, &state),
        Phase::Failed => rollback::recover_rollback(store, supervisor, package, state),
        Phase::Idle | Phase::Downloading if !has_live_generations(&state) => Ok(()),
        _ => Err(UpdateError::RecoveryRequired.into()),
    }
}

fn has_live_generations(state: &PackageState) -> bool {
    state.candidate_pid.is_some()
        || state.candidate_generation_id.is_some()
        || state.candidate_generation.is_some()
        || state.handoff_previous_pid.is_some()
        || state.handoff_previous_generation.is_some()
        || state.handoff_previous_version.is_some()
        || state.retiring_pid.is_some()
        || state.retiring_generation.is_some()
}

pub(super) fn load_candidate(
    store: &Store,
    package: &AllowedPackage,
    state: &PackageState,
) -> anyhow::Result<(PackageContract, String)> {
    let version = state
        .candidate_version
        .as_deref()
        .context(UpdateError::RecoveryRequired)?;
    let version_dir = store
        .package_dir(&package.package_id)
        .join("versions")
        .join(version);
    ensure_real_dir(&version_dir)?;
    let manifest_path = version_dir.join("manifest.json");
    ensure_regular_file(&manifest_path)?;
    let manifest = fs::read(&manifest_path)?;
    ensure!(manifest.len() <= 1024 * 1024, UpdateError::RecoveryRequired);
    let contract: PackageContract = serde_json::from_slice(&manifest)?;
    validate_contract(&contract, u64::MAX)?;
    ensure!(
        contract.package_id == package.package_id
            && contract.service_name == package.service_name
            && contract.architecture == package.architecture
            && contract.version.to_string() == version,
        UpdateError::RecoveryRequired
    );
    ensure!(
        package.allows_readiness(&contract.health.readiness),
        UpdateError::RecoveryRequired
    );
    let artifact = version_dir.join("target.part");
    ensure_regular_file(&artifact)?;
    let metadata = fs::metadata(&artifact)?;
    let digest = crate::tuf_client::digest_file(&artifact)?;
    ensure!(
        metadata.len() == contract.size
            && digest.eq_ignore_ascii_case(&contract.sha256)
            && state
                .candidate_digest
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(&digest)),
        UpdateError::RecoveryRequired
    );
    let digest_path = version_dir.join("digest.sha256");
    ensure_regular_file(&digest_path)?;
    ensure!(
        fs::read_to_string(digest_path)?
            .trim()
            .eq_ignore_ascii_case(&digest),
        UpdateError::RecoveryRequired
    );
    ensure_real_dir(&version_dir.join("payload.tree"))?;
    for entry in walkdir::WalkDir::new(version_dir.join("payload.tree")).follow_links(false) {
        let entry = entry?;
        ensure!(
            !fs::symlink_metadata(entry.path())?.file_type().is_symlink(),
            UpdateError::RecoveryRequired
        );
    }
    Ok((contract, hex::encode(sha2::Sha256::digest(&manifest))))
}

pub(super) fn discover_candidate(
    state: &PackageState,
    status: &SupervisorStatus,
) -> anyhow::Result<Option<GenerationHandle>> {
    ensure!(
        state.candidate_pid.is_none()
            || (state.candidate_version.is_some() && state.candidate_generation_id.is_some()),
        UpdateError::RecoveryRequired
    );
    let Some(generation_id) = state.candidate_generation_id.as_deref() else {
        ensure!(
            state.candidate_generation.is_none(),
            UpdateError::RecoveryRequired
        );
        return Ok(None);
    };
    if let Some(saved) = state.candidate_generation.as_ref() {
        ensure!(
            saved.generation_id == generation_id
                && saved.service == status.service
                && saved.package_id == state.package_id
                && Some(saved.version.as_str()) == state.candidate_version.as_deref()
                && state
                    .candidate_digest
                    .as_deref()
                    .is_some_and(|digest| saved.digest.eq_ignore_ascii_case(digest)),
            UpdateError::RecoveryRequired
        );
        if let Some(actual) = handle_for_generation_id(status, generation_id)? {
            ensure!(actual == *saved, UpdateError::RecoveryRequired);
        }
        return Ok(Some(saved.clone()));
    }
    let Some(generation) = handle_for_generation_id(status, generation_id)? else {
        return Ok(None);
    };
    ensure!(
        generation.package_id == state.package_id
            && Some(generation.version.as_str()) == state.candidate_version.as_deref()
            && state
                .candidate_digest
                .as_deref()
                .is_some_and(|digest| generation.digest.eq_ignore_ascii_case(digest))
            && state.candidate_pid.is_none_or(|pid| pid == generation.pid),
        UpdateError::RecoveryRequired
    );
    Ok(Some(generation))
}

pub(super) fn discover_previous(
    state: &PackageState,
    status: &SupervisorStatus,
) -> anyhow::Result<Option<GenerationHandle>> {
    ensure!(
        state.handoff_previous_pid.is_none() || state.handoff_previous_version.is_some(),
        UpdateError::RecoveryRequired
    );
    let Some(version) = state.handoff_previous_version.as_deref() else {
        ensure!(
            state.handoff_previous_generation.is_none() && state.handoff_previous_pid.is_none(),
            UpdateError::RecoveryRequired
        );
        return Ok(None);
    };
    let saved = state
        .handoff_previous_generation
        .as_ref()
        .context(UpdateError::RecoveryRequired)?;
    ensure!(
        saved.service == status.service
            && saved.package_id == state.package_id
            && saved.version == version
            && state
                .handoff_previous_pid
                .is_none_or(|pid| pid == saved.pid),
        UpdateError::RecoveryRequired
    );
    if let Some(actual) = handle_for_generation_id(status, &saved.generation_id)? {
        ensure!(actual == *saved, UpdateError::RecoveryRequired);
    }
    Ok(Some(saved.clone()))
}

pub(super) fn handle_for_generation_id(
    status: &SupervisorStatus,
    generation_id: &str,
) -> anyhow::Result<Option<GenerationHandle>> {
    let mut matches = status
        .generations
        .iter()
        .filter(|generation| generation.generation_id == generation_id);
    let handle = matches
        .next()
        .map(|generation| {
            generation
                .handle(&status.service)
                .context(UpdateError::RecoveryRequired)
        })
        .transpose()?;
    ensure!(matches.next().is_none(), UpdateError::RecoveryRequired);
    Ok(handle)
}

pub(super) fn ensure_known_generations(
    status: &SupervisorStatus,
    state: &PackageState,
    candidate: Option<&GenerationHandle>,
    previous: Option<&GenerationHandle>,
) -> anyhow::Result<()> {
    ensure!(
        candidate.is_none_or(|candidate| previous.is_none_or(|previous| candidate != previous)),
        UpdateError::RecoveryRequired
    );
    let known = [
        candidate,
        previous,
        state.active_generation.as_ref(),
        state.previous_generation.as_ref(),
        state.candidate_generation.as_ref(),
        state.handoff_previous_generation.as_ref(),
        state.retiring_generation.as_ref(),
        state.recovery_generation.as_ref(),
    ];
    ensure!(
        status.generations.iter().all(|generation| {
            generation
                .handle(&status.service)
                .is_some_and(|handle| known.into_iter().flatten().any(|item| *item == handle))
        }),
        UpdateError::RecoveryRequired
    );
    ensure!(
        status.current_main_pid.is_none_or(|pid| {
            status
                .handle_for_pid(pid)
                .is_some_and(|current| known.into_iter().flatten().any(|item| *item == current))
        }),
        UpdateError::RecoveryRequired
    );
    Ok(())
}

pub(super) fn replace_pointer(
    store: &Store,
    package: &str,
    pointer: &str,
    version: Option<&str>,
) -> anyhow::Result<()> {
    match version {
        Some(version) => store.replace_pointer(package, pointer, version),
        None => store.clear_pointer(package, pointer),
    }
}

pub(super) fn append_terminal_history(store: &Store, state: &PackageState) -> anyhow::Result<()> {
    let result = match state.phase {
        Phase::Committed => {
            if state.retiring_pid.is_some() {
                "committed_cleanup_pending"
            } else {
                "committed"
            }
        }
        Phase::RolledBack => "rolled_back",
        Phase::Failed => "failed",
        _ => return Ok(()),
    };
    append_history_once(
        store,
        state,
        result,
        manifest_sha256(store, state).as_deref(),
    )
}

pub(super) fn append_history_once(
    store: &Store,
    state: &PackageState,
    result: &str,
    manifest_sha256: Option<&str>,
) -> anyhow::Result<()> {
    let Some(version) = state.candidate_version.as_deref() else {
        return Ok(());
    };
    if state.operation_id == "none" {
        return Ok(());
    }
    history::append_once(
        store.root(),
        HistoryEvent {
            operation_id: state.operation_id.clone(),
            package_id: state.package_id.clone(),
            version: Some(version.to_owned()),
            digest: state.candidate_digest.clone(),
            manifest_sha256: manifest_sha256.map(str::to_owned),
            operation: match state.operation_kind {
                OperationKind::Update => "update",
                OperationKind::Rollback => "rollback",
            }
            .into(),
            result: result.to_owned(),
            timestamp_unix_ms: history::now_ms(),
        },
    )
}

pub(super) fn manifest_sha256(store: &Store, state: &PackageState) -> Option<String> {
    let version = state.candidate_version.as_deref()?;
    let path = store
        .package_dir(&state.package_id)
        .join("versions")
        .join(version)
        .join("manifest.json");
    let manifest = fs::read(path).ok()?;
    Some(hex::encode(sha2::Sha256::digest(manifest)))
}

pub(super) fn persist(store: &Store, state: &mut PackageState) -> anyhow::Result<()> {
    state.touch();
    store.save_json(&store.state_path(&state.package_id), state)
}

fn ensure_real_dir(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink() && fs::canonicalize(path)? == path,
        UpdateError::RecoveryRequired
    );
    Ok(())
}

fn ensure_regular_file(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && fs::canonicalize(path)? == path,
        UpdateError::RecoveryRequired
    );
    Ok(())
}
