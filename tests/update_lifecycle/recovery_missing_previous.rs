use super::recovery::UnhealthyPidSupervisor;
use super::{
    FIXTURE_TEST_LOCK, activate_old, allow_package, connect, supervisor, verified_fixture,
};
use apollo_updated::{
    disk::Store,
    lifecycle::recover_packages,
    state::{PackageState, Phase},
    supervisor::Supervisor,
};

#[test]
fn recovery_restarts_retained_previous_when_candidate_is_unhealthy_and_old_pid_is_gone() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);

    let verified = verified_fixture(&store, "2.0.0", Some(".fixture-unhealthy"));
    let candidate_version = verified.contract.version.to_string();
    let candidate_digest = verified.artifact_sha256.clone();
    let candidate_tree = store
        .promote(
            "fixture",
            &candidate_version,
            &verified.artifact_path,
            &candidate_digest,
            &verified.manifest,
        )
        .unwrap();
    let candidate_pid = supervisor
        .start_successor(
            "fixture",
            &candidate_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();

    let mut state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    state.operation_id = uuid::Uuid::new_v4().to_string();
    state.phase = Phase::RollingBack;
    state.candidate_version = Some(candidate_version);
    state.candidate_digest = Some(candidate_digest);
    state.handoff_previous_version = Some("1.0.0".into());
    state.handoff_previous_pid = None;
    state.handoff_previous_generation = Some(supervisor.generation_handle(old_pid).unwrap());
    state.candidate_pid = Some(candidate_pid);
    let candidate = supervisor.generation_handle(candidate_pid).unwrap();
    state.candidate_generation_id = Some(candidate.generation_id.clone());
    state.candidate_generation = Some(candidate);
    store
        .save_json(&store.state_path("fixture"), &state)
        .unwrap();

    assert!(supervisor.drain_pid(old_pid, 5000).unwrap());
    assert!(supervisor.wait_for_drain(old_pid, 5000, None).unwrap());
    supervisor.stop_pid(old_pid).unwrap();

    recover_packages(&store, &supervisor, &[allow_package()]).unwrap();

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.generations.len(), 1);
    let restored = &status.generations[0];
    assert_eq!(restored.version.as_deref(), Some("1.0.0"));
    assert!(restored.serving && restored.healthy);
    assert_eq!(status.current_main_pid, Some(restored.pid));
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    let recovered = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(recovered.phase, Phase::RolledBack);
    assert_eq!(recovered.active_pid, Some(restored.pid));
    assert_eq!(recovered.candidate_pid, None);
    connect(address, "1.0.0").finish("1.0.0");
}

#[test]
fn recovery_serves_restored_generation_before_draining_serving_candidate() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);

    let verified = verified_fixture(&store, "2.0.0", None);
    let candidate_version = verified.contract.version.to_string();
    let candidate_digest = verified.artifact_sha256.clone();
    let candidate_tree = store
        .promote(
            "fixture",
            &candidate_version,
            &verified.artifact_path,
            &candidate_digest,
            &verified.manifest,
        )
        .unwrap();
    let candidate_pid = supervisor
        .start_successor(
            "fixture",
            &candidate_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    let old_generation = supervisor.generation_handle(old_pid).unwrap();
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
    supervisor.stop_pid(old_pid).unwrap();
    let in_flight = connect(address, "2.0.0");

    let mut state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    state.operation_id = uuid::Uuid::new_v4().to_string();
    state.phase = Phase::RollingBack;
    state.candidate_version = Some(candidate_version);
    state.candidate_digest = Some(candidate_digest);
    state.handoff_previous_version = Some("1.0.0".into());
    state.handoff_previous_pid = Some(old_pid);
    state.handoff_previous_generation = Some(old_generation);
    state.candidate_pid = Some(candidate_pid);
    let candidate = supervisor.generation_handle(candidate_pid).unwrap();
    state.candidate_generation_id = Some(candidate.generation_id.clone());
    state.candidate_generation = Some(candidate);
    store
        .save_json(&store.state_path("fixture"), &state)
        .unwrap();

    let (drained_tx, drained_rx) = std::sync::mpsc::channel();
    let probe = UnhealthyPidSupervisor {
        inner: &supervisor,
        unhealthy_pid: candidate_pid,
        stop_calls: std::sync::atomic::AtomicUsize::new(0),
        drain_notice: Some(drained_tx),
    };
    std::thread::scope(|scope| {
        let recovery = scope.spawn(|| recover_packages(&store, &probe, &[allow_package()]));
        let (drained_pid, drained) = match drained_rx
            .recv_timeout(std::time::Duration::from_secs(5))
        {
            Ok(result) => result,
            Err(_) => {
                let result = recovery.join().unwrap();
                panic!(
                    "recovery did not reach candidate drain: {result:?}; status={:?}; state={:?}",
                    supervisor.status("fixture").unwrap(),
                    store.load_state::<PackageState>("fixture").unwrap()
                );
            }
        };
        assert_eq!(drained_pid, candidate_pid);
        assert!(drained);
        connect(address, "1.0.0").finish("1.0.0");
        in_flight.finish("2.0.0");
        recovery.join().unwrap().unwrap();
    });

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.generations.len(), 1);
    assert_eq!(status.current_main_pid, Some(status.generations[0].pid));
    assert!(status.generations[0].serving && status.generations[0].healthy);
}

