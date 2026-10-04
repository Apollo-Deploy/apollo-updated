use super::super::{FIXTURE_TEST_LOCK, activate_old, allow_package, connect, supervisor};
use apollo_updated::{
    disk::Store,
    lifecycle::recover_packages,
    state::{PackageState, Phase},
    supervisor::Supervisor,
};

#[test]
fn committed_release_is_restarted_when_reboot_removed_its_generation() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let store = Store::open(&scratch.path().join("state-root")).unwrap();
    let old_pid = activate_old(&store, &supervisor);

    assert!(supervisor.drain_pid(old_pid, 5000).unwrap());
    assert!(supervisor.wait_for_drain(old_pid, 5000, None).unwrap());
    supervisor.stop_pid(old_pid).unwrap();
    assert!(supervisor.status("fixture").unwrap().generations.is_empty());

    recover_packages(&store, &supervisor, &[allow_package()]).unwrap();

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.generations.len(), 1);
    let restored = &status.generations[0];
    assert_eq!(restored.version.as_deref(), Some("1.0.0"));
    assert!(restored.serving && restored.healthy);
    assert_eq!(status.current_main_pid, Some(restored.pid));
    let state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(state.phase, Phase::Committed);
    assert_eq!(state.active_pid, Some(restored.pid));
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    connect(address, "1.0.0").finish("1.0.0");
}
