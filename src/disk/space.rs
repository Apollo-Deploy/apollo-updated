use crate::error::UpdateError;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

#[derive(Debug)]
pub struct SpaceReservation {
    path: std::path::PathBuf,
    _file: File,
}

impl Drop for SpaceReservation {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) fn reserve_download_space(
    store: &super::Store,
    package: &str,
    artifact_size: u64,
    extraction_limit: u64,
) -> anyhow::Result<SpaceReservation> {
    let versions = store.package_dir(package).join("versions");
    let mut retained = 0_u64;
    match fs::symlink_metadata(&versions) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
            return Err(UpdateError::StateCorrupt.into());
        }
        Ok(_) => {
            for entry in fs::read_dir(&versions)? {
                let entry = entry?;
                let metadata = fs::symlink_metadata(entry.path())?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(UpdateError::StateCorrupt.into());
                }
                retained = retained
                    .checked_add(tree_size(&entry.path())?)
                    .ok_or(UpdateError::InsufficientDisk)?;
            }
        }
    }
    let required = artifact_size
        .checked_add(extraction_limit)
        .and_then(|needed| needed.checked_add(retained))
        .ok_or(UpdateError::InsufficientDisk)?;

    let reservations = store.root().join("state").join("reservations");
    super::create_dir(&reservations)?;
    let global_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(reservations.join(".lock"))?;
    global_lock.lock_exclusive()?;
    let already_reserved = active_reservations(&reservations)?;
    if fs2::available_space(store.root())?.saturating_sub(already_reserved) < required {
        return Err(UpdateError::InsufficientDisk.into());
    }
    let path = reservations.join(format!("{}.bytes", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.lock_exclusive()?;
    file.write_all(required.to_string().as_bytes())?;
    file.sync_all()?;
    File::open(&reservations)?.sync_all()?;
    drop(global_lock);
    Ok(SpaceReservation { path, _file: file })
}

fn tree_size(root: &Path) -> anyhow::Result<u64> {
    let mut total = 0_u64;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(UpdateError::StateCorrupt.into());
        }
        if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or(UpdateError::InsufficientDisk)?;
        }
    }
    Ok(total)
}

fn active_reservations(directory: &Path) -> anyhow::Result<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() == ".lock" {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(UpdateError::StateCorrupt.into());
        }
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                drop(file);
                fs::remove_file(path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let bytes = fs::read(path)?;
                if bytes.is_empty() || bytes.len() > 20 {
                    return Err(UpdateError::StateCorrupt.into());
                }
                let value = std::str::from_utf8(&bytes)?.parse::<u64>()?;
                total = total
                    .checked_add(value)
                    .ok_or(UpdateError::InsufficientDisk)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::super::Store;

    #[test]
    fn overflowing_disk_reservation_is_rejected() {
        let dir = tempfile::tempdir().expect("fixture root");
        let store = Store::open(&dir.path().join("state-root")).expect("store");
        assert!(store.reserve_download_space("sample", u64::MAX, 1).is_err());
    }

    #[test]
    fn concurrent_package_reservations_share_one_disk_budget() {
        let dir = tempfile::tempdir().expect("fixture root");
        let store = Store::open(&dir.path().join("state-root")).expect("store");
        let half = fs2::available_space(store.root()).expect("available space") / 2 + 1;
        let first = store
            .reserve_download_space("first", half, 0)
            .expect("first reservation");
        assert!(store.reserve_download_space("second", half, 0).is_err());
        drop(first);
        assert!(store.reserve_download_space("second", half, 0).is_ok());
    }
}
