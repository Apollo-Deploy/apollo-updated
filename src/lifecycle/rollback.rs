use crate::{
    contract::PackageContract,
    disk::Store,
    error::UpdateError,
    history::{self, HistoryEvent},
    state::{OperationKind, PackageState, Phase},
    supervisor::{GenerationHandle, Supervisor},
};
use anyhow::Context;

pub(super) fn compensate(
    store: &Store,
    supervisor: &dyn Supervisor,
    mut state: PackageState,
    contract: &PackageContract,
    operation_id: &str,
    previous: Option<GenerationHandle>,
    candidate: GenerationHandle,
    supervisor_committed: bool,
    reason: &str,
    cause: anyhow::Error,
) -> anyhow::Result<PackageState> {
    state.phase = Phase::RollingBack;
    state.failure = Some(reason.to_owned());
    let intent_error = persist(store, &mut state).err();
    let restoration = (|| -> anyhow::Result<()> {
        if let Some(previous) = previous.as_ref() {
            // Resume the known-good generation before gating a candidate that
            // may already have accepted traffic during the overlap.
            supervisor.resume_generation(previous)?;
        }
        let candidate_gated = supervisor
            .drain_generation(&candidate, contract.drain_timeout_ms)
            .unwrap_or(false);
        let candidate_drained = candidate_gated
            && supervisor
                .wait_for_drain(&candidate, contract.drain_timeout_ms, previous.as_ref())
                .unwrap_or(false);
        if !candidate_drained {
            if supervisor.process_is_alive(&candidate)? {
                anyhow::bail!("candidate is still live and has not completed its drain");
            }
        }
        if supervisor_committed {
            if let Some(previous) = previous.as_ref() {
                supervisor.commit_active(Some(&candidate), previous)?;
            }
        }
        // A drained generation is safe to stop; an un-drained generation
        // reaches this point only when the supervisor confirmed it exited.
        supervisor
            .stop_generation(&candidate)
            .context("candidate could not be safely reconciled after compensation")?;
        if let Some(previous) = previous.as_ref() {
            if supervisor_committed {
                if let Some(version) = state.handoff_previous_version.as_deref() {
                    store.replace_pointer(&state.package_id, "active", version)?;
                } else {
                    store.clear_pointer(&state.package_id, "active")?;
                }
                match state.previous_version.as_deref() {
                    Some(version) => {
                        store.replace_pointer(&state.package_id, "previous", version)?
                    }
                    None => store.clear_pointer(&state.package_id, "previous")?,
                }
            }
            state
                .active_version
                .clone_from(&state.handoff_previous_version);
            state.active_pid = Some(previous.pid);
            state.active_generation = Some(previous.clone());
        } else if supervisor_committed {
            store.clear_pointer(&state.package_id, "active")?;
            match state.previous_version.as_deref() {
                Some(version) => store.replace_pointer(&state.package_id, "previous", version)?,
                None => store.clear_pointer(&state.package_id, "previous")?,
            }
            state.active_version = None;
            state.active_pid = None;
            state.active_generation = None;
        } else {
            state.active_version = None;
            state.active_pid = None;
            state.active_generation = None;
        }
        state.handoff_previous_version = None;
        state.handoff_previous_pid = None;
        state.handoff_previous_generation = None;
        Ok(())
    })();
    match restoration {
        Ok(()) => {
            state.phase = Phase::RolledBack;
            state.candidate_pid = None;
            state.candidate_generation_id = None;
            state.candidate_generation = None;
            state.retiring_generation = None;
            state.failure = Some(format!("{reason}: {cause:#}"));
            if let Some(error) = intent_error {
                state.failure = Some(format!(
                    "{reason}: {cause:#}; rollback intent could not be saved: {error:#}"
                ));
            }
            persist(store, &mut state).context("save completed rollback state")?;
            history::append_once(
                store.root(),
                HistoryEvent {
                    operation_id: operation_id.to_owned(),
                    package_id: state.package_id.clone(),
                    version: Some(contract.version.to_string()),
                    digest: Some(contract.sha256.clone()),
                    manifest_sha256: None,
                    operation: match state.operation_kind {
                        OperationKind::Update => "update",
                        OperationKind::Rollback => "rollback",
                    }
                    .into(),
                    result: "rolled_back".into(),
                    timestamp_unix_ms: history::now_ms(),
                },
            )?;
            Err(cause.context(std::io::Error::other(reason.to_owned())))
        }
        Err(recovery_error) => {
            state.phase = Phase::Failed;
            state.failure = Some(match intent_error {
                Some(intent_error) => format!(
                    "{reason}; recovery required: {recovery_error:#}; rollback intent save failed: {intent_error:#}"
                ),
                None => format!("{reason}; recovery required: {recovery_error:#}"),
            });
            persist(store, &mut state).context("save failed recovery state")?;
            Err(UpdateError::RecoveryFailed.into())
        }
    }
}

