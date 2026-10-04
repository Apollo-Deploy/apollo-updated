use apollo_updated::{
    contract::{
        ArtifactSource, Compatibility, HealthContract, ListenerMode, PackageContract, Readiness,
        SUPPORTED_PACKAGE_FORMAT, SUPPORTED_PROTOCOL_VERSION, SUPPORTED_STATE_FORMAT,
    },
    disk::Store,
    lifecycle::install_verified,
    settings::Settings,
    supervisor::{Supervisor, SystemdSupervisor},
    tuf_client::VerifiedPackage,
};
use semver::Version;
use sha2::Digest;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    time::{Duration, Instant},
};

const FIXTURE: &str = env!("CARGO_BIN_EXE_apollo-updated-fixture");
const SERVICE: &str = "apollo-updated-qualification-fixture";

fn fixture_binary() -> std::path::PathBuf {
    std::env::var_os("APOLLO_UPDATED_FIXTURE_BINARY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| FIXTURE.into())
}

struct Connection {
    stream: BufReader<TcpStream>,
    accepted: bool,
}

impl Connection {
    fn await_accept(&mut self, version: &str) {
        let mut line = String::new();
        self.stream.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), format!("accepted:{version}"));
        self.accepted = true;
    }

    fn finish(mut self, version: &str) {
        if !self.accepted {
            self.await_accept(version);
        }
        self.stream.get_mut().write_all(b"finish\n").unwrap();
        let mut line = String::new();
        self.stream.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), format!("finished:{version}"));
    }
}

fn connect(address: SocketAddr, version: &str) -> Connection {
    let mut connection = connect_pending(address);
    connection.await_accept(version);
    connection
}

fn connect_pending(address: SocketAddr) -> Connection {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    Connection {
        stream: BufReader::new(stream),
        accepted: false,
    }
}

