use crate::{
    disk::Store,
    history::{self, HistoryEvent},
};
use sha2::{Digest, Sha256};

pub(super) fn append(
    store: &Store,
    operation_id: String,
    package_id: &str,
    version: String,
    digest: String,
    manifest: &[u8],
    operation: &str,
    cleanup_pending: bool,
) -> anyhow::Result<()> {
    history::append_once(
        store.root(),
        HistoryEvent {
            operation_id,
            package_id: package_id.to_owned(),
            version: Some(version),
            digest: Some(digest),
            manifest_sha256: Some(hex::encode(Sha256::digest(manifest))),
            operation: operation.to_owned(),
            result: if cleanup_pending {
                "committed_cleanup_pending"
            } else {
                "committed"
            }
            .into(),
            timestamp_unix_ms: history::now_ms(),
        },
    )
}
