use super::{FIXTURE_TEST_LOCK, activate_old, allow_package, connect, verified_fixture};
use apollo_updated::{
    disk::Store,
    history,
    lifecycle::recover_packages,
    state::{PackageState, Phase},
    supervisor::{
        GenerationHandle, ListenerSource, Supervisor, fixture::InProcessFixtureSupervisor,
    },
};
use std::path::Path;

#[path = "recovery/active_boot.rs"]
mod active_boot;
#[path = "recovery/reboot_edge_cases.rs"]
mod reboot_edge_cases;

pub(super) struct UnhealthyPidSupervisor<'a> {
    pub(super) inner: &'a InProcessFixtureSupervisor,
    pub(super) unhealthy_pid: u32,
    pub(super) stop_calls: std::sync::atomic::AtomicUsize,
    pub(super) drain_notice: Option<std::sync::mpsc::Sender<(u32, bool)>>,
}

impl Supervisor for UnhealthyPidSupervisor<'_> {
    fn status(
        &self,
        service: &str,
    ) -> anyhow::Result<apollo_updated::supervisor::SupervisorStatus> {
        self.inner.status(service)
    }

    fn supports_reversible_handoff(&self, service: &str) -> anyhow::Result<bool> {
        self.inner.supports_reversible_handoff(service)
    }

    fn listener_source(&self, service: &str) -> anyhow::Result<Option<ListenerSource>> {
        self.inner.listener_source(service)
    }

    fn process_is_alive(&self, generation: &GenerationHandle) -> anyhow::Result<bool> {
        self.inner.process_is_alive(generation)
    }

    fn start_successor(
        &self,
        service: &str,
        package_id: &str,
        generation_id: &str,
        tree: &Path,
        listener: Option<&ListenerSource>,
    ) -> anyhow::Result<GenerationHandle> {
        Supervisor::start_successor(
            self.inner,
            service,
            package_id,
            generation_id,
            tree,
            listener,
        )
    }

    fn health_of_generation(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool> {
        if generation.pid == self.unhealthy_pid {
            Ok(false)
        } else {
            self.inner.health_of_pid(generation.pid, timeout_ms)
        }
    }

    fn activate_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()> {
        Supervisor::activate_generation(self.inner, generation)
    }

    fn drain_generation(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool> {
        let drained = Supervisor::drain_generation(self.inner, generation, timeout_ms)?;
        if let Some(sender) = &self.drain_notice {
            let _ = sender.send((generation.pid, drained));
        }
        Ok(drained)
    }

    fn wait_for_drain(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
        must_remain_healthy: Option<&GenerationHandle>,
    ) -> anyhow::Result<bool> {
        Supervisor::wait_for_drain(self.inner, generation, timeout_ms, must_remain_healthy)
    }

    fn resume_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()> {
        Supervisor::resume_generation(self.inner, generation)
    }

    fn stop_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()> {
        self.stop_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Supervisor::stop_generation(self.inner, generation)
    }

    fn commit_active(
        &self,
        previous: Option<&GenerationHandle>,
        candidate: &GenerationHandle,
    ) -> anyhow::Result<()> {
        Supervisor::commit_active(self.inner, previous, candidate)
    }
}

fn prepare_candidate(
    store: &Store,
    supervisor: &InProcessFixtureSupervisor,
    old_pid: u32,
) -> (u32, PackageState) {
    let verified = verified_fixture(store, "2.0.0", None);
    let version = verified.contract.version.to_string();
    let digest = verified.artifact_sha256.clone();
    let tree = store
        .promote(
            "fixture",
            &version,
            &verified.artifact_path,
            &digest,
            &verified.manifest,
        )
        .unwrap();
    let candidate_pid = supervisor
        .start_successor(
            "fixture",
            &tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    let mut state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    state.operation_id = uuid::Uuid::new_v4().to_string();
    state.candidate_version = Some(version);
    state.candidate_digest = Some(digest);
    let candidate = supervisor.generation_handle(candidate_pid).unwrap();
    state.candidate_generation_id = Some(candidate.generation_id.clone());
    state.candidate_generation = Some(candidate);
    state.highest_verified_version = Some("2.0.0".into());
    state.handoff_previous_version = Some("1.0.0".into());
    state.handoff_previous_pid = Some(old_pid);
    state.handoff_previous_generation = Some(supervisor.generation_handle(old_pid).unwrap());
    state.candidate_pid = Some(candidate_pid);
    (candidate_pid, state)
}

fn persist(store: &Store, state: &PackageState) {
    store
        .save_json(&store.state_path("fixture"), state)
        .unwrap();
}

fn recover(store: &Store, supervisor: &InProcessFixtureSupervisor) {
    recover_packages(store, supervisor, &[allow_package()]).unwrap();
}

#[test]
fn interrupted_drain_resumes_previous_generation_and_removes_candidate() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = super::supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);
    let (candidate_pid, mut state) = prepare_candidate(&store, &supervisor, old_pid);
    state.phase = Phase::Draining;
    persist(&store, &state);

    assert!(supervisor.drain_pid(old_pid, 5000).unwrap());
    assert!(
        supervisor
            .wait_for_drain(old_pid, 5000, Some(candidate_pid))
            .unwrap()
    );
    recover(&store, &supervisor);

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(old_pid));
    assert_eq!(status.generations.len(), 1);
    assert!(status.generations[0].serving && status.generations[0].healthy);
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    assert_eq!(store.read_pointer("fixture", "previous").unwrap(), None);
    let durable = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(durable.phase, Phase::RolledBack);
    assert_eq!(durable.active_pid, Some(old_pid));
    assert_eq!(durable.candidate_pid, None);
    connect(address, "1.0.0").finish("1.0.0");
    let history_after_first_recovery = history::read(&root, "fixture").unwrap();
    assert_eq!(
        history_after_first_recovery.last().unwrap().result,
        "rolled_back"
    );

    recover(&store, &supervisor);
    assert_eq!(
        history::read(&root, "fixture").unwrap().len(),
        history_after_first_recovery.len()
    );
}