#[test]
fn recovery_drain_timeout_keeps_restored_release_uncommitted_and_retryable() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);
    let old = supervisor.generation_handle(old_pid).unwrap();

    let mut verified = verified_fixture(&store, "2.0.0", None);
    verified.contract.drain_timeout_ms = 100;
    verified.manifest = serde_json::to_vec(&verified.contract).unwrap();
    let candidate_version = verified.contract.version.to_string();
    let candidate_digest = verified.artifact_sha256.clone();
    let candidate_tree = store
        .promote(
            "fixture",
            &candidate_version,
            &verified.artifact_path,
            &candidate_digest,
            &verified.manifest,
        )
        .unwrap();
    let generation_id = uuid::Uuid::new_v4().to_string();
    let candidate = Supervisor::start_successor(
        &supervisor,
        "fixture",
        "fixture",
        &generation_id,
        &candidate_tree,
        Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
    )
    .unwrap();
    assert!(Supervisor::drain_generation(&supervisor, &old, 1000).unwrap());
    assert!(Supervisor::wait_for_drain(&supervisor, &old, 1000, None).unwrap());
    Supervisor::activate_generation(&supervisor, &candidate).unwrap();
    Supervisor::commit_active(&supervisor, Some(&old), &candidate).unwrap();
    Supervisor::stop_generation(&supervisor, &old).unwrap();
    store
        .replace_pointer("fixture", "previous", "1.0.0")
        .unwrap();
    store.replace_pointer("fixture", "active", "2.0.0").unwrap();
    let in_flight = connect(address, "2.0.0");

    let mut state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    state.operation_id = uuid::Uuid::new_v4().to_string();
    state.phase = Phase::RollingBack;
    state.active_version = Some("2.0.0".into());
    state.active_pid = Some(candidate.pid);
    state.active_generation = Some(candidate.clone());
    state.previous_version = Some("1.0.0".into());
    state.candidate_version = Some(candidate_version);
    state.candidate_digest = Some(candidate_digest);
    state.candidate_pid = Some(candidate.pid);
    state.candidate_generation_id = Some(candidate.generation_id.clone());
    state.candidate_generation = Some(candidate.clone());
    state.handoff_previous_version = Some("1.0.0".into());
    state.handoff_previous_pid = Some(old.pid);
    state.handoff_previous_generation = Some(old);
    state.highest_verified_version = Some("2.0.0".into());
    store
        .save_json(&store.state_path("fixture"), &state)
        .unwrap();

    let probe = UnhealthyPidSupervisor {
        inner: &supervisor,
        unhealthy_pid: candidate.pid,
        stop_calls: std::sync::atomic::AtomicUsize::new(0),
        drain_notice: None,
    };
    assert!(recover_packages(&store, &probe, &[allow_package()]).is_err());
    assert_eq!(
        probe.stop_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(candidate.pid));
    assert!(status.generations.iter().any(|generation| {
        generation.version.as_deref() == Some("1.0.0") && generation.serving && generation.healthy
    }));
    assert!(
        status
            .generations
            .iter()
            .any(|generation| { generation.pid == candidate.pid && !generation.serving })
    );
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("2.0.0")
    );
    let pending = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(pending.phase, Phase::RollingBack);
    assert_eq!(pending.active_version.as_deref(), Some("2.0.0"));
    assert!(pending.recovery_generation.is_some());

    connect(address, "1.0.0").finish("1.0.0");
    in_flight.finish("2.0.0");
    assert!(Supervisor::wait_for_drain(&supervisor, &candidate, 1000, None).unwrap());
    recover_packages(&store, &probe, &[allow_package()]).unwrap();

    let completed = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(completed.phase, Phase::RolledBack);
    assert_eq!(completed.active_version.as_deref(), Some("1.0.0"));
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        probe.stop_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn recovery_restarts_a_live_previous_generation_that_fails_its_health_contract() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);
    let mut state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    state.operation_id = uuid::Uuid::new_v4().to_string();
    state.phase = Phase::RollingBack;
    state.handoff_previous_version = Some("1.0.0".into());
    state.handoff_previous_pid = Some(old_pid);
    state.handoff_previous_generation = Some(supervisor.generation_handle(old_pid).unwrap());
    store
        .save_json(&store.state_path("fixture"), &state)
        .unwrap();
    let probe = UnhealthyPidSupervisor {
        inner: &supervisor,
        unhealthy_pid: old_pid,
        stop_calls: std::sync::atomic::AtomicUsize::new(0),
        drain_notice: None,
    };

    recover_packages(&store, &probe, &[allow_package()]).unwrap();

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.generations.len(), 1);
    let restored = &status.generations[0];
    assert_ne!(restored.pid, old_pid);
    assert!(restored.serving && restored.healthy);
    assert_eq!(status.current_main_pid, Some(restored.pid));
    connect(address, "1.0.0").finish("1.0.0");
}

