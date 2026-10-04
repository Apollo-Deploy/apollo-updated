use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

/// Root-owned execution policy for a package generation. None of these values
/// may be supplied or overridden by signed package metadata.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationPolicy {
    pub socket_unit: Option<String>,
    pub entrypoint: PathBuf,
    #[serde(default)]
    pub argv: Vec<String>,
    pub runtime_uid: u32,
    pub runtime_gid: u32,
    #[serde(default)]
    pub writable_paths: Vec<PathBuf>,
}

impl GenerationPolicy {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        if !safe_relative(&self.entrypoint)
            || self.runtime_uid == 0
            || self.runtime_gid == 0
            || self.argv.len() > 64
            || self
                .argv
                .iter()
                .any(|arg| arg.len() > 4096 || arg.contains('\0'))
            || self.writable_paths.len() > 32
            || self.writable_paths.iter().any(|path| !safe_absolute(path))
        {
            anyhow::bail!("invalid package generation execution policy");
        }
        if self
            .socket_unit
            .as_deref()
            .is_some_and(|unit| !unit.ends_with(".socket") || !unit_name(unit))
        {
            anyhow::bail!("invalid package listener socket unit");
        }
        Ok(())
    }
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn unit_name(unit: &str) -> bool {
    !unit.is_empty()
        && unit
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@'))
}

#[cfg(test)]
mod tests {
    use super::GenerationPolicy;
    use std::path::PathBuf;

    fn policy() -> GenerationPolicy {
        GenerationPolicy {
            socket_unit: Some("apollo-demo.socket".into()),
            entrypoint: PathBuf::from("bin/demo"),
            argv: vec!["--serve".into()],
            runtime_uid: 1001,
            runtime_gid: 1001,
            writable_paths: vec![PathBuf::from("/var/lib/demo")],
        }
    }

    #[test]
    fn generation_policy_rejects_manifest_like_paths_and_root_users() {
        assert!(policy().validate().is_ok());
        let mut invalid = policy();
        invalid.entrypoint = PathBuf::from("../bin/demo");
        assert!(invalid.validate().is_err());
        let mut invalid = policy();
        invalid.socket_unit = Some("../demo.socket".into());
        assert!(invalid.validate().is_err());
        let mut invalid = policy();
        invalid.runtime_uid = 0;
        assert!(invalid.validate().is_err());
    }
}
