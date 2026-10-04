use crate::{
    settings::{AllowedPackage, Settings},
    supervisor::ListenerSource,
};
use anyhow::{Context, bail, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

const ROOT: &str = "/var/lib/apollo-updated-supervisor";
const UNIT_DIR: &str = "/etc/systemd/system";
const RUNTIME_DIR: &str = "/run/apollo-updated-generations";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnitPlan {
    pub id: String,
    #[serde(default)]
    pub package_id: String,
    pub version: String,
    pub digest: String,
    pub service_unit: String,
    pub control_socket_unit: String,
    pub control_path: PathBuf,
    pub immutable_tree: PathBuf,
    pub listener_socket_unit: Option<String>,
}

pub(crate) fn listener_name(package_id: &str) -> String {
    format!("apollo-updated-listener-{package_id}")
}

pub(crate) fn listener_socket_ready(unit: &str, package_id: &str) -> bool {
    active(unit)
        && property(unit, "Accept").is_ok_and(|value| value == "no")
        && property(unit, "Triggers").is_ok_and(|value| {
            value
                .split_whitespace()
                .any(|trigger| trigger == "apollo-updated-supervisor.service")
        })
        && property(unit, "FileDescriptorName")
            .is_ok_and(|value| value == listener_name(package_id))
}

pub fn prepare(
    settings: &Settings,
    package: &AllowedPackage,
    version: &str,
    generation_id: &str,
    source: &Path,
    listener: Option<&ListenerSource>,
) -> anyhow::Result<UnitPlan> {
    let policy = package
        .generation
        .as_ref()
        .context("generation policy missing")?;
    semver::Version::parse(version).context("invalid generation version")?;
    let listener_socket_unit = match (&policy.socket_unit, listener) {
        (Some(expected), Some(ListenerSource::SystemdSocket { unit })) if expected == unit => {
            Some(expected.clone())
        }
        (None, None) if package.allow_listenerless => None,
        _ => bail!(crate::error::UpdateError::HandoffUnavailable),
    };
    let expected_source = settings
        .data_root
        .join("packages")
        .join(&package.package_id)
        .join("versions")
        .join(version);
    ensure!(
        source == expected_source,
        "generation source is outside its package version directory"
    );
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "generation source is not a plain directory"
    );
    ensure!(
        source.canonicalize()? == source,
        "generation source contains a symlinked parent"
    );
    ensure!(
        !generation_id.is_empty()
            && generation_id.len() <= 64
            && generation_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "invalid generation identity"
    );
    let digest = read_digest(source)?;
    ensure_public_dir(Path::new(ROOT))?;
    ensure_public_dir(&Path::new(ROOT).join("packages"))?;
    ensure_public_dir(&Path::new(ROOT).join("packages").join(&package.package_id))?;
    ensure_public_dir(
        &Path::new(ROOT)
            .join("packages")
            .join(&package.package_id)
            .join("versions"),
    )?;
    let immutable_tree = PathBuf::from(ROOT)
        .join("packages")
        .join(&package.package_id)
        .join("versions")
        .join(format!("{version}-{digest}"));
    if !immutable_tree.exists() {
        copy_immutable_tree(source, &immutable_tree, settings.max_artifact_size)?;
    } else {
        let copied_digest = read_digest(&immutable_tree)?;
        ensure!(
            copied_digest == digest,
            "root-owned generation digest mismatch"
        );
    }
    let entrypoint = immutable_tree.join("payload.tree").join(&policy.entrypoint);
    let entry_meta = fs::symlink_metadata(&entrypoint)?;
    ensure!(
        entry_meta.is_file()
            && !entry_meta.file_type().is_symlink()
            && entry_meta.mode() & 0o111 != 0,
        "generation entrypoint is not executable"
    );
    let service_unit = format!("apollo-updated-gen-{generation_id}.service");
    let control_socket_unit = format!("apollo-updated-control-{generation_id}.socket");
    let control_path = PathBuf::from(RUNTIME_DIR).join(format!("{generation_id}.sock"));
    let plan = UnitPlan {
        id: generation_id.to_owned(),
        package_id: package.package_id.clone(),
        version: version.into(),
        digest,
        service_unit,
        control_socket_unit,
        control_path,
        immutable_tree,
        listener_socket_unit,
    };
    write_generation_units(
        package,
        &plan,
        policy.runtime_uid,
        policy.runtime_gid,
        &policy.argv,
    )?;
    Ok(plan)
}

