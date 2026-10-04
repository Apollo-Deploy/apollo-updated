use crate::contract::Readiness;
use anyhow::Context;
use nix::{
    fcntl::{OFlag, open},
    sys::stat::fstat,
    unistd::{Group, User},
};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use url::Url;

#[path = "settings/generation.rs"]
mod generation;
pub use generation::GenerationPolicy;

#[derive(Debug, Clone)]
pub struct Settings {
    pub data_root: PathBuf,
    pub socket_path: PathBuf,
    pub allowed_group: u32,
    pub max_artifact_size: u64,
    pub metadata_url: Url,
    pub targets_url: Url,
    pub trusted_root: PathBuf,
    pub(crate) trusted_root_bytes: Vec<u8>,
    pub packages: Vec<AllowedPackage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    data_root: PathBuf,
    socket_path: PathBuf,
    allowed_group: GroupSelector,
    max_artifact_size: u64,
    metadata_url: Url,
    targets_url: Url,
    trusted_root: PathBuf,
    packages: Vec<AllowedPackage>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GroupSelector {
    Id(u32),
    Name(String),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedPackage {
    pub package_id: String,
    pub service_name: String,
    pub architecture: String,
    #[serde(default)]
    pub allow_listenerless: bool,
    #[serde(default)]
    pub health_executables: BTreeSet<PathBuf>,
    /// Exact probes approved by the root-owned local package policy.
    #[serde(default)]
    pub health_probes: Vec<Readiness>,
    #[serde(default)]
    pub generation: Option<GenerationPolicy>,
}

impl AllowedPackage {
    pub fn allows_readiness(&self, readiness: &Readiness) -> bool {
        match readiness {
            Readiness::ProcessAlive | Readiness::SupervisorReady => true,
            Readiness::UnixProbe { .. } => self.health_probes.contains(readiness),
            Readiness::AllowlistedExecutable { path, .. } => {
                self.health_executables.contains(path) && self.health_probes.contains(readiness)
            }
        }
    }
}

impl Settings {
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var_os("APOLLO_UPDATED_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/apollo-updated/config.toml"));
        if !path.is_absolute() {
            anyhow::bail!("configuration path must be absolute");
        }
        validate_protected_parent(&path)?;
        let config_bytes = read_admin_file(&path, 1024 * 1024, None)?;
        let config_text = std::str::from_utf8(&config_bytes)?;
        let config: SettingsFile = toml::from_str(config_text)?;
        if !config.data_root.is_absolute() || !config.trusted_root.is_absolute() {
            anyhow::bail!("state root and trust anchor paths must be absolute");
        }
        if config.trusted_root.starts_with(&config.data_root) {
            anyhow::bail!("trusted root must be outside the updater-owned package tree");
        }
        validate_protected_parent(&config.data_root)?;
        validate_protected_parent(&config.trusted_root)?;
        let trusted_root_bytes = read_admin_file(&config.trusted_root, 1024 * 1024, Some(0o644))?;
        crate::tuf_client::validate_trust_anchor(&trusted_root_bytes)?;
        let allowed_group = match config.allowed_group {
            GroupSelector::Id(gid) => gid,
            GroupSelector::Name(name) => resolve_group(&name)?,
        };
        let config = Self {
            data_root: config.data_root,
            socket_path: config.socket_path,
            allowed_group,
            max_artifact_size: config.max_artifact_size,
            metadata_url: config.metadata_url,
            targets_url: config.targets_url,
            trusted_root: config.trusted_root,
            trusted_root_bytes,
            packages: config.packages,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if !self.data_root.is_absolute()
            || !self.socket_path.is_absolute()
            || !self.trusted_root.is_absolute()
            || self.max_artifact_size == 0
            || self.max_artifact_size > 64 * 1024 * 1024 * 1024
        {
            anyhow::bail!("invalid updater path or artifact limit");
        }
        for url in [&self.metadata_url, &self.targets_url] {
            if !matches!(url.scheme(), "https" | "file")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                anyhow::bail!("TUF URLs must use HTTPS or file URLs without credentials");
            }
        }
        if self.packages.is_empty() {
            anyhow::bail!("package allowlist is empty");
        }
        let mut seen = BTreeSet::new();
        let mut seen_services = BTreeSet::new();
        let mut seen_listener_units = BTreeSet::new();
        let updater_uid = User::from_name("apollo-updated")?
            .context("dedicated updater account is missing")?
            .uid
            .as_raw();
        for package in &self.packages {
            if package.package_id.is_empty()
                || !package
                    .package_id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
                || package.service_name.is_empty()
                || !package
                    .service_name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'@'))
                || package.architecture.is_empty()
                || !seen.insert(package.package_id.clone())
                || !seen_services.insert(package.service_name.clone())
                || package
                    .health_executables
                    .iter()
                    .any(|path| !path.is_absolute())
                || package.health_probes.len() > 32
                || package.health_probes.iter().any(|probe| match probe {
                    Readiness::ProcessAlive | Readiness::SupervisorReady => false,
                    Readiness::UnixProbe {
                        socket,
                        request,
                        expected,
                        timeout_ms,
                    } => {
                        !socket.is_absolute()
                            || *socket == self.socket_path
                            || request.len() > 4096
                            || expected.len() > 4096
                            || *timeout_ms == 0
                    }
                    Readiness::AllowlistedExecutable {
                        path,
                        argv,
                        timeout_ms,
                        ..
                    } => {
                        !path.is_absolute()
                            || !package.health_executables.contains(path)
                            || argv.len() > 32
                            || argv.iter().any(|arg| arg.len() > 4096)
                            || *timeout_ms == 0
                    }
                })
            {
                anyhow::bail!("invalid or duplicate package allowlist entry");
            }
            if let Some(generation) = &package.generation {
                generation.validate()?;
                if let Some(unit) = &generation.socket_unit
                    && !seen_listener_units.insert(unit.clone())
                {
                    anyhow::bail!("duplicate package listener socket unit");
                }
                if generation.runtime_uid == updater_uid
                    || generation.runtime_gid == self.allowed_group
                {
                    anyhow::bail!("package runtime identity overlaps updater authorization");
                }
            }
        }
        Ok(())
    }

    pub fn package(&self, id: &str) -> Option<&AllowedPackage> {
        self.packages
            .iter()
            .find(|package| package.package_id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::{AllowedPackage, read_admin_file, resolve_group, validate_protected_parent};
    use crate::contract::Readiness;
    use std::path::Path;
    use std::{collections::BTreeSet, path::PathBuf};

    #[test]
    fn group_selector_accepts_numeric_id_and_system_group_name() {
        assert_eq!(resolve_group("995").expect("numeric group id"), 995);
        assert_eq!(resolve_group("root").expect("root group name"), 0);
    }

    #[test]
    fn configured_data_roots_cannot_use_world_writable_ancestors() {
        assert!(validate_protected_parent(Path::new("/tmp/updater-data")).is_err());
    }

    #[test]
    fn root_anchor_open_rejects_a_symlink() {
        let dir = tempfile::tempdir().expect("fixture root");
        let target = dir.path().join("target.json");
        let link = dir.path().join("root.json");
        std::fs::write(&target, b"{}").expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("root symlink");
        assert!(read_admin_file(&link, 1024, None).is_err());
    }

    #[test]
    fn health_policy_binds_probe_bytes_and_executable_arguments() {
        let executable = PathBuf::from("/usr/libexec/apollo-probe");
        let approved = Readiness::AllowlistedExecutable {
            path: executable.clone(),
            argv: vec!["--ready".into()],
            timeout_ms: 500,
            expected_exit: 0,
        };
        let socket_probe = Readiness::UnixProbe {
            socket: PathBuf::from("/run/example/health.sock"),
            request: b"health\n".to_vec(),
            expected: b"ready\n".to_vec(),
            timeout_ms: 500,
        };
        let package = AllowedPackage {
            package_id: "sample".into(),
            service_name: "sample".into(),
            architecture: "x86_64-linux-gnu".into(),
            allow_listenerless: false,
            health_executables: BTreeSet::from([executable.clone()]),
            health_probes: vec![approved.clone(), socket_probe.clone()],
            generation: None,
        };

        assert!(package.allows_readiness(&approved));
        assert!(package.allows_readiness(&socket_probe));
        assert!(
            !package.allows_readiness(&Readiness::AllowlistedExecutable {
                path: executable.clone(),
                argv: vec!["--ready".into(), "--delete-state".into()],
                timeout_ms: 500,
                expected_exit: 0,
            })
        );
        assert!(!package.allows_readiness(&Readiness::UnixProbe {
            socket: PathBuf::from("/run/example/health.sock"),
            request: b"mutate\n".to_vec(),
            expected: b"ready\n".to_vec(),
            timeout_ms: 500,
        }));
    }
}

fn resolve_group(value: &str) -> anyhow::Result<u32> {
    if let Ok(gid) = value.parse::<u32>() {
        return Ok(gid);
    }
    Group::from_name(value)?
        .map(|group| group.gid.as_raw())
        .ok_or_else(|| anyhow::anyhow!("configured updater group does not exist"))
}

fn read_admin_file(
    path: &Path,
    limit: usize,
    expected_mode: Option<libc::mode_t>,
) -> anyhow::Result<Vec<u8>> {
    let descriptor = open(
        path,
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        nix::sys::stat::Mode::empty(),
    )?;
    let metadata = fstat(&descriptor)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_uid != 0
        || metadata.st_mode & 0o022 != 0
        || expected_mode.is_some_and(|mode| metadata.st_mode & 0o777 != mode)
        || metadata.st_size < 0
        || metadata.st_size as usize > limit
    {
        anyhow::bail!("admin file must be root-owned, non-writable, and within its size limit");
    }
    let mut bytes = Vec::with_capacity(metadata.st_size as usize);
    File::from(descriptor)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        anyhow::bail!("admin file exceeds configured size limit");
    }
    Ok(bytes)
}

fn validate_protected_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent"))?;
    if fs::canonicalize(parent)? != parent {
        anyhow::bail!("configured path contains symlink traversal");
    }
    let mut current = PathBuf::from("/");
    for component in parent.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                current.push(name);
                let metadata = fs::symlink_metadata(&current)?;
                if !metadata.is_dir()
                    || metadata.file_type().is_symlink()
                    || metadata.uid() != 0
                    || metadata.mode() & 0o022 != 0
                {
                    anyhow::bail!("configured path has an untrusted writable parent");
                }
            }
            _ => anyhow::bail!("configured path has an unsafe parent component"),
        }
    }
    Ok(())
}