#[test]
fn recovery_keeps_healthy_candidate_serving_until_retained_previous_is_ready() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);
    let verified = verified_fixture(&store, "2.0.0", None);
    let candidate_version = verified.contract.version.to_string();
    let candidate_digest = verified.artifact_sha256.clone();
    let candidate_tree = store
        .promote(
            "fixture",
            &candidate_version,
            &verified.artifact_path,
            &candidate_digest,
            &verified.manifest,
        )
        .unwrap();
    let candidate_pid = supervisor
        .start_successor(
            "fixture",
            &candidate_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    assert!(matches!(
        supervisor
            .handoff(old_pid, candidate_pid, 5000, 5000)
            .unwrap(),
        apollo_updated::supervisor::fixture::HandoffOutcome::Committed { .. }
    ));
    let in_flight = connect(address, "2.0.0");

    let mut state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    state.operation_id = uuid::Uuid::new_v4().to_string();
    state.phase = Phase::RollingBack;
    state.candidate_version = Some(candidate_version);
    state.candidate_digest = Some(candidate_digest);
    state.highest_verified_version = Some("2.0.0".into());
    state.handoff_previous_version = Some("1.0.0".into());
    state.handoff_previous_pid = Some(old_pid);
    state.handoff_previous_generation = Some(supervisor.generation_handle(old_pid).unwrap());
    state.candidate_pid = Some(candidate_pid);
    let candidate = supervisor.generation_handle(candidate_pid).unwrap();
    state.candidate_generation_id = Some(candidate.generation_id.clone());
    state.candidate_generation = Some(candidate);
    store
        .save_json(&store.state_path("fixture"), &state)
        .unwrap();

    let (drained_tx, drained_rx) = std::sync::mpsc::channel();
    let probe = UnhealthyPidSupervisor {
        inner: &supervisor,
        unhealthy_pid: old_pid,
        stop_calls: std::sync::atomic::AtomicUsize::new(0),
        drain_notice: Some(drained_tx),
    };
    std::thread::scope(|scope| {
        let recovery = scope.spawn(|| recover_packages(&store, &probe, &[allow_package()]));
        let (first_pid, first_drained) = match drained_rx
            .recv_timeout(std::time::Duration::from_secs(20))
        {
            Ok(result) => result,
            Err(_) => {
                let result = recovery.join().unwrap();
                panic!(
                    "recovery did not drain the unhealthy predecessor: {result:?}; status={:?}; state={:?}",
                    supervisor.status("fixture").unwrap(),
                    store.load_state::<PackageState>("fixture").unwrap()
                );
            }
        };
        assert_eq!(first_pid, old_pid);
        assert!(first_drained);
        let serving = supervisor.status("fixture").unwrap();
        assert_eq!(serving.current_main_pid, Some(candidate_pid));
        assert!(
            serving
                .generations
                .iter()
                .any(|generation| generation.pid == candidate_pid && generation.serving)
        );

        let (candidate_drained, drained) = drained_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("recovery did not reach candidate drain");
        assert_eq!(candidate_drained, candidate_pid);
        assert!(drained);
        let restored_status = supervisor.status("fixture").unwrap();
        assert_eq!(restored_status.current_main_pid, Some(candidate_pid));
        let restored = restored_status
            .generations
            .iter()
            .find(|generation| generation.version.as_deref() == Some("1.0.0"))
            .unwrap();
        assert_eq!(restored.version.as_deref(), Some("1.0.0"));
        assert!(restored.serving && restored.healthy);
        assert!(
            restored_status
                .generations
                .iter()
                .any(|generation| { generation.pid == candidate_pid && !generation.serving })
        );
        connect(address, "1.0.0").finish("1.0.0");
        in_flight.finish("2.0.0");
        recovery.join().unwrap().unwrap();
    });

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.generations.len(), 1);
    assert_eq!(status.generations[0].version.as_deref(), Some("1.0.0"));
    assert_eq!(status.current_main_pid, Some(status.generations[0].pid));
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
}
