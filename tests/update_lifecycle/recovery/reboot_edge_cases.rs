use super::*;

#[test]
fn recovery_ignores_a_candidate_pid_reused_by_the_previous_version() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = super::super::supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);
    let (candidate_pid, mut state) = prepare_candidate(&store, &supervisor, old_pid);
    supervisor.stop_pid(candidate_pid).unwrap();

    // Model a reboot where the saved candidate PID now names the restarted
    // previous binary. Version identity must win over the stale numeric PID.
    state.phase = Phase::RollingBack;
    state.candidate_pid = Some(old_pid);
    persist(&store, &state);
    recover(&store, &supervisor);

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(old_pid));
    assert_eq!(status.generations.len(), 1);
    assert!(status.generations[0].serving && status.generations[0].healthy);
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        store
            .load_state::<PackageState>("fixture")
            .unwrap()
            .unwrap()
            .phase,
        Phase::RolledBack
    );
    connect(address, "1.0.0").finish("1.0.0");
}

#[test]
fn recovery_commits_healthy_candidate_when_reboot_left_no_previous_generation() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = super::super::supervisor();
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
    supervisor.stop_pid(old_pid).unwrap();
    store.replace_pointer("fixture", "active", "2.0.0").unwrap();
    state.phase = Phase::RollingBack;
    persist(&store, &state);

    // A boot from the candidate active pointer can leave only the candidate
    // process running. Recovery may commit it after revalidating the release.
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
    let recovered = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(recovered.phase, Phase::Committed);
    assert_eq!(recovered.active_pid, Some(candidate_pid));
    connect(address, "2.0.0").finish("2.0.0");
}
