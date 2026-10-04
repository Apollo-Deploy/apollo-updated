use apollo_updated::supervisor::{Supervisor, fixture::InProcessFixtureSupervisor};
use apollo_updated::{
    contract::{
        ArtifactSource, Compatibility, HealthContract, ListenerMode, PackageContract, Readiness,
        SUPPORTED_PACKAGE_FORMAT, SUPPORTED_PROTOCOL_VERSION, SUPPORTED_STATE_FORMAT,
    },
    disk::Store,
    history::{self, HistoryEvent},
    lifecycle::{install_verified, rollback_previous},
    settings::AllowedPackage,
    state::{PackageState, Phase},
    tuf_client::VerifiedPackage,
};
use semver::Version;
use sha2::Digest;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};

const FIXTURE: &str = env!("CARGO_BIN_EXE_apollo-updated-fixture");
static FIXTURE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[path = "update_lifecycle/crash_during_drain.rs"]
mod crash_during_drain;
#[path = "update_lifecycle/drain_failure.rs"]
mod drain_failure;
#[path = "update_lifecycle/recovery.rs"]
mod recovery;
#[path = "update_lifecycle/recovery_missing_previous.rs"]
mod recovery_missing_previous;
#[path = "update_lifecycle/unresolved_transaction.rs"]
mod unresolved_transaction;

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

