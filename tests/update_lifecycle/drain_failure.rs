use super::*;

fn connect_any(address: std::net::SocketAddr) -> (Connection, String) {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut connection = Connection(BufReader::new(stream));
    let mut line = String::new();
    connection.0.read_line(&mut line).unwrap();
    let version = line
        .trim()
        .strip_prefix("accepted:")
        .expect("fixture accept response")
        .to_owned();
    (connection, version)
}

#[test]
fn drain_failure_restores_old_generation_after_overlap() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old_with_marker(&store, &supervisor, Some(".fixture-drain-delay"));
    let package = allow_package();
    let mut verified = verified_fixture(&store, "2.0.0", None);
    verified.contract.drain_timeout_ms = 100;
    verified.manifest = serde_json::to_vec(&verified.contract).unwrap();
    let observer_store = Store::open(&root).unwrap();
    let update_store = store;
    let update_supervisor = std::sync::Arc::new(supervisor);
    let update_listener = update_supervisor.listener_fd();
    let handoff_supervisor = std::sync::Arc::clone(&update_supervisor);
    let update = std::thread::spawn(move || {
        install_verified(
            &update_store,
            handoff_supervisor.as_ref(),
            &package,
            verified,
            Some(update_listener),
        )
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let phase = observer_store
            .load_state::<PackageState>("fixture")
            .unwrap()
            .unwrap()
            .phase;
        if phase == Phase::Draining {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "update did not reach drain"
        );
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(50));
    let pending_connection = std::thread::spawn(move || {
        let (connection, version) = connect_any(address);
        connection.finish(&version);
        version
    });
    assert!(update.join().unwrap().is_err());
    assert!(["1.0.0", "2.0.0"].contains(&pending_connection.join().unwrap().as_str()));
    let status = update_supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(old_pid));
    assert_eq!(status.generations.len(), 1);
    assert!(status.generations[0].serving && status.generations[0].healthy);
    connect(address, "1.0.0").finish("1.0.0");
    let state = Store::open(&root)
        .unwrap()
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(state.phase, Phase::RolledBack);
    assert_eq!(state.active_pid, Some(old_pid));
    assert_eq!(state.active_version.as_deref(), Some("1.0.0"));
}