pub(super) fn fail<T>(
    store: &Store,
    mut state: PackageState,
    contract: &PackageContract,
    operation_id: &str,
    reason: &str,
    error: anyhow::Error,
) -> anyhow::Result<T> {
    let cleanup_error = store
        .discard_staged(&state.package_id, &contract.version.to_string())
        .err();
    state.candidate_pid = None;
    state.candidate_generation_id = None;
    state.candidate_generation = None;
    state.handoff_previous_version = None;
    state.handoff_previous_pid = None;
    state.handoff_previous_generation = None;
    state.phase = Phase::Failed;
    state.failure = Some(match cleanup_error {
        Some(cleanup_error) => {
            format!("{reason}: {error:#}; staging cleanup failed: {cleanup_error:#}")
        }
        None => format!("{reason}: {error:#}"),
    });
    persist(store, &mut state)?;
    history::append_once(
        store.root(),
        HistoryEvent {
            operation_id: operation_id.to_owned(),
            package_id: state.package_id.clone(),
            version: Some(contract.version.to_string()),
            digest: Some(contract.sha256.clone()),
            manifest_sha256: None,
            operation: match state.operation_kind {
                OperationKind::Update => "update",
                OperationKind::Rollback => "rollback",
            }
            .into(),
            result: "failed".into(),
            timestamp_unix_ms: history::now_ms(),
        },
    )?;
    Err(error.context(std::io::Error::other(reason.to_owned())))
}

pub(super) fn persist(store: &Store, state: &mut PackageState) -> anyhow::Result<()> {
    state.touch();
    store.save_json(&store.state_path(&state.package_id), state)
}