#[test]
fn committing_restart_repairs_partial_pointers_and_replays_history_once() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = super::supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);
    let (candidate_pid, mut state) = prepare_candidate(&store, &supervisor, old_pid);
    state.phase = Phase::Draining;
    persist(&store, &state);

    assert!(supervisor.drain_pid(old_pid, 5000).unwrap());
    assert!(
        supervisor
            .wait_for_drain(old_pid, 5000, Some(candidate_pid))
            .unwrap()
    );
    state.phase = Phase::Committing;
    persist(&store, &state);
    supervisor.activate_pid(candidate_pid).unwrap();
    supervisor
        .commit_active(Some(old_pid), candidate_pid)
        .unwrap();
    store
        .replace_pointer("fixture", "previous", "1.0.0")
        .unwrap();
    store.replace_pointer("fixture", "active", "2.0.0").unwrap();

    recover(&store, &supervisor);

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(candidate_pid));
    assert_eq!(status.generations.len(), 1);
    assert!(status.generations[0].serving && status.generations[0].healthy);
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("2.0.0")
    );
    assert_eq!(
        store
            .read_pointer("fixture", "previous")
            .unwrap()
            .as_deref(),
        Some("1.0.0")
    );
    let durable = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(durable.phase, Phase::Committed);
    assert_eq!(durable.active_pid, Some(candidate_pid));
    assert_eq!(durable.active_version.as_deref(), Some("2.0.0"));
    assert_eq!(durable.previous_version.as_deref(), Some("1.0.0"));
    connect(address, "2.0.0").finish("2.0.0");
    let history_after_first_recovery = history::read(&root, "fixture").unwrap();
    assert_eq!(
        history_after_first_recovery.last().unwrap().result,
        "committed"
    );

    recover(&store, &supervisor);
    assert_eq!(
        history::read(&root, "fixture").unwrap().len(),
        history_after_first_recovery.len()
    );
}

