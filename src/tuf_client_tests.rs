use super::{
    SignedTargetBinding, inspect_trust_anchor, public_key_identity, validate_artifact_binding,
    validate_trust_anchor, validate_trust_anchor_excluding,
};
use crate::{
    contract::{
        ArtifactSource, Compatibility, HealthContract, ListenerMode, PackageContract, Readiness,
    },
    disk::Store,
    settings::{AllowedPackage, Settings},
    state::PackageState,
};
use chrono::{Duration, Utc};
use semver::Version;
use sha2::Digest;
use std::{collections::BTreeSet, fs, num::NonZeroU64, path::Path, process::Command};
use tough::{
    DefaultTransport, ExpirationEnforcement, RepositoryLoader, TargetName,
    editor::{RepositoryEditor, signed::PathExists},
    key_source::LocalKeySource,
    schema::Target,
};
use url::Url;

#[test]
fn artifact_target_is_bound_to_its_package_version_and_architecture() {
    let expected = SignedTargetBinding {
        package_id: "sample".into(),
        version: Version::parse("1.2.3").expect("version"),
        architecture: "x86_64-linux-gnu".into(),
        manifest_target: "packages/sample/1.2.3/x86_64-linux-gnu/manifest.json".into(),
    };
    let custom = serde_json::json!({
        "package_id":"other",
        "version":"1.2.3",
        "architecture":"x86_64-linux-gnu",
        "manifest_target":"packages/sample/1.2.3/x86_64-linux-gnu/manifest.json"
    });
    assert!(
        validate_artifact_binding(
            "packages/other/1.2.3/x86_64-linux-gnu/artifact.tar.zst",
            &custom,
            &expected,
            &expected.manifest_target
        )
        .is_err()
    );
}