pub fn remove(plan: &UnitPlan) -> anyhow::Result<()> {
    let service = unit_file(&plan.service_unit);
    let control = unit_file(&plan.control_socket_unit);
    remove_managed_unit(&service, "apollo-updated-gen-")?;
    remove_managed_unit(&control, "apollo-updated-control-")?;
    match fs::symlink_metadata(&plan.control_path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            fs::remove_file(&plan.control_path)?;
        }
        Ok(_) => bail!("refusing to remove a non-socket generation control path"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    systemctl(&["daemon-reload"])?;
    Ok(())
}

pub fn commit_alias(service: &str, target: &str) -> anyhow::Result<()> {
    ensure!(
        target.starts_with("apollo-updated-gen-") && target.ends_with(".service"),
        "invalid generation alias target"
    );
    ensure!(
        service
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@')),
        "invalid stable service name"
    );
    let alias = PathBuf::from(UNIT_DIR).join(format!("{service}.service"));
    let temp =
        PathBuf::from(UNIT_DIR).join(format!(".{service}.service.{}.tmp", std::process::id()));
    match fs::symlink_metadata(&alias) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let link = fs::read_link(&alias)?;
            let name = link
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or_default();
            ensure!(
                name.starts_with("apollo-updated-gen-") && name.ends_with(".service"),
                "refusing to replace an unmanaged service alias"
            );
        }
        Ok(_) => bail!(crate::error::UpdateError::SupervisorCannotHandoff),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let _ = fs::remove_file(&temp);
    std::os::unix::fs::symlink(target, &temp)?;
    fs::rename(&temp, &alias)?;
    File::open(UNIT_DIR)?.sync_all()?;
    systemctl(&["daemon-reload"])?;
    Ok(())
}

pub fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let result = Command::new("systemctl").args(args).output()?;
    ensure!(
        result.status.success(),
        "systemctl operation failed: {}",
        args.join(" ")
    );
    Ok(())
}

pub fn property(unit: &str, name: &str) -> anyhow::Result<String> {
    let result = Command::new("systemctl")
        .args(["show", "--property", name, "--value", unit])
        .output()?;
    ensure!(result.status.success(), "systemd property query failed");
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

pub fn active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

fn write_generation_units(
    package: &AllowedPackage,
    plan: &UnitPlan,
    uid: u32,
    gid: u32,
    argv: &[String],
) -> anyhow::Result<()> {
    let runtime = plan
        .control_path
        .parent()
        .context("control path has no parent")?;
    ensure_public_dir(runtime)?;
    let socket_text = format!(
        "[Unit]\nDescription=Apollo generation control socket {}\n\n[Socket]\nListenStream={}\nSocketUser=root\nSocketGroup=root\nSocketMode=0600\nRemoveOnStop=yes\nService={}\n",
        plan.id,
        unit_path(&plan.control_path)?,
        plan.service_unit,
    );
    let entrypoint = plan.immutable_tree.join("payload.tree").join(
        package
            .generation
            .as_ref()
            .context("generation policy missing")?
            .entrypoint
            .as_path(),
    );
    let mut command = vec![quote(&entrypoint.to_string_lossy())?];
    command.extend(
        argv.iter()
            .map(|arg| quote(arg))
            .collect::<anyhow::Result<Vec<_>>>()?,
    );
    let unit_text = format!(
        "[Unit]\nDescription=Apollo package {} generation {}\nAfter=network.target\n\n[Service]\nType=simple\nSockets={}\nUser={}\nGroup={}\nSupplementaryGroups=\nWorkingDirectory={}\nEnvironment=APOLLO_PACKAGE_VERSION={}\nEnvironment=APOLLO_UPDATED_CONTROL_FD=3\nEnvironment=APOLLO_UPDATED_EXPECT_LISTENER_FD={}\nExecStart={}\nRestart=no\nNoNewPrivileges=yes\nPrivateTmp=yes\nProtectSystem=strict\nProtectHome=yes\nProtectKernelTunables=yes\nProtectKernelModules=yes\nProtectControlGroups=yes\nRestrictSUIDSGID=yes\nLockPersonality=yes\nMemoryDenyWriteExecute=yes\nReadWritePaths={}\nUMask=0077\nCapabilityBoundingSet=\nAmbientCapabilities=\nTimeoutStopSec=30s\n",
        package.package_id,
        plan.version,
        plan.control_socket_unit,
        uid,
        gid,
        unit_path(&plan.immutable_tree.join("payload.tree"))?,
        plan.version,
        if plan.listener_socket_unit.is_some() {
            1
        } else {
            0
        },
        command.join(" "),
        package
            .generation
            .as_ref()
            .unwrap()
            .writable_paths
            .iter()
            .map(|path| unit_path(path))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(" "),
    );
    write_unit(&plan.control_socket_unit, &socket_text)?;
    write_unit(&plan.service_unit, &unit_text)?;
    systemctl(&["daemon-reload"])?;
    Ok(())
}

fn write_unit(name: &str, contents: &str) -> anyhow::Result<()> {
    let path = unit_file(name);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "managed unit path is not a regular file"
        );
        let old = fs::read(&path)?;
        if old == contents.as_bytes() {
            return Ok(());
        }
        bail!("refusing to mutate an existing immutable generation unit");
    }
    atomic_write(&path, contents.as_bytes(), 0o644)
}