#[test]
fn interrupted_rollback_restores_old_acceptance_before_draining_candidate() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = super::supervisor();
    let supervisor = std::sync::Arc::new(supervisor);
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, supervisor.as_ref());
    let (candidate_pid, mut state) = prepare_candidate(&store, supervisor.as_ref(), old_pid);
    state.phase = Phase::Draining;
    persist(&store, &state);

    assert!(supervisor.drain_pid(old_pid, 5000).unwrap());
    assert!(
        supervisor
            .wait_for_drain(old_pid, 5000, Some(candidate_pid))
            .unwrap()
    );
    state.phase = Phase::RollingBack;
    persist(&store, &state);
    supervisor.activate_pid(candidate_pid).unwrap();
    supervisor
        .commit_active(Some(old_pid), candidate_pid)
        .unwrap();
    let in_flight_candidate = connect(address, "2.0.0");

    let recovery_store = Store::open(&root).unwrap();
    let recovery_supervisor = std::sync::Arc::clone(&supervisor);
    let recovery = std::thread::spawn(move || {
        recover_packages(
            &recovery_store,
            recovery_supervisor.as_ref(),
            &[allow_package()],
        )
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = supervisor.status("fixture").unwrap();
        let old_accepting = status
            .generations
            .iter()
            .any(|generation| generation.pid == old_pid && generation.serving);
        let candidate_gated = status
            .generations
            .iter()
            .any(|generation| generation.pid == candidate_pid && !generation.serving);
        if old_accepting && candidate_gated {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "recovery did not restore old acceptance before draining candidate: {status:?}; state={:?}",
            store.load_state::<PackageState>("fixture").unwrap()
        );
        std::thread::yield_now();
    }
    let connection_during_recovery = connect(address, "1.0.0");
    in_flight_candidate.finish("2.0.0");
    recovery.join().unwrap().unwrap();
    connection_during_recovery.finish("1.0.0");

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(old_pid));
    assert_eq!(status.generations.len(), 1);
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    let durable = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(durable.phase, Phase::RolledBack);
    assert_eq!(durable.active_pid, Some(old_pid));
}

#[test]
fn recovery_does_not_stop_a_live_candidate_when_its_drain_times_out() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = super::supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);
    let (candidate_pid, mut state) = prepare_candidate(&store, &supervisor, old_pid);
    state.phase = Phase::Draining;
    persist(&store, &state);

    assert!(supervisor.drain_pid(old_pid, 5000).unwrap());
    assert!(
        supervisor
            .wait_for_drain(old_pid, 5000, Some(candidate_pid))
            .unwrap()
    );
    supervisor.activate_pid(candidate_pid).unwrap();
    supervisor
        .commit_active(Some(old_pid), candidate_pid)
        .unwrap();
    let in_flight = connect(address, "2.0.0");
    state.phase = Phase::RollingBack;
    persist(&store, &state);

    let probe = UnhealthyPidSupervisor {
        inner: &supervisor,
        unhealthy_pid: candidate_pid,
        stop_calls: std::sync::atomic::AtomicUsize::new(0),
        drain_notice: None,
    };
    assert!(
        recover_packages(&store, &probe, &[allow_package()]).is_err(),
        "recovery must remain pending while candidate work is in flight"
    );
    assert_eq!(
        probe.stop_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "recovery must not stop a live candidate whose drain did not finish"
    );
    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(candidate_pid));
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    assert!(status.generations.iter().any(|generation| {
        generation.pid == old_pid && generation.serving && generation.healthy
    }));
    assert!(
        status
            .generations
            .iter()
            .any(|generation| { generation.pid == candidate_pid && !generation.serving })
    );
    assert_eq!(
        store
            .load_state::<PackageState>("fixture")
            .unwrap()
            .unwrap()
            .phase,
        Phase::RollingBack
    );
    connect(address, "1.0.0").finish("1.0.0");
    in_flight.finish("2.0.0");
    assert!(
        supervisor
            .wait_for_drain(candidate_pid, 1000, Some(old_pid))
            .unwrap()
    );
    supervisor.stop_pid(candidate_pid).unwrap();
}
