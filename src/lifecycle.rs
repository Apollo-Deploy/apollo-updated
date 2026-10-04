use crate::{
    disk::Store, settings::AllowedPackage, state::PackageState, supervisor::Supervisor,
    tuf_client::VerifiedPackage,
};
use std::os::fd::RawFd;

#[path = "lifecycle/commit_history.rs"]
mod commit_history;
#[path = "lifecycle/health.rs"]
mod health;
#[path = "lifecycle/install.rs"]
mod install;
#[path = "lifecycle/operator_rollback.rs"]
mod operator_rollback;
#[path = "lifecycle/recovery.rs"]
mod recovery;
#[path = "lifecycle/rollback.rs"]
mod rollback;

/// Reconcile any interrupted lifecycle transaction before package operations or service startup.
pub fn recover_package(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
) -> anyhow::Result<()> {
    recovery::recover_package(store, supervisor, package)
}

/// Recover configured packages in a stable order while holding their normal mutation locks.
pub fn recover_packages(
    store: &Store,
    supervisor: &dyn Supervisor,
    packages: &[AllowedPackage],
) -> anyhow::Result<()> {
    for package in packages {
        let _lock = store.lock_package(&package.package_id)?;
        recover_package(store, supervisor, package)?;
    }
    Ok(())
}

/// Promotes one TUF-verified release and commits it only after overlap handoff succeeds.
pub fn install_verified(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    verified: VerifiedPackage,
    listener_fd: Option<RawFd>,
) -> anyhow::Result<PackageState> {
    install::install_verified_with_policy(
        store,
        supervisor,
        package,
        verified,
        listener_fd,
        false,
        "update",
    )
}

/// Reinstalls the retained, locally recorded previous release through the same overlap handoff.
pub fn rollback_previous(
    store: &Store,
    supervisor: &dyn Supervisor,
    package: &AllowedPackage,
    listener_fd: Option<RawFd>,
) -> anyhow::Result<PackageState> {
    let verified = operator_rollback::load_previous(store, package)?;
    install::install_verified_with_policy(
        store,
        supervisor,
        package,
        verified,
        listener_fd,
        true,
        "rollback",
    )
}
