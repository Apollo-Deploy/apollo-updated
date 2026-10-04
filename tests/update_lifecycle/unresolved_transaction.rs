use super::{FIXTURE_TEST_LOCK, activate_old, allow_package, supervisor, verified_fixture};
use apollo_updated::{
    disk::Store,
    lifecycle::install_verified,
    state::{PackageState, Phase},
    supervisor::Supervisor,
};

#[test]
fn unresolved_transaction_blocks_a_new_install() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, _address, scratch) = supervisor();
    let store = Store::open(&scratch.path().join("state-root")).unwrap();
    activate_old(&store, &supervisor);
    let mut state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    state.phase = Phase::Draining;
    state.candidate_version = Some("2.0.0".into());
    state.candidate_pid = Some(u32::MAX);
    store
        .save_json(&store.state_path("fixture"), &state)
        .unwrap();

    let error = install_verified(
        &store,
        &supervisor,
        &allow_package(),
        verified_fixture(&store, "2.0.0", None),
        Some(supervisor.listener_fd()),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("requires recovery"));
    assert_eq!(
        supervisor.status("fixture").unwrap().current_main_pid,
        Some(state.active_pid.unwrap())
    );
}
