use super::*;

#[test]
fn candidate_crash_during_predecessor_drain_resumes_old_generation_promptly() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let supervisor = std::sync::Arc::new(supervisor);
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, supervisor.as_ref());
    let existing = connect(address, "1.0.0");
    let verified = verified_fixture(&store, "2.0.0", None);
    let package = allow_package();
    let observer = Store::open(&root).unwrap();
    let lock = store.lock_package("fixture").unwrap();
    let update_supervisor = std::sync::Arc::clone(&supervisor);
    let listener_fd = supervisor.listener_fd();
    let update = std::thread::spawn(move || {
        let _lock = lock;
        install_verified(
            &store,
            update_supervisor.as_ref(),
            &package,
            verified,
            Some(listener_fd),
        )
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let candidate_pid = loop {
        let state = observer
            .load_state::<PackageState>("fixture")
            .unwrap()
            .unwrap();
        if state.phase == Phase::Draining {
            let candidate_pid = state.candidate_pid.expect("draining state has a candidate");
            let status = supervisor.status("fixture").unwrap();
            let candidate_serving = status.generations.iter().any(|generation| {
                generation.pid == candidate_pid && generation.healthy && generation.serving
            });
            let predecessor_draining = status
                .generations
                .iter()
                .any(|generation| generation.pid == old_pid && !generation.serving);
            if candidate_serving && predecessor_draining {
                break candidate_pid;
            }
        }
        if std::time::Instant::now() >= deadline {
            let status = supervisor.status("fixture").unwrap();
            let update_result = match update.join() {
                Ok(Ok(_)) => "success".to_owned(),
                Ok(Err(error)) => format!("{error:#}"),
                Err(_) => "update thread panicked".to_owned(),
            };
            panic!(
                "update did not reach serving-candidate predecessor drain: phase={:?}, candidate_pid={:?}, status={status:?}, update_result={update_result}",
                state.phase, state.candidate_pid
            );
        }
        std::thread::yield_now();
    };

    let crash_at = std::time::Instant::now();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(candidate_pid).unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    let (connection_tx, connection_rx) = std::sync::mpsc::sync_channel(1);
    let queued_client = std::thread::spawn(move || {
        connection_tx.send(connect(address, "1.0.0")).unwrap();
    });

    assert!(update.join().unwrap().is_err());
    assert!(
        crash_at.elapsed() < Duration::from_secs(2),
        "candidate failure was not detected before the five-second drain deadline"
    );
    let resumed = connection_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("old generation resumes accepts promptly after candidate crash");
    queued_client.join().unwrap();
    resumed.finish("1.0.0");
    existing.finish("1.0.0");

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(old_pid));
    assert_eq!(
        Store::open(&root)
            .unwrap()
            .read_pointer("fixture", "active")
            .unwrap()
            .as_deref(),
        Some("1.0.0")
    );
}