#[cfg(test)]
mod tests {
    use super::compensate;
    use crate::{
        contract::{
            ArtifactSource, Compatibility, HealthContract, ListenerMode, PackageContract, Readiness,
        },
        disk::Store,
        settings::AllowedPackage,
        state::{PackageState, Phase},
        supervisor::{
            GenerationHandle, GenerationStatus, ListenerSource, Supervisor, SupervisorStatus,
        },
    };
    use semver::Version;
    use std::{
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct UndrainedCandidate {
        stop_calls: AtomicUsize,
        resumed: AtomicUsize,
    }

    fn handle(pid: u32, version: &str) -> GenerationHandle {
        GenerationHandle {
            service: "sample".into(),
            package_id: "sample".into(),
            generation_id: format!("generation-{pid}"),
            pid,
            invocation_id: format!("invocation-{pid}"),
            version: version.into(),
            digest: "a".repeat(64),
        }
    }

    impl Supervisor for UndrainedCandidate {
        fn status(&self, service: &str) -> anyhow::Result<SupervisorStatus> {
            Ok(SupervisorStatus {
                service: service.into(),
                unit_state: "active".into(),
                current_main_pid: Some(10),
                generations: vec![GenerationStatus {
                    pid: 11,
                    version: Some("2.0.0".into()),
                    package_id: "sample".into(),
                    generation_id: "generation-11".into(),
                    invocation_id: "invocation-11".into(),
                    digest: "a".repeat(64),
                    serving: true,
                    healthy: true,
                }],
                listener_owner: None,
            })
        }

        fn supports_reversible_handoff(&self, _: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn listener_source(&self, _: &str) -> anyhow::Result<Option<ListenerSource>> {
            Ok(None)
        }

        fn process_is_alive(&self, generation: &GenerationHandle) -> anyhow::Result<bool> {
            Ok(generation.pid == 11)
        }

        fn start_successor(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &Path,
            _: Option<&ListenerSource>,
        ) -> anyhow::Result<GenerationHandle> {
            anyhow::bail!("unexpected successor start")
        }

        fn health_of_generation(&self, _: &GenerationHandle, _: u32) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn activate_generation(&self, _: &GenerationHandle) -> anyhow::Result<()> {
            Ok(())
        }

        fn drain_generation(&self, generation: &GenerationHandle, _: u32) -> anyhow::Result<bool> {
            anyhow::ensure!(generation.pid == 11, "unexpected drain target");
            Ok(true)
        }

        fn wait_for_drain(
            &self,
            generation: &GenerationHandle,
            _: u32,
            _: Option<&GenerationHandle>,
        ) -> anyhow::Result<bool> {
            anyhow::ensure!(generation.pid == 11, "unexpected wait target");
            Ok(false)
        }

        fn resume_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()> {
            anyhow::ensure!(generation.pid == 10, "unexpected resume target");
            self.resumed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn stop_generation(&self, _: &GenerationHandle) -> anyhow::Result<()> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn commit_active(
            &self,
            _: Option<&GenerationHandle>,
            _: &GenerationHandle,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn compensation_never_stops_a_live_candidate_after_drain_timeout() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(&directory.path().join("state")).expect("store");
        let supervisor = UndrainedCandidate {
            stop_calls: AtomicUsize::new(0),
            resumed: AtomicUsize::new(0),
        };
        let package = AllowedPackage {
            package_id: "sample".into(),
            service_name: "sample".into(),
            architecture: "x86_64-linux-gnu".into(),
            allow_listenerless: false,
            health_executables: Default::default(),
            health_probes: Vec::new(),
            generation: None,
        };
        let contract = PackageContract {
            package_id: package.package_id.clone(),
            version: Version::parse("2.0.0").unwrap(),
            architecture: package.architecture.clone(),
            artifact: ArtifactSource::Tuf {
                target: "packages/sample/2.0.0/x86_64-linux-gnu/payload.tar.zst".into(),
            },
            sha256: "a".repeat(64),
            size: 1,
            service_name: package.service_name.clone(),
            listener: ListenerMode::InheritedFd,
            health: HealthContract {
                readiness: Readiness::ProcessAlive,
                stabilization_ms: 0,
                failure_threshold: 1,
            },
            drain_timeout_ms: 10,
            health_timeout_ms: 10,
            stabilization_ms: 0,
            compatibility: Compatibility {
                minimum_updater: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
                maximum_updater: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
                package_format: 1,
                state_format: 1,
                protocol_version: 1,
            },
            coordinated_set: None,
        };
        let mut state = PackageState::initial(&package.package_id);
        let previous = handle(10, "1.0.0");
        let candidate = handle(11, "2.0.0");
        state.candidate_pid = Some(11);
        state.candidate_generation = Some(candidate.clone());
        state.handoff_previous_pid = Some(10);
        state.handoff_previous_version = Some("1.0.0".into());
        state.handoff_previous_generation = Some(previous.clone());

        let result = compensate(
            &store,
            &supervisor,
            state,
            &contract,
            "op-undrained",
            Some(previous),
            candidate,
            false,
            "test drain timeout",
            anyhow::anyhow!("in-flight connection remains"),
        );

        assert!(result.is_err());
        assert_eq!(supervisor.resumed.load(Ordering::SeqCst), 1);
        assert_eq!(supervisor.stop_calls.load(Ordering::SeqCst), 0);
        let persisted = store
            .load_state::<PackageState>(&package.package_id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.phase, Phase::Failed);
        assert_eq!(persisted.candidate_pid, Some(11));
    }
}