fn remove_managed_unit(path: &Path, prefix: &str) -> anyhow::Result<()> {
    if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
        ensure!(
            name.starts_with(prefix),
            "refusing to remove unmanaged unit"
        );
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path)?
        }
        Ok(_) => bail!("managed unit is not a plain file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn copy_immutable_tree(source: &Path, destination: &Path, max_artifact: u64) -> anyhow::Result<()> {
    ensure_public_dir(destination)?;
    let limit = max_artifact
        .saturating_mul(3)
        .saturating_add(16 * 1024 * 1024);
    let mut copied = 0_u64;
    for entry in WalkDir::new(source).follow_links(false).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        ensure!(
            relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
            "unsafe package tree path"
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "package tree contains a symlink"
        );
        let target = destination.join(relative);
        if metadata.is_dir() {
            fs::create_dir(&target)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        } else if metadata.is_file() {
            copied = copied
                .checked_add(metadata.len())
                .context("package tree size overflow")?;
            ensure!(copied <= limit, "package tree exceeds extraction bound");
            let mut options = OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            let mut input = options.open(entry.path())?;
            let after = input.metadata()?;
            ensure!(
                after.dev() == metadata.dev() && after.ino() == metadata.ino(),
                "package tree changed while copying"
            );
            let mode = if metadata.mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&target)?;
            output.set_permissions(fs::Permissions::from_mode(mode))?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        } else {
            bail!("package tree contains a non-regular file");
        }
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}

fn read_digest(tree: &Path) -> anyhow::Result<String> {
    let path = tree.join("digest.sha256");
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 128,
        "invalid generation digest sidecar"
    );
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    let digest = String::from_utf8(bytes)?.trim().to_ascii_lowercase();
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid generation digest"
    );
    Ok(digest)
}

fn unit_file(name: &str) -> PathBuf {
    PathBuf::from(UNIT_DIR).join(name)
}

fn ensure_public_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "generation directory is not a plain directory"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> anyhow::Result<()> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(mode);
    let mut file = options.open(&temp)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(path.parent().context("unit file has no parent")?)?.sync_all()?;
        Ok::<_, anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn quote(value: &str) -> anyhow::Result<String> {
    ensure!(
        !value.contains('\0') && !value.contains('\n') && !value.contains('\r'),
        "invalid unit value"
    );
    let escaped = value
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

fn unit_path(path: &Path) -> anyhow::Result<String> {
    let value = path
        .to_str()
        .context("systemd unit paths must be valid UTF-8")?;
    anyhow::ensure!(
        !value.contains('\0') && !value.contains('\n') && !value.contains('\r'),
        "invalid systemd unit path"
    );
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'%' => escaped.push_str("%%"),
            b'/' | b'.' | b'_' | b'-' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => {
                escaped.push(char::from(byte));
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                escaped.push_str("\\x");
                escaped.push(char::from(HEX[(byte >> 4) as usize]));
                escaped.push(char::from(HEX[(byte & 0x0f) as usize]));
            }
        }
    }
    Ok(escaped)
}
