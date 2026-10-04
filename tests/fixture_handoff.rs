use apollo_updated::supervisor::{
    Supervisor,
    fixture::{HandoffOutcome, InProcessFixtureSupervisor},
};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const FIXTURE: &str = env!("CARGO_BIN_EXE_apollo-updated-fixture");
static FIXTURE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Connection(BufReader<TcpStream>);

impl Connection {
    fn finish(mut self, version: &str) {
        self.0.get_mut().write_all(b"finish\n").unwrap();
        let mut line = String::new();
        self.0.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), format!("finished:{version}"));
    }
}

fn connect(address: std::net::SocketAddr, version: &str) -> Connection {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut connection = Connection(BufReader::new(stream));
    let mut line = String::new();
    connection.0.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), format!("accepted:{version}"));
    connection
}

fn tree(parent: &Path, version: &str, marker: Option<&str>) -> PathBuf {
    let path = parent.join(version);
    fs::create_dir_all(&path).unwrap();
    fs::copy(FIXTURE, path.join("fixture-daemon")).unwrap();
    fs::write(path.join("digest.sha256"), format!("{}\n", "0".repeat(64))).unwrap();
    if let Some(marker) = marker {
        fs::write(path.join(marker), b"qualification").unwrap();
    }
    path
}

fn supervisor() -> (
    InProcessFixtureSupervisor,
    std::net::SocketAddr,
    tempfile::TempDir,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let supervisor = InProcessFixtureSupervisor::new(listener).unwrap();
    (supervisor, address, scratch)
}

