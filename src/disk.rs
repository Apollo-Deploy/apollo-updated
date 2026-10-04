use crate::error::UpdateError;
use anyhow::{Context, bail};
use fs2::FileExt;
use nix::{
    errno::Errno,
    fcntl::{OFlag, open, openat},
    sys::stat::{Mode, fchmod, fstat, mkdirat},
    unistd::geteuid,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::sync::mpsc;

mod space;
pub use space::SpaceReservation;

#[cfg(test)]
#[path = "disk/tests.rs"]
mod tests;

pub struct ArtifactSink {
    pub path: PathBuf,
    sender: Option<mpsc::Sender<Vec<u8>>>,
    worker: Option<tokio::task::JoinHandle<anyhow::Result<(u64, String)>>>,
    finished: bool,
}

impl ArtifactSink {
    pub async fn send(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.sender
            .as_ref()
            .context("artifact writer is already closed")?
            .send(bytes)
            .await
            .map_err(|_| anyhow::anyhow!("artifact writer stopped"))
    }

    pub async fn finish(mut self) -> anyhow::Result<(PathBuf, u64, String)> {
        self.sender.take();
        let worker = self
            .worker
            .take()
            .context("artifact writer was already joined")?;
        match worker.await? {
            Ok((length, digest)) => {
                self.finished = true;
                Ok((self.path.clone(), length, digest))
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for ArtifactSink {
    fn drop(&mut self) {
        if !self.finished {
            if let Some(worker) = self.worker.take() {
                worker.abort();
            }
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if !path.is_absolute() {
            bail!("state root must be an absolute path");
        }
        let canonical = create_root_without_symlinks(path)?;
        for child in ["packages", "staging", "state", "history", "trust"] {
            create_dir(&canonical.join(child))?;
        }
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn package_dir(&self, package: &str) -> PathBuf {
        self.root.join("packages").join(package)
    }

    pub fn state_path(&self, package: &str) -> PathBuf {
        self.root.join("state").join(format!("{package}.json"))
    }

    pub fn load_state<T: DeserializeOwned>(&self, package: &str) -> anyhow::Result<Option<T>> {
        let path = self.state_path(package);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
            Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
                Err(UpdateError::StateCorrupt.into())
            }
            Ok(_) => serde_json::from_slice(&fs::read(path)?)
                .map(Some)
                .map_err(Into::into),
        }
    }

    pub fn save_json<T: Serialize>(&self, path: &Path, value: &T) -> anyhow::Result<()> {
        self.require_owned_path(path)?;
        let parent = path.parent().context("file has no parent")?;
        create_owned_dirs(&self.root, parent)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let temp = unique_temp(parent);
        let mut file = options.open(&temp)?;
        let result = (|| {
            serde_json::to_writer(&mut file, value)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            File::open(parent)?.sync_all()?;
            Ok::<_, anyhow::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    pub fn artifact_sink(
        &self,
        package: &str,
        version: &str,
        max_size: u64,
    ) -> anyhow::Result<ArtifactSink> {
        let dir = self.root.join("staging").join(package).join(version);
        create_owned_dirs(&self.root, &dir)?;
        let path = dir.join("target.part");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(4);
        let worker = tokio::task::spawn_blocking(move || {
            let mut file = file;
            let mut hash = Sha256::new();
            let mut total = 0_u64;
            while let Some(bytes) = receiver.blocking_recv() {
                total = total
                    .checked_add(bytes.len() as u64)
                    .ok_or(UpdateError::ArtifactLimit)?;
                if total > max_size {
                    return Err(UpdateError::ArtifactLimit.into());
                }
                hash.update(&bytes);
                file.write_all(&bytes)?;
            }
            file.sync_all()?;
            Ok((total, hex::encode(hash.finalize())))
        });
        Ok(ArtifactSink {
            path,
            sender: Some(sender),
            worker: Some(worker),
            finished: false,
        })
    }

    pub fn discard_staged(&self, package: &str, version: &str) -> anyhow::Result<()> {
        if !safe_component(package) || !safe_component(version) {
            bail!("invalid staging path component");
        }
        let staging = self.root.join("staging");
        let package_dir = staging.join(package);
        let version_dir = package_dir.join(version);
        for directory in [&package_dir, &version_dir] {
            match fs::symlink_metadata(directory) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                    return Err(UpdateError::StateCorrupt.into());
                }
                Ok(_) => {}
            }
        }
        for entry in walkdir::WalkDir::new(&version_dir).follow_links(false) {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(UpdateError::StateCorrupt.into());
            }
        }
        fs::remove_dir_all(&version_dir)?;
        File::open(&package_dir)?.sync_all()?;
        match fs::remove_dir(&package_dir) {
            Ok(()) => File::open(&staging)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    pub fn reserve_download_space(
        &self,
        package: &str,
        artifact_size: u64,
        extraction_limit: u64,
    ) -> anyhow::Result<SpaceReservation> {
        space::reserve_download_space(self, package, artifact_size, extraction_limit)
    }

    pub fn lock_package(&self, package: &str) -> anyhow::Result<File> {
        let locks = self.root.join("state").join("locks");
        create_dir(&locks)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(locks.join(format!("{package}.lock")))?;
        file.lock_exclusive()?;
        Ok(file)
    }

    pub fn promote(
        &self,
        package: &str,
        version: &str,
        staged: &Path,
        digest: &str,
        manifest: &[u8],
    ) -> anyhow::Result<PathBuf> {
        if !safe_component(package)
            || !safe_component(version)
            || digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("invalid immutable package path or digest");
        }
        self.require_owned_path(staged)?;
        let staged_meta = fs::symlink_metadata(staged)?;
        if !staged_meta.is_file() || staged_meta.file_type().is_symlink() {
            bail!("staged artifact is not a regular updater-owned file");
        }
        let package_dir = self.package_dir(package);
        create_dir(&package_dir)?;
        let versions = package_dir.join("versions");
        create_dir(&versions)?;
        let final_dir = versions.join(version);
        if final_dir.exists() {
            bail!("immutable version already exists");
        }
        let stage_dir = staged.parent().context("staged target has no parent")?;
        let extracting = stage_dir.join("payload");
        create_dir(&extracting)?;
        crate::archive::extract_confined(staged, &extracting, 8 * 1024 * 1024 * 1024)?;
        write_new(&stage_dir.join("manifest.json"), manifest)?;
        write_new(&stage_dir.join("digest.sha256"), digest.as_bytes())?;
        fs::rename(&extracting, stage_dir.join("payload.tree"))?;
        File::open(stage_dir)?.sync_all()?;
        fs::rename(stage_dir, &final_dir)?;
        File::open(&versions)?.sync_all()?;
        Ok(final_dir)
    }

    pub fn replace_pointer(&self, package: &str, name: &str, version: &str) -> anyhow::Result<()> {
        if !safe_component(package)
            || !safe_component(version)
            || !matches!(name, "active" | "previous")
        {
            bail!("invalid package pointer path");
        }
        let base = self.package_dir(package);
        create_owned_dirs(&self.root, &base)?;
        let version_dir = base.join("versions").join(version);
        let version_meta = fs::symlink_metadata(&version_dir)?;
        if !version_meta.is_dir() || version_meta.file_type().is_symlink() {
            bail!("package pointer target is not an immutable version directory");
        }
        let target = format!("versions/{version}");
        let temp = base.join(format!(".{name}.{}", uuid::Uuid::new_v4()));
        std::os::unix::fs::symlink(target, &temp)?;
        fs::rename(&temp, base.join(name))?;
        File::open(&base)?.sync_all()?;
        Ok(())
    }

    pub fn clear_pointer(&self, package: &str, name: &str) -> anyhow::Result<()> {
        if !safe_component(package) || !matches!(name, "active" | "previous") {
            bail!("invalid package pointer path");
        }
        let base = self.package_dir(package);
        create_owned_dirs(&self.root, &base)?;
        let path = base.join(name);
        self.require_owned_path(&path)?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(metadata) if !metadata.file_type().is_symlink() => {
                bail!("refusing to clear a non-symlink package pointer");
            }
            Ok(_) => {}
        }
        // Validate the link target before unlinking it. `remove_file` unlinks the pointer itself
        // and never follows it, so neither the target nor any foreign path can be deleted.
        self.read_pointer(package, name)?;
        fs::remove_file(path)?;
        File::open(base)?.sync_all()?;
        Ok(())
    }

    pub fn read_pointer(&self, package: &str, name: &str) -> anyhow::Result<Option<String>> {
        if !safe_component(package) || !matches!(name, "active" | "previous") {
            bail!("invalid package pointer path");
        }
        let path = self.package_dir(package).join(name);
        let target = match fs::read_link(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(target) => target,
        };
        let prefix = Path::new("versions");
        let mut parts = target.components();
        if parts.next() != Some(std::path::Component::Normal("versions".as_ref())) {
            return Err(UpdateError::StateCorrupt.into());
        }
        let version = parts
            .next()
            .context("invalid pointer")?
            .as_os_str()
            .to_string_lossy()
            .into_owned();
        if parts.next().is_some() || !safe_component(&version) {
            return Err(UpdateError::StateCorrupt.into());
        }
        let version_dir = self.package_dir(package).join(prefix).join(&version);
        let meta = fs::symlink_metadata(version_dir)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(UpdateError::StateCorrupt.into());
        }
        Ok(Some(version))
    }

    fn require_owned_path(&self, path: &Path) -> anyhow::Result<()> {
        if !path.starts_with(&self.root) || path == self.root {
            bail!("path is outside updater root");
        }
        let parent = path.parent().context("invalid path")?;
        let canonical_parent = fs::canonicalize(parent)?;
        if !canonical_parent.starts_with(&self.root) || canonical_parent != parent {
            bail!("symlink traversal in updater path");
        }
        Ok(())
    }
}

fn create_root_without_symlinks(path: &Path) -> anyhow::Result<PathBuf> {
    let mut current = open(
        "/",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    let components = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::RootDir => None,
            std::path::Component::Normal(name) => Some(Ok(name.to_owned())),
            _ => Some(Err(anyhow::anyhow!("unsafe state root component"))),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if components.is_empty() {
        bail!("state root cannot be the filesystem root");
    }
    for (index, component) in components.iter().enumerate() {
        let child = match openat(
            &current,
            component.as_os_str(),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(child) => child,
            Err(Errno::ENOENT) => {
                match mkdirat(
                    &current,
                    component.as_os_str(),
                    Mode::from_bits_truncate(0o700),
                ) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                openat(
                    &current,
                    component.as_os_str(),
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                    Mode::empty(),
                )?
            }
            Err(error) => return Err(error.into()),
        };
        let stat = fstat(&child)?;
        if stat.st_mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32 {
            bail!("state root contains a non-directory component");
        }
        if index + 1 == components.len() {
            if stat.st_uid != geteuid().as_raw() {
                bail!("state root must be owned by the updater user");
            }
            fchmod(&child, Mode::from_bits_truncate(0o700))?;
        }
        current = child;
    }
    Ok(path.to_path_buf())
}

fn create_dir(path: &Path) -> anyhow::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => set_private_dir(path),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path)?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                bail!("unsafe updater directory");
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
}

fn set_private_dir(path: &Path) -> anyhow::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn create_owned_dirs(root: &Path, path: &Path) -> anyhow::Result<()> {
    let relative = path
        .strip_prefix(root)
        .context("directory is outside updater root")?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            bail!("unsafe updater directory path");
        };
        current.push(name);
        create_dir(&current)?;
    }
    Ok(())
}

fn unique_temp(dir: &Path) -> PathBuf {
    dir.join(format!(
        ".write-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

fn write_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