fn verified_fixture(store: &Store, version: &str) -> VerifiedPackage {
    let artifact_dir = store.root().join("staging/fixture").join(version);
    fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_path = artifact_dir.join("target.part");
    let encoder =
        zstd::stream::write::Encoder::new(fs::File::create(&artifact_path).unwrap(), 0).unwrap();
    let mut archive = tar::Builder::new(encoder);
    let fixture_bytes = fs::read(fixture_binary()).unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_size(fixture_bytes.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    archive
        .append_data(&mut header, "fixture-daemon", fixture_bytes.as_slice())
        .unwrap();
    archive.into_inner().unwrap().finish().unwrap();
    let artifact = fs::read(&artifact_path).unwrap();
    let digest = hex::encode(sha2::Sha256::digest(&artifact));
    let contract = PackageContract {
        package_id: "fixture".into(),
        version: Version::parse(version).unwrap(),
        architecture: "aarch64-unknown-linux-gnu".into(),
        artifact: ArtifactSource::Tuf {
            target: format!("packages/fixture/{version}/aarch64-unknown-linux-gnu/payload.tar.zst"),
        },
        sha256: digest.clone(),
        size: artifact.len() as u64,
        service_name: SERVICE.into(),
        listener: ListenerMode::InheritedFd,
        health: HealthContract {
            readiness: Readiness::ProcessAlive,
            stabilization_ms: 1000,
            failure_threshold: 1,
        },
        drain_timeout_ms: 5000,
        health_timeout_ms: 2500,
        stabilization_ms: 1000,
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
    let reservation = store
        .reserve_download_space("fixture", artifact.len() as u64, 0)
        .unwrap();
    VerifiedPackage {
        contract,
        manifest,
        target_name: format!("packages/fixture/{version}/aarch64-unknown-linux-gnu/manifest.json"),
        artifact_path,
        artifact_sha256: digest,
        _space_reservation: Some(reservation),
    }
}

fn wait_for_old_to_stop_accepting(supervisor: &SystemdSupervisor) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = supervisor.status(SERVICE).unwrap();
        let old_is_drained = status.generations.iter().any(|generation| {
            generation.version.as_deref() == Some("1.0.0")
                && generation.healthy
                && !generation.serving
        });
        let candidate_is_ready_and_serving = status.generations.iter().any(|generation| {
            generation.version.as_deref() == Some("2.0.0")
                && generation.healthy
                && generation.serving
        });
        if old_is_drained && candidate_is_ready_and_serving {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "generations did not overlap and drain: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_overlapping_acceptors(supervisor: &SystemdSupervisor) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = supervisor.status(SERVICE).unwrap();
        let old_serving = status.generations.iter().any(|generation| {
            generation.version.as_deref() == Some("1.0.0")
                && generation.healthy
                && generation.serving
        });
        let candidate_serving = status.generations.iter().any(|generation| {
            generation.version.as_deref() == Some("2.0.0")
                && generation.healthy
                && generation.serving
        });
        if old_serving && candidate_serving {
            assert!(status.listener_owner.is_none());
            return;
        }
        assert!(
            Instant::now() < deadline,
            "successor did not become ready while predecessor served: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn systemd_socket_handoff_preserves_listener_and_open_connection() {
    if std::env::var("APOLLO_UPDATED_SYSTEMD_FIXTURE").as_deref() != Ok("1") {
        eprintln!("skipping real systemd qualification outside its disposable VM");
        return;
    }
    let updater_uid = nix::unistd::User::from_name("apollo-updated")
        .unwrap()
        .expect("updater account")
        .uid;
    assert_eq!(nix::unistd::geteuid(), updater_uid);
    assert_eq!(nix::unistd::getuid(), updater_uid);
    let address: SocketAddr = std::env::var("APOLLO_UPDATED_FIXTURE_ADDRESS")
        .expect("qualification address")
        .parse()
        .unwrap();
    let settings = Settings::load().unwrap();
    let package = settings
        .package("fixture")
        .expect("fixture allowlist")
        .clone();
    let store = Store::open(&settings.data_root).unwrap();
    let supervisor = SystemdSupervisor;

    install_verified(
        &store,
        &supervisor,
        &package,
        verified_fixture(&store, "1.0.0"),
        None,
    )
    .unwrap();
    let old_status = supervisor.status(SERVICE).unwrap();
    let old_pid = old_status.current_main_pid.unwrap();
    assert_eq!(old_status.listener_owner, Some(old_pid));
    let existing = connect(address, "1.0.0");

    let update_data_root = settings.data_root.clone();
    let update_package = package;
    let update = std::thread::spawn(move || {
        let update_store = Store::open(&update_data_root).unwrap();
        install_verified(
            &update_store,
            &supervisor,
            &update_package,
            verified_fixture(&update_store, "2.0.0"),
            None,
        )
    });
    wait_for_overlapping_acceptors(&SystemdSupervisor);
    wait_for_old_to_stop_accepting(&SystemdSupervisor);
    let queued = connect_pending(address);
    existing.finish("1.0.0");
    let committed = update.join().unwrap().unwrap_or_else(|error| {
        let status = SystemdSupervisor.status(SERVICE).ok();
        panic!("systemd handoff update failed: {error:#}; status: {status:?}");
    });
    assert_eq!(committed.active_version.as_deref(), Some("2.0.0"));
    queued.finish("2.0.0");

    let final_status = SystemdSupervisor.status(SERVICE).unwrap();
    let candidate_pid = final_status.current_main_pid.unwrap();
    assert_ne!(candidate_pid, old_pid);
    assert_eq!(final_status.listener_owner, Some(candidate_pid));
    assert!(final_status.generations.iter().any(|generation| {
        generation.pid == candidate_pid
            && generation.version.as_deref() == Some("2.0.0")
            && generation.healthy
            && generation.serving
    }));
    connect(address, "2.0.0").finish("2.0.0");
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
    assert!(Path::new(&format!("/etc/systemd/system/{SERVICE}.service")).is_symlink());
}