#[test]
fn inherited_listener_handoff_preserves_open_connection_and_unowned_process() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let supervisor = std::sync::Arc::new(supervisor);
    let mut side_process = ChildGuard(
        Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let old_tree = tree(scratch.path(), "1.0.0", None);
    let old_pid = supervisor
        .start_successor(
            "fixture",
            &old_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    supervisor.set_initial_active(old_pid, 1000).unwrap();
    let existing = connect(address, "1.0.0");

    let new_tree = tree(scratch.path(), "2.0.0", None);
    let new_pid = supervisor
        .start_successor(
            "fixture",
            &new_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    let handoff_supervisor = std::sync::Arc::clone(&supervisor);
    let handoff =
        std::thread::spawn(move || handoff_supervisor.handoff(old_pid, new_pid, 1000, 1000));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while supervisor
        .status("fixture")
        .unwrap()
        .generations
        .iter()
        .any(|generation| generation.pid == old_pid && generation.serving)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "old generation did not drain"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let status = supervisor.status("fixture").unwrap();
    assert!(status.generations.iter().any(|generation| {
        generation.pid == new_pid && !generation.serving && generation.healthy
    }));
    let pending = std::thread::spawn(move || connect(address, "2.0.0"));
    existing.finish("1.0.0");
    pending.join().unwrap().finish("2.0.0");
    assert_eq!(
        handoff.join().unwrap().unwrap(),
        HandoffOutcome::Committed {
            previous_pid: old_pid,
            serving_pid: new_pid,
        }
    );
    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(new_pid));
    assert_eq!(status.listener_owner, Some(std::process::id()));
    assert_eq!(status.generations.len(), 2);
    assert!(status.generations.iter().any(|generation| {
        generation.pid == old_pid && !generation.serving && generation.healthy
    }));
    assert!(status.generations.iter().any(|generation| {
        generation.pid == new_pid && generation.serving && generation.healthy
    }));

    supervisor.stop_pid(old_pid).unwrap();
    assert!(side_process.0.try_wait().unwrap().is_none());
}

#[test]
fn unhealthy_successor_and_drain_timeout_leave_old_generation_serving() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let old_tree = tree(scratch.path(), "1.0.0", None);
    let old_pid = supervisor
        .start_successor(
            "fixture",
            &old_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    supervisor.set_initial_active(old_pid, 1000).unwrap();

    let unhealthy_tree = tree(scratch.path(), "2.0.0", Some(".fixture-unhealthy"));
    let unhealthy_pid = supervisor
        .start_successor(
            "fixture",
            &unhealthy_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    assert_eq!(
        supervisor
            .handoff(old_pid, unhealthy_pid, 1000, 1000)
            .unwrap(),
        HandoffOutcome::CandidateUnhealthy
    );
    connect(address, "1.0.0").finish("1.0.0");
    supervisor.stop_pid(unhealthy_pid).unwrap();

    let in_flight = connect(address, "1.0.0");
    let delayed_tree = tree(scratch.path(), "3.0.0", None);
    let delayed_pid = supervisor
        .start_successor(
            "fixture",
            &delayed_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    assert_eq!(
        supervisor.handoff(old_pid, delayed_pid, 1000, 50).unwrap(),
        HandoffOutcome::DrainTimedOut
    );
    assert_eq!(
        supervisor.status("fixture").unwrap().current_main_pid,
        Some(old_pid)
    );
    assert!(
        supervisor
            .status("fixture")
            .unwrap()
            .generations
            .iter()
            .any(|generation| generation.pid == delayed_pid && !generation.serving)
    );
    in_flight.finish("1.0.0");
    connect(address, "1.0.0").finish("1.0.0");
    supervisor.stop_pid(delayed_pid).unwrap();
}

#[test]
fn successor_crash_during_activation_resumes_the_previous_pid() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let old_tree = tree(scratch.path(), "1.0.0", None);
    let old_pid = supervisor
        .start_successor(
            "fixture",
            &old_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    supervisor.set_initial_active(old_pid, 1000).unwrap();
    let crash_tree = tree(scratch.path(), "2.0.0", Some(".fixture-crash-on-activate"));
    let crash_pid = supervisor
        .start_successor(
            "fixture",
            &crash_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();

    assert_eq!(
        supervisor.handoff(old_pid, crash_pid, 1000, 1000).unwrap(),
        HandoffOutcome::CandidateUnavailable
    );
    assert_eq!(
        supervisor.status("fixture").unwrap().current_main_pid,
        Some(old_pid)
    );
    assert!(supervisor.health_of_pid(old_pid, 1000).unwrap());
    connect(address, "1.0.0").finish("1.0.0");
    supervisor.stop_pid(crash_pid).unwrap();

    let after_activate_tree = tree(
        scratch.path(),
        "3.0.0",
        Some(".fixture-crash-after-activate"),
    );
    let after_activate_pid = supervisor
        .start_successor(
            "fixture",
            &after_activate_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    assert_eq!(
        supervisor
            .handoff(old_pid, after_activate_pid, 1000, 1000)
            .unwrap(),
        HandoffOutcome::CandidateUnavailable
    );
    assert_eq!(
        supervisor.status("fixture").unwrap().current_main_pid,
        Some(old_pid)
    );
    assert!(supervisor.health_of_pid(old_pid, 1000).unwrap());
    connect(address, "1.0.0").finish("1.0.0");
}

#[test]
fn successor_crash_in_the_commit_window_restores_the_previous_listener_owner() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let old_tree = tree(scratch.path(), "1.0.0", None);
    let old_pid = supervisor
        .start_successor(
            "fixture",
            &old_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    supervisor.set_initial_active(old_pid, 1000).unwrap();
    let crash_tree = tree(
        scratch.path(),
        "2.0.0",
        Some(".fixture-crash-before-commit"),
    );
    let crash_pid = supervisor
        .start_successor(
            "fixture",
            &crash_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();

    assert_eq!(
        supervisor.handoff(old_pid, crash_pid, 1000, 1000).unwrap(),
        HandoffOutcome::CandidateUnavailable
    );
    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(old_pid));
    assert!(status.generations.iter().any(|generation| {
        generation.pid == old_pid && generation.serving && generation.healthy
    }));
    connect(address, "1.0.0").finish("1.0.0");
    supervisor.stop_pid(crash_pid).unwrap();
}

#[test]
fn successor_crash_during_old_drain_restores_accepts_promptly() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let supervisor = std::sync::Arc::new(supervisor);
    let old_tree = tree(scratch.path(), "1.0.0", None);
    let old_pid = supervisor
        .start_successor(
            "fixture",
            &old_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    supervisor.set_initial_active(old_pid, 1000).unwrap();
    let existing = connect(address, "1.0.0");

    let candidate_tree = tree(scratch.path(), "2.0.0", None);
    let candidate_pid = supervisor
        .start_successor(
            "fixture",
            &candidate_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    let handoff_supervisor = std::sync::Arc::clone(&supervisor);
    let handoff =
        std::thread::spawn(move || handoff_supervisor.handoff(old_pid, candidate_pid, 1000, 3000));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while supervisor
        .status("fixture")
        .unwrap()
        .generations
        .iter()
        .any(|generation| generation.pid == old_pid && generation.serving)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "old generation did not enter drain"
        );
        std::thread::yield_now();
    }

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(candidate_pid).unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    let (connection_tx, connection_rx) = std::sync::mpsc::sync_channel(1);
    let resumed_client = std::thread::spawn(move || {
        connection_tx.send(connect(address, "1.0.0")).unwrap();
    });
    let resumed = connection_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("old generation resumes accepts before the bounded drain deadline");
    resumed_client.join().unwrap();
    assert_eq!(
        handoff.join().unwrap().unwrap(),
        HandoffOutcome::CandidateUnavailable
    );
    resumed.finish("1.0.0");
    existing.finish("1.0.0");
    assert_eq!(
        supervisor.status("fixture").unwrap().current_main_pid,
        Some(old_pid)
    );
    supervisor.stop_pid(candidate_pid).unwrap();
}

#[test]
fn drain_timeout_keeps_candidate_gated_and_restores_queued_connections_to_old() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let supervisor = std::sync::Arc::new(supervisor);
    let old_tree = tree(scratch.path(), "1.0.0", None);
    let old_pid = supervisor
        .start_successor(
            "fixture",
            &old_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    supervisor.set_initial_active(old_pid, 1000).unwrap();
    let existing = connect(address, "1.0.0");

    let candidate_tree = tree(scratch.path(), "2.0.0", None);
    let candidate_pid = supervisor
        .start_successor(
            "fixture",
            &candidate_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    let handoff_supervisor = std::sync::Arc::clone(&supervisor);
    let handoff =
        std::thread::spawn(move || handoff_supervisor.handoff(old_pid, candidate_pid, 1000, 150));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while supervisor
        .status("fixture")
        .unwrap()
        .generations
        .iter()
        .any(|generation| generation.pid == old_pid && generation.serving)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "old generation did not enter drain"
        );
        std::thread::yield_now();
    }
    let pending = std::thread::spawn(move || connect(address, "1.0.0"));
    assert_eq!(
        handoff.join().unwrap().unwrap(),
        HandoffOutcome::DrainTimedOut
    );

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(old_pid));
    assert!(status.generations.iter().any(|generation| {
        generation.pid == old_pid && generation.serving && generation.healthy
    }));
    assert!(status.generations.iter().any(|generation| {
        generation.pid == candidate_pid && !generation.serving && generation.healthy
    }));
    assert!(status.generations.iter().any(|generation| {
        generation.pid == candidate_pid && generation.healthy && !generation.serving
    }));
    pending.join().unwrap().finish("1.0.0");
    existing.finish("1.0.0");
    supervisor.stop_pid(candidate_pid).unwrap();
}

#[test]
fn ten_thousand_update_rollback_cycles_keep_two_generations_and_no_fd_leak() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, _, scratch) = supervisor();
    let old_tree = tree(scratch.path(), "1.0.0", None);
    let new_tree = tree(scratch.path(), "2.0.0", None);
    let old_pid = supervisor
        .start_successor(
            "fixture",
            &old_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    supervisor.set_initial_active(old_pid, 1000).unwrap();
    let new_pid = supervisor
        .start_successor(
            "fixture",
            &new_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    let baseline_fds = fs::read_dir("/proc/self/fd").unwrap().count();
    let mut serving = old_pid;

    for _ in 0..10_000 {
        let candidate = if serving == old_pid { new_pid } else { old_pid };
        assert_eq!(
            supervisor.handoff(serving, candidate, 1000, 1000).unwrap(),
            HandoffOutcome::Committed {
                previous_pid: serving,
                serving_pid: candidate,
            }
        );
        serving = candidate;
        let rollback = if serving == old_pid { new_pid } else { old_pid };
        assert_eq!(
            supervisor.handoff(serving, rollback, 1000, 1000).unwrap(),
            HandoffOutcome::Committed {
                previous_pid: serving,
                serving_pid: rollback,
            }
        );
        serving = rollback;
    }

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(serving));
    assert_eq!(status.generations.len(), 2);
    assert_eq!(fs::read_dir("/proc/self/fd").unwrap().count(), baseline_fds);
    let candidate = if serving == old_pid { new_pid } else { old_pid };
    assert!(matches!(
        supervisor.handoff(serving, candidate, 1000, 1000).unwrap(),
        HandoffOutcome::Committed { .. }
    ));
    supervisor.stop_pid(old_pid).unwrap();
    drop(supervisor);
}