fn verified_fixture(store: &Store, version: &str, marker: Option<&str>) -> VerifiedPackage {
    let artifact_dir = store.root().join("staging/fixture").join(version);
    fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_path = artifact_dir.join("target.part");
    let output = fs::File::create(&artifact_path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(output, 0).unwrap();
    let mut archive = tar::Builder::new(encoder);
    let fixture_bytes = fs::read(FIXTURE).unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_size(fixture_bytes.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    archive
        .append_data(&mut header, "fixture-daemon", fixture_bytes.as_slice())
        .unwrap();
    if let Some(marker) = marker {
        let mut header = tar::Header::new_gnu();
        header.set_size(marker.len() as u64);
        header.set_mode(0o400);
        header.set_cksum();
        archive
            .append_data(&mut header, marker, marker.as_bytes())
            .unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap();
    let artifact = fs::read(&artifact_path).unwrap();
    let digest = hex::encode(sha2::Sha256::digest(&artifact));
    let contract = PackageContract {
        package_id: "fixture".into(),
        version: Version::parse(version).unwrap(),
        architecture: "x86_64-linux-gnu".into(),
        artifact: ArtifactSource::Tuf {
            target: format!("packages/fixture/{version}/x86_64-linux-gnu/payload.tar.zst"),
        },
        sha256: digest.clone(),
        size: artifact.len() as u64,
        service_name: "fixture".into(),
        listener: ListenerMode::InheritedFd,
        health: HealthContract {
            readiness: Readiness::ProcessAlive,
            stabilization_ms: 0,
            failure_threshold: 1,
        },
        drain_timeout_ms: 5000,
        health_timeout_ms: 1000,
        stabilization_ms: 0,
        compatibility: Compatibility {
            minimum_updater: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            maximum_updater: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            package_format: SUPPORTED_PACKAGE_FORMAT,
            state_format: SUPPORTED_STATE_FORMAT,
            protocol_version: SUPPORTED_PROTOCOL_VERSION,
        },
        coordinated_set: None,
    };
    let manifest = serde_json::to_vec(&contract).unwrap();
    VerifiedPackage {
        contract,
        manifest,
        target_name: format!("packages/fixture/{version}/x86_64-linux-gnu/manifest.json"),
        artifact_path,
        artifact_sha256: digest,
        _space_reservation: None,
    }
}

fn allow_package() -> AllowedPackage {
    AllowedPackage {
        package_id: "fixture".into(),
        service_name: "fixture".into(),
        architecture: "x86_64-linux-gnu".into(),
        ..AllowedPackage::default()
    }
}

fn activate_old(store: &Store, supervisor: &InProcessFixtureSupervisor) -> u32 {
    activate_old_with_marker(store, supervisor, None)
}

fn activate_old_with_marker(
    store: &Store,
    supervisor: &InProcessFixtureSupervisor,
    marker: Option<&str>,
) -> u32 {
    let verified = verified_fixture(store, "1.0.0", marker);
    let digest = verified.artifact_sha256.clone();
    let manifest_sha256 = hex::encode(sha2::Sha256::digest(&verified.manifest));
    let version_dir = store
        .promote(
            "fixture",
            "1.0.0",
            &verified.artifact_path,
            &verified.artifact_sha256,
            &verified.manifest,
        )
        .unwrap();
    let old_tree = version_dir;
    store.replace_pointer("fixture", "active", "1.0.0").unwrap();
    let old_pid = supervisor
        .start_successor(
            "fixture",
            &old_tree,
            Some(&supervisor.listener_source("fixture").unwrap().unwrap()),
        )
        .unwrap();
    supervisor.set_initial_active(old_pid, 1000).unwrap();
    let active_generation = supervisor.generation_handle(old_pid).unwrap();
    let mut state = PackageState::initial("fixture");
    state.phase = Phase::Committed;
    state.active_version = Some("1.0.0".into());
    state.active_pid = Some(old_pid);
    state.active_generation = Some(active_generation.clone());
    state.highest_verified_version = Some("1.0.0".into());
    store
        .save_json(&store.state_path("fixture"), &state)
        .unwrap();
    history::append(
        store.root(),
        HistoryEvent {
            operation_id: uuid::Uuid::new_v4().to_string(),
            package_id: "fixture".into(),
            version: Some("1.0.0".into()),
            digest: Some(digest),
            manifest_sha256: Some(manifest_sha256),
            operation: "update".into(),
            result: "committed".into(),
            timestamp_unix_ms: history::now_ms(),
        },
    )
    .unwrap();
    old_pid
}

#[test]
fn verified_update_commits_durable_pointers_only_after_listener_handoff() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let supervisor = std::sync::Arc::new(supervisor);
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, supervisor.as_ref());
    let existing = connect(address, "1.0.0");

    let verified = verified_fixture(&store, "2.0.0", None);
    let allow = allow_package();
    let lock = store.lock_package("fixture").unwrap();
    let update_supervisor = std::sync::Arc::clone(&supervisor);
    let update = std::thread::spawn(move || {
        let _lock = lock;
        install_verified(
            &store,
            update_supervisor.as_ref(),
            &allow,
            verified,
            Some(update_supervisor.listener_fd()),
        )
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let status = supervisor.status("fixture").unwrap();
        let candidate_serving = status.generations.iter().any(|generation| {
            generation.version.as_deref() == Some("2.0.0")
                && generation.healthy
                && generation.serving
        });
        let predecessor_draining = status
            .generations
            .iter()
            .any(|generation| generation.pid == old_pid && !generation.serving);
        if candidate_serving && predecessor_draining {
            assert_eq!(
                supervisor.status("fixture").unwrap().current_main_pid,
                Some(old_pid),
                "durable active service pointer remains on v1 until v2 serves"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "successor did not serve while predecessor drained"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let pending_connection = std::thread::spawn(move || connect(address, "2.0.0"));
    existing.finish("1.0.0");
    pending_connection.join().unwrap().finish("2.0.0");
    let committed = update.join().unwrap().unwrap();

    let store = Store::open(&root).unwrap();
    assert_eq!(committed.phase, Phase::Committed);
    assert_eq!(committed.retiring_pid, None);
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
    assert_eq!(
        durable.active_pid,
        supervisor.status("fixture").unwrap().current_main_pid
    );
    assert_eq!(durable.active_version.as_deref(), Some("2.0.0"));
    assert_eq!(durable.previous_version.as_deref(), Some("1.0.0"));
    assert_eq!(
        apollo_updated::history::read(&root, "fixture").unwrap()[0].result,
        "committed"
    );
}

#[test]
fn operator_rollback_reuses_retained_release_without_lowering_verified_version() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let supervisor = std::sync::Arc::new(supervisor);
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    activate_old(&store, supervisor.as_ref());
    let allow = allow_package();
    let upgraded = install_verified(
        &store,
        supervisor.as_ref(),
        &allow,
        verified_fixture(&store, "2.0.0", None),
        Some(supervisor.listener_fd()),
    )
    .unwrap();
    let existing = connect(address, "2.0.0");
    let retiring_pid = upgraded.active_pid.unwrap();
    let rollback_supervisor = std::sync::Arc::clone(&supervisor);
    let rollback = std::thread::spawn(move || {
        rollback_previous(
            &store,
            rollback_supervisor.as_ref(),
            &allow,
            Some(rollback_supervisor.listener_fd()),
        )
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let status = supervisor.status("fixture").unwrap();
        let predecessor_draining = status
            .generations
            .iter()
            .find(|generation| generation.pid == retiring_pid)
            .is_some_and(|generation| !generation.serving);
        if predecessor_draining {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rollback did not begin draining the current generation"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    existing.finish("2.0.0");
    let rolled_back = rollback.join().unwrap().unwrap();
    let store = Store::open(&root).unwrap();

    assert_eq!(rolled_back.active_version.as_deref(), Some("1.0.0"));
    assert_eq!(rolled_back.previous_version.as_deref(), Some("2.0.0"));
    assert_eq!(
        rolled_back.highest_verified_version.as_deref(),
        Some("2.0.0")
    );
    assert_eq!(
        rolled_back.operation_kind,
        apollo_updated::state::OperationKind::Rollback
    );
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        store
            .read_pointer("fixture", "previous")
            .unwrap()
            .as_deref(),
        Some("2.0.0")
    );
    connect(address, "1.0.0").finish("1.0.0");
    let events = history::read(&root, "fixture").unwrap();
    assert_eq!(events.last().unwrap().operation, "rollback");
    assert_eq!(events.last().unwrap().result, "committed");
}

#[test]
fn unhealthy_successor_is_removed_before_the_old_generation_resumes() {
    let _serial = FIXTURE_TEST_LOCK.lock().unwrap();
    let (supervisor, address, scratch) = supervisor();
    let root = scratch.path().join("state-root");
    let store = Store::open(&root).unwrap();
    let old_pid = activate_old(&store, &supervisor);
    let existing = connect(address, "1.0.0");
    let verified = verified_fixture(&store, "2.0.0", Some(".fixture-unhealthy"));
    let package = allow_package();
    assert!(
        install_verified(
            &store,
            &supervisor,
            &package,
            verified,
            Some(supervisor.listener_fd()),
        )
        .is_err()
    );

    let status = supervisor.status("fixture").unwrap();
    assert_eq!(status.current_main_pid, Some(old_pid));
    assert_eq!(status.generations.len(), 1);
    assert!(status.generations[0].serving && status.generations[0].healthy);
    existing.finish("1.0.0");
    assert_eq!(
        store.read_pointer("fixture", "active").unwrap().as_deref(),
        Some("1.0.0")
    );
    assert_eq!(store.read_pointer("fixture", "previous").unwrap(), None);
    let state = store
        .load_state::<PackageState>("fixture")
        .unwrap()
        .unwrap();
    assert_eq!(state.phase, Phase::RolledBack);
    assert_eq!(state.active_pid, Some(old_pid));
    assert_eq!(state.candidate_pid, None);
    assert_eq!(state.handoff_previous_pid, None);

    assert!(
        install_verified(
            &store,
            &supervisor,
            &package,
            verified_fixture(&store, "3.0.0", Some(".fixture-crash-after-activate")),
            Some(supervisor.listener_fd()),
        )
        .is_err()
    );
    assert_eq!(
        supervisor.status("fixture").unwrap().current_main_pid,
        Some(old_pid)
    );
    assert_eq!(supervisor.status("fixture").unwrap().generations.len(), 1);
    connect(address, "1.0.0").finish("1.0.0");
}