#[test]
fn trust_anchor_expiry_is_reported_from_signed_root_metadata() {
    let status = inspect_trust_anchor(br#"{"signed":{"expires":"2000-01-01T00:00:00Z"}}"#)
        .expect("inspect root expiry");
    assert!(status.expired);
    assert_eq!(status.expires, "2000-01-01T00:00:00Z");
}

#[test]
fn bootstrap_root_requires_a_valid_root_role_signature() {
    let root = include_bytes!("../qualification/test-root.json");
    validate_trust_anchor(root).expect("valid qualification root");

    let mut tampered =
        serde_json::from_slice::<serde_json::Value>(root).expect("qualification root JSON");
    tampered["signatures"][0]["sig"] = serde_json::Value::String("00".repeat(256));
    let tampered = serde_json::to_vec(&tampered).expect("serialize tampered root");
    assert!(validate_trust_anchor(&tampered).is_err());
}

#[test]
fn production_root_rejects_qualification_key_even_when_json_is_reformatted() {
    let root = include_bytes!("../qualification/test-root.json");
    let mut reformatted = root.to_vec();
    reformatted.extend_from_slice(b"\n  \n");
    validate_trust_anchor(&reformatted).expect("root signature ignores outer whitespace");
    assert!(validate_trust_anchor_excluding(&reformatted, root).is_err());
}

#[test]
fn root_key_identity_normalizes_hex_case() {
    let key = |public: &str| {
        serde_json::from_value::<tough::schema::key::Key>(serde_json::json!({
            "keytype": "ed25519",
            "keyval": { "public": public },
            "scheme": "ed25519"
        }))
        .expect("valid Ed25519 public key")
    };
    let lower = "abcdef0123456789".repeat(4);
    let upper = lower.to_uppercase();
    assert_eq!(
        public_key_identity(key(&lower)),
        public_key_identity(key(&upper))
    );
}

#[tokio::test]
async fn signed_qualification_release_loads_through_the_production_verifier() {
    let scratch = tempfile::tempdir().expect("qualification scratch");
    let qualification_dir = scratch.path().join("qualification");
    let generator =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("qualification/generate_test_root.py");
    let generated = Command::new("python3")
        .arg(generator)
        .arg(&qualification_dir)
        .output()
        .expect("run qualification test-root generator");
    assert!(
        generated.status.success(),
        "qualification test-root generation failed: {}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let trusted_root = qualification_dir.join("test-root.json");
    let signing_key_path = qualification_dir.join("private/test-root-ed25519.pem");
    let source = scratch.path().join("source");
    let metadata = scratch.path().join("metadata");
    let targets = scratch.path().join("targets");
    fs::create_dir_all(&source).expect("target source");
    fs::create_dir_all(&metadata).expect("metadata output");
    fs::create_dir_all(&targets).expect("target output");

    let package = "fixture-daemon";
    let version = Version::parse("1.2.3").expect("version");
    let architecture = "aarch64-linux-gnu";
    let prefix = format!("packages/{package}/{version}/{architecture}");
    let manifest_name = format!("{prefix}/manifest.json");
    let artifact_name = format!("{prefix}/payload.tar.zst");
    let artifact_bytes = b"qualification artifact bytes";
    let digest = hex::encode(sha2::Sha256::digest(artifact_bytes));
    let manifest = PackageContract {
        package_id: package.to_owned(),
        version: version.clone(),
        architecture: architecture.to_owned(),
        artifact: ArtifactSource::Tuf {
            target: artifact_name.clone(),
        },
        sha256: digest.clone(),
        size: artifact_bytes.len() as u64,
        service_name: package.to_owned(),
        listener: ListenerMode::None,
        health: HealthContract {
            readiness: Readiness::ProcessAlive,
            stabilization_ms: 1,
            failure_threshold: 1,
        },
        drain_timeout_ms: 1000,
        health_timeout_ms: 1000,
        stabilization_ms: 1,
        compatibility: Compatibility {
            minimum_updater: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            maximum_updater: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            package_format: crate::contract::SUPPORTED_PACKAGE_FORMAT,
            state_format: crate::contract::SUPPORTED_STATE_FORMAT,
            protocol_version: crate::contract::SUPPORTED_PROTOCOL_VERSION,
        },
        coordinated_set: None,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).expect("typed manifest");
    let manifest_source = source.join(&manifest_name);
    let artifact_source = source.join(&artifact_name);
    fs::create_dir_all(manifest_source.parent().unwrap()).expect("manifest namespace");
    fs::create_dir_all(artifact_source.parent().unwrap()).expect("artifact namespace");
    fs::write(&manifest_source, &manifest_bytes).expect("manifest target");
    fs::write(&artifact_source, artifact_bytes).expect("artifact target");

    let binding = serde_json::json!({
        "package_id": package,
        "version": version,
        "architecture": architecture,
        "manifest_target": manifest_name,
    });
    let mut editor = RepositoryEditor::new(trusted_root.clone())
        .await
        .expect("TUF editor");
    for (name, path) in [
        (&manifest_name, &manifest_source),
        (&artifact_name, &artifact_source),
    ] {
        let mut target = Target::from_path(path).await.expect("target hash");
        target
            .custom
            .insert("apollo_package".to_owned(), binding.clone());
        editor
            .add_target(
                TargetName::try_from(name.as_str()).expect("safe target name"),
                target,
            )
            .expect("add target");
    }
    let expires = Utc::now() + Duration::days(30);
    editor.targets_version(NonZeroU64::new(1).unwrap()).unwrap();
    editor.targets_expires(expires).unwrap();
    editor.snapshot_version(NonZeroU64::new(1).unwrap());
    editor.snapshot_expires(expires);
    editor.timestamp_version(NonZeroU64::new(1).unwrap());
    editor.timestamp_expires(expires);
    let signing_key_pem = fs::read(&signing_key_path).expect("ephemeral qualification key");
    let signing_key_pem = pem::parse(signing_key_pem).expect("PKCS#8 PEM key");
    assert_eq!(signing_key_pem.tag(), "PRIVATE KEY");
    let signing_key = scratch.path().join("qualification-signing-key.pk8");
    fs::write(&signing_key, signing_key_pem.into_contents()).expect("write PKCS#8 signing key");
    let signed = editor
        .sign(&[Box::new(LocalKeySource { path: signing_key })])
        .await
        .expect("sign TUF metadata with qualification key");
    signed.write(&metadata).await.expect("write metadata");
    for (name, path) in [
        (&manifest_name, &manifest_source),
        (&artifact_name, &artifact_source),
    ] {
        let target_name = TargetName::try_from(name.as_str()).expect("safe target name");
        fs::create_dir_all(targets.join(Path::new(name).parent().expect("target parent")))
            .expect("target namespace");
        signed
            .copy_target(path, &targets, PathExists::Fail, Some(&target_name))
            .await
            .expect("copy signed target");
    }

    let root_bytes = fs::read(&trusted_root).expect("qualification trust root");
    let store = Store::open(&scratch.path().join("state")).expect("updater store");
    let allow = AllowedPackage {
        package_id: package.to_owned(),
        service_name: package.to_owned(),
        architecture: architecture.to_owned(),
        allow_listenerless: true,
        health_executables: BTreeSet::new(),
        health_probes: Vec::new(),
        generation: None,
    };
    let settings = Settings {
        data_root: store.root().to_path_buf(),
        socket_path: scratch.path().join("control.sock"),
        allowed_group: 0,
        max_artifact_size: 1024 * 1024,
        metadata_url: Url::from_directory_path(&metadata).expect("metadata URL"),
        targets_url: Url::from_directory_path(&targets).expect("targets URL"),
        trusted_root,
        trusted_root_bytes: root_bytes,
        packages: vec![allow.clone()],
    };
    let repository = RepositoryLoader::new(
        &settings.trusted_root_bytes,
        settings.metadata_url.clone(),
        settings.targets_url.clone(),
    )
    .transport(DefaultTransport::new())
    .datastore(store.root().join("trust"))
    .expiration_enforcement(ExpirationEnforcement::Safe)
    .load()
    .await
    .expect("signed qualification repository");
    let verified = super::verify_target(
        &repository,
        &settings,
        &store,
        &allow,
        &PackageState::initial(package),
    )
    .await
    .expect("production TUF target verifier");
    assert_eq!(verified.contract.version, version);
    assert_eq!(verified.artifact_sha256, digest);
    assert_eq!(verified.target_name, manifest_name);
    assert_eq!(fs::read(&verified.artifact_path).unwrap(), artifact_bytes);
    let reservations = store.root().join("state/reservations");
    assert_eq!(fs::read_dir(&reservations).unwrap().count(), 2);
    drop(verified);
    assert_eq!(fs::read_dir(&reservations).unwrap().count(), 1);
}
