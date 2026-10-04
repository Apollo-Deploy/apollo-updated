use crate::{
    contract::{PackageContract, validate_contract},
    disk::Store,
    error::UpdateError,
    history,
    settings::AllowedPackage,
    state::{PackageState, Phase},
    tuf_client::VerifiedPackage,
};
use anyhow::{Context, ensure};
use sha2::Digest;
use std::{fs, path::PathBuf};

pub(super) fn load_previous(
    store: &Store,
    package: &AllowedPackage,
) -> anyhow::Result<VerifiedPackage> {
    let state = store
        .load_state::<PackageState>(&package.package_id)?
        .context("package has no durable state")?;
    ensure!(
        matches!(state.phase, Phase::Committed | Phase::RolledBack)
            && state.retiring_pid.is_none()
            && state.candidate_pid.is_none()
            && state.handoff_previous_pid.is_none()
            && state.handoff_previous_version.is_none(),
        UpdateError::RecoveryRequired
    );
    let active = store.read_pointer(&package.package_id, "active")?;
    let previous = store.read_pointer(&package.package_id, "previous")?;
    ensure!(
        state.active_version == active && state.previous_version == previous,
        UpdateError::StateCorrupt
    );
    let version = previous.context(UpdateError::RollbackDenied)?;
    load_known_good_version(store, package, &version)
}

pub(super) fn load_known_good_version(
    store: &Store,
    package: &AllowedPackage,
    version: &str,
) -> anyhow::Result<VerifiedPackage> {
    let version_dir = store
        .package_dir(&package.package_id)
        .join("versions")
        .join(version);
    let metadata = fs::symlink_metadata(&version_dir)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        UpdateError::StateCorrupt
    );
    ensure!(
        fs::canonicalize(&version_dir)? == version_dir,
        UpdateError::StateCorrupt
    );

    let manifest_path = version_dir.join("manifest.json");
    ensure_regular_file(&manifest_path)?;
    let manifest = fs::read(&manifest_path)?;
    ensure!(manifest.len() <= 1024 * 1024, UpdateError::ArtifactLimit);
    let manifest_sha256 = hex::encode(sha2::Sha256::digest(&manifest));
    let contract: PackageContract = serde_json::from_slice(&manifest)?;
    validate_contract(&contract, u64::MAX)?;
    ensure!(
        contract.package_id == package.package_id
            && contract.version.to_string() == version
            && contract.architecture == package.architecture
            && contract.service_name == package.service_name,
        UpdateError::TargetMismatch
    );
    ensure!(
        package.allows_readiness(&contract.health.readiness),
        UpdateError::UnsafeManifest
    );
    if contract.listener == crate::contract::ListenerMode::None {
        ensure!(package.allow_listenerless, UpdateError::HandoffUnavailable);
    }

    let artifact_path = version_dir.join("target.part");
    ensure_regular_file(&artifact_path)?;
    let artifact_meta = fs::metadata(&artifact_path)?;
    ensure!(
        artifact_meta.len() == contract.size,
        UpdateError::TargetMismatch
    );
    let digest = crate::tuf_client::digest_file(&artifact_path)?;
    ensure!(
        digest.eq_ignore_ascii_case(&contract.sha256),
        UpdateError::TargetMismatch
    );

    let digest_path = version_dir.join("digest.sha256");
    ensure_regular_file(&digest_path)?;
    let recorded_digest = fs::read_to_string(digest_path)?;
    ensure!(
        recorded_digest.trim().eq_ignore_ascii_case(&digest),
        UpdateError::StateCorrupt
    );
    let known_good = history::read(store.root(), &package.package_id)?
        .iter()
        .any(|event| {
            event.version.as_deref() == Some(version)
                && event
                    .digest
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(&digest))
                && event
                    .manifest_sha256
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(&manifest_sha256))
                && matches!(event.operation.as_str(), "update" | "rollback")
                && matches!(
                    event.result.as_str(),
                    "committed" | "committed_cleanup_pending"
                )
        });
    ensure!(known_good, UpdateError::RollbackDenied);

    let tree = version_dir.join("payload.tree");
    ensure_directory(&tree)?;
    for entry in walkdir::WalkDir::new(&tree).follow_links(false) {
        let entry = entry?;
        ensure!(
            !fs::symlink_metadata(entry.path())?.file_type().is_symlink(),
            UpdateError::StateCorrupt
        );
    }
    let target_name = format!(
        "packages/{}/{}/{}/manifest.json",
        package.package_id, contract.version, package.architecture
    );
    Ok(VerifiedPackage {
        contract,
        manifest,
        target_name,
        artifact_path,
        artifact_sha256: digest,
        _space_reservation: None,
    })
}

fn ensure_regular_file(path: &PathBuf) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && fs::canonicalize(path)? == *path,
        UpdateError::StateCorrupt
    );
    Ok(())
}

fn ensure_directory(path: &PathBuf) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink() && fs::canonicalize(path)? == *path,
        UpdateError::StateCorrupt
    );
    Ok(())
}
