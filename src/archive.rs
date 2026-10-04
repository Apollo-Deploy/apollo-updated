use crate::error::UpdateError;
use anyhow::{Context, bail};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path},
};

pub(super) fn extract_confined(archive: &Path, dest: &Path, max_total: u64) -> anyhow::Result<()> {
    const MAX_MEMBERS: usize = 10_000;
    let decoder = zstd::stream::read::Decoder::new(File::open(archive)?)?;
    let mut tar = tar::Archive::new(decoder);
    let mut total = 0_u64;
    let mut members = 0_usize;
    for item in tar.entries()? {
        members = members.checked_add(1).ok_or(UpdateError::ArtifactLimit)?;
        if members > MAX_MEMBERS {
            return Err(UpdateError::ArtifactLimit.into());
        }
        let mut entry = item?;
        let path = entry.path()?.into_owned();
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(UpdateError::UnsafeArchive.into());
        }
        let output = dest.join(&path);
        if !output.starts_with(dest) {
            return Err(UpdateError::UnsafeArchive.into());
        }
        if entry.header().entry_type().is_dir() {
            create_tree_dirs(&output, dest)?;
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(UpdateError::UnsafeArchive.into());
        }
        total = total
            .checked_add(entry.size())
            .ok_or(UpdateError::ArtifactLimit)?;
        if total > max_total {
            return Err(UpdateError::ArtifactLimit.into());
        }
        let parent = output.parent().context("archive member has no parent")?;
        create_tree_dirs(parent, dest)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .open(&output)?;
        std::io::copy(&mut entry, &mut file)?;
        let mode = entry.header().mode().unwrap_or(0o400) & 0o555;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
    }
    sync_tree(dest)
}

fn create_tree_dirs(path: &Path, root: &Path) -> anyhow::Result<()> {
    let relative = path.strip_prefix(root)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(UpdateError::UnsafeArchive.into());
        };
        current.push(part);
        match fs::create_dir(&current) {
            Ok(()) => fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&current)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    bail!("unsafe archive directory");
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sync_tree(path: &Path) -> anyhow::Result<()> {
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(UpdateError::UnsafeArchive.into());
        }
        if metadata.is_file() || metadata.is_dir() {
            File::open(entry.path())?.sync_all()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::extract_confined;
    use std::fs::File;

    fn archive_with(
        path: &str,
        body: &[u8],
    ) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temporary fixture directory");
        let archive = dir.path().join("fixture.tar.zst");
        let output = File::create(&archive).expect("archive output");
        let encoder = zstd::stream::write::Encoder::new(output, 0).expect("zstd encoder");
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Regular);
        let name = path.as_bytes();
        assert!(name.len() < 100);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_cksum();
        builder
            .append(&mut header, body)
            .expect("append fixture entry");
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish zstd");
        let destination = dir.path().join("payload");
        std::fs::create_dir(&destination).expect("payload root");
        (dir, archive, destination)
    }

    #[test]
    fn traversal_archive_is_rejected_before_writing_outside_destination() {
        let (dir, archive, destination) = archive_with("../../escaped", b"owned");
        let result = extract_confined(&archive, &destination, 1024);
        assert!(result.is_err());
        assert!(!dir.path().join("escaped").exists());
    }

    #[test]
    fn ordinary_files_extract_with_no_write_permission() {
        let (_dir, archive, destination) = archive_with("bin/service", b"fixture");
        extract_confined(&archive, &destination, 1024).expect("valid archive");
        assert_eq!(
            std::fs::read(destination.join("bin/service")).expect("payload"),
            b"fixture"
        );
        assert_eq!(
            std::fs::metadata(destination.join("bin/service"))
                .expect("mode")
                .permissions()
                .mode()
                & 0o222,
            0
        );
    }

    #[test]
    fn archive_member_count_is_bounded() {
        let dir = tempfile::tempdir().expect("temporary fixture directory");
        let archive = dir.path().join("many-members.tar.zst");
        let output = File::create(&archive).expect("archive output");
        let encoder = zstd::stream::write::Encoder::new(output, 0).expect("zstd encoder");
        let mut builder = tar::Builder::new(encoder);
        for _ in 0..=10_000 {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_cksum();
            builder
                .append_data(&mut header, "repeated-directory", &[][..])
                .expect("append directory entry");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish zstd");
        let destination = dir.path().join("payload");
        std::fs::create_dir(&destination).expect("payload root");
        assert!(extract_confined(&archive, &destination, 1024).is_err());
    }

    use std::os::unix::fs::PermissionsExt;
}
