use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub operation_id: String,
    pub package_id: String,
    pub version: Option<String>,
    pub digest: Option<String>,
    #[serde(default)]
    pub manifest_sha256: Option<String>,
    pub operation: String,
    pub result: String,
    pub timestamp_unix_ms: u128,
}

pub fn append(root: &Path, event: HistoryEvent) -> anyhow::Result<()> {
    let path = root
        .join("history")
        .join(format!("{}.jsonl", event.package_id));
    let mut options = OpenOptions::new();
    options.create(true).append(true).mode(0o600);
    let mut file = options.open(path)?;
    serde_json::to_writer(&mut file, &event)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

pub fn append_once(root: &Path, event: HistoryEvent) -> anyhow::Result<()> {
    if read(root, &event.package_id)?
        .iter()
        .any(|existing| existing.operation_id == event.operation_id)
    {
        return Ok(());
    }
    append(root, event)
}

pub fn read(root: &Path, package: &str) -> anyhow::Result<Vec<HistoryEvent>> {
    let path = root.join("history").join(format!("{package}.jsonl"));
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
            anyhow::bail!("history path is unsafe")
        }
        Ok(_) => fs::read_to_string(path)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()
            .map_err(Into::into),
    }
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis())
}
