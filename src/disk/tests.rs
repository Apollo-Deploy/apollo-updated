use super::Store;
use crate::state::PackageState;

#[test]
fn store_rejects_symlinked_ancestor_before_creating_outside_descendants() {
    let dir = tempfile::tempdir().expect("fixture root");
    let victim = dir.path().join("victim");
    let link = dir.path().join("link");
    std::fs::create_dir(&victim).expect("victim directory");
    std::os::unix::fs::symlink(&victim, &link).expect("ancestor symlink");
    assert!(Store::open(&link.join("created-by-updater")).is_err());
    assert!(!victim.join("created-by-updater").exists());
}

#[test]
fn package_pointer_rejects_escape_and_state_is_atomically_readable() {
    let dir = tempfile::tempdir().expect("fixture root");
    let store = Store::open(&dir.path().join("state-root")).expect("store");
    let package = store.package_dir("sample");
    std::fs::create_dir_all(package.join("versions/1.2.3")).expect("version tree");
    store
        .replace_pointer("sample", "active", "1.2.3")
        .expect("active pointer");
    assert_eq!(
        store
            .read_pointer("sample", "active")
            .expect("read pointer"),
        Some("1.2.3".to_owned())
    );
    let state = PackageState::initial("sample");
    store
        .save_json(&store.state_path("sample"), &state)
        .expect("durable state");
    assert_eq!(
        store
            .load_state::<PackageState>("sample")
            .expect("load state")
            .unwrap()
            .package_id,
        "sample"
    );
    std::os::unix::fs::symlink("../../outside", package.join("previous"))
        .expect("malicious pointer");
    assert!(store.read_pointer("sample", "previous").is_err());
}

#[test]
fn package_pointer_clear_is_idempotent_and_refuses_malformed_targets() {
    let dir = tempfile::tempdir().expect("fixture root");
    let store = Store::open(&dir.path().join("state-root")).expect("store");
    let package = store.package_dir("sample");
    std::fs::create_dir_all(package.join("versions/1.2.3")).expect("version tree");
    store
        .replace_pointer("sample", "active", "1.2.3")
        .expect("active pointer");
    store
        .clear_pointer("sample", "active")
        .expect("clear active pointer");
    store
        .clear_pointer("sample", "active")
        .expect("clear absent pointer");
    assert_eq!(store.read_pointer("sample", "active").unwrap(), None);

    std::os::unix::fs::symlink("versions/..", package.join("active")).expect("malformed pointer");
    assert!(store.clear_pointer("sample", "active").is_err());
    assert!(std::fs::symlink_metadata(package.join("active")).is_ok());
}

#[tokio::test]
async fn size_failure_removes_partial_target() {
    let dir = tempfile::tempdir().expect("fixture root");
    let store = Store::open(&dir.path().join("state-root")).expect("store");
    let sink = store
        .artifact_sink("sample", "1.0.0", 4)
        .expect("bounded sink");
    let path = sink.path.clone();
    sink.send(vec![1, 2, 3, 4, 5])
        .await
        .expect("bounded channel accepts chunk");
    assert!(sink.finish().await.is_err());
    assert!(!path.exists());
}

#[tokio::test]
async fn completed_verification_can_remove_its_owned_staging_tree() {
    let dir = tempfile::tempdir().expect("fixture root");
    let store = Store::open(&dir.path().join("state-root")).expect("store");
    let sink = store
        .artifact_sink("sample", "1.0.0", 16)
        .expect("artifact sink");
    sink.send(b"verified".to_vec())
        .await
        .expect("write artifact");
    let (path, _, _) = sink.finish().await.expect("finish artifact");
    assert!(path.exists());
    store
        .discard_staged("sample", "1.0.0")
        .expect("remove owned staging");
    assert!(!path.exists());
    assert!(!path.parent().expect("staging version").exists());
}
