use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

pub const SUPPORTED_PACKAGE_FORMAT: u32 = 1;
pub const SUPPORTED_STATE_FORMAT: u32 = 1;
pub const SUPPORTED_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageContract {
    pub package_id: String,
    pub version: Version,
    pub architecture: String,
    pub artifact: ArtifactSource,
    pub sha256: String,
    pub size: u64,
    pub service_name: String,
    pub listener: ListenerMode,
    pub health: HealthContract,
    pub drain_timeout_ms: u32,
    pub health_timeout_ms: u32,
    pub stabilization_ms: u32,
    pub compatibility: Compatibility,
    #[serde(default)]
    pub coordinated_set: Option<CoordinatedSet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum ArtifactSource {
    Tuf { target: String },
    Local { path: PathBuf },
    Https { url: Url },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ListenerMode {
    InheritedFd,
    ReusePortDrain,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthContract {
    pub readiness: Readiness,
    pub stabilization_ms: u32,
    pub failure_threshold: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum Readiness {
    ProcessAlive,
    SupervisorReady,
    UnixProbe {
        socket: PathBuf,
        request: Vec<u8>,
        expected: Vec<u8>,
        timeout_ms: u32,
    },
    AllowlistedExecutable {
        path: PathBuf,
        argv: Vec<String>,
        timeout_ms: u32,
        expected_exit: i32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    pub minimum_updater: Version,
    pub maximum_updater: Version,
    pub package_format: u32,
    pub state_format: u32,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatedSet {
    pub set_id: String,
    pub member_versions: Vec<(String, Version)>,
    pub handoff_order: Vec<String>,
}

pub fn validate_contract(contract: &PackageContract, max_artifact: u64) -> anyhow::Result<()> {
    if !valid_id(&contract.package_id)
        || !valid_id(&contract.service_name)
        || contract.size == 0
        || contract.size > max_artifact
        || contract.sha256.len() != 64
        || !contract.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || contract.health.failure_threshold == 0
        || contract.health_timeout_ms == 0
        || contract.drain_timeout_ms == 0
    {
        anyhow::bail!("invalid package contract bounds or identifier");
    }
    if let ArtifactSource::Https { url } = &contract.artifact {
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            anyhow::bail!("artifact URL must be HTTPS without embedded credentials");
        }
    }
    if let Readiness::AllowlistedExecutable { path, argv, .. } = &contract.health.readiness {
        if !path.is_absolute() || argv.len() > 32 || argv.iter().any(|arg| arg.len() > 4096) {
            anyhow::bail!("invalid health executable contract");
        }
    }
    if let Readiness::UnixProbe {
        socket,
        request,
        expected,
        ..
    } = &contract.health.readiness
    {
        if !socket.is_absolute() || request.len() > 4096 || expected.len() > 4096 {
            anyhow::bail!("invalid health probe contract");
        }
    }
    validate_compatibility(&contract.compatibility)?;
    Ok(())
}

pub fn validate_compatibility(compatibility: &Compatibility) -> anyhow::Result<()> {
    let updater = Version::parse(env!("CARGO_PKG_VERSION"))?;
    if compatibility.minimum_updater > compatibility.maximum_updater
        || updater < compatibility.minimum_updater
        || updater > compatibility.maximum_updater
        || compatibility.package_format != SUPPORTED_PACKAGE_FORMAT
        || compatibility.state_format != SUPPORTED_STATE_FORMAT
        || compatibility.protocol_version != SUPPORTED_PROTOCOL_VERSION
    {
        return Err(crate::error::UpdateError::IncompatiblePackage.into());
    }
    Ok(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

#[cfg(test)]
mod tests {
    use super::{Compatibility, PackageContract, validate_compatibility, validate_contract};
    use semver::Version;

    #[test]
    fn manifest_shell_hook_is_rejected_by_typed_schema() {
        let manifest = br#"{
          "package_id":"sample","version":"1.2.3","architecture":"aarch64-linux-gnu",
          "artifact":{"kind":"tuf","target":"packages/sample/1.2.3/aarch64-linux-gnu.tar.zst"},
          "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "size":12,"service_name":"sample","listener":"none",
          "health":{"readiness":{"kind":"process_alive"},"stabilization_ms":100,"failure_threshold":1},
          "drain_timeout_ms":1000,"health_timeout_ms":1000,"stabilization_ms":100,
          "compatibility":{"minimum_updater":"0.1.0","maximum_updater":"0.1.0","package_format":1,"state_format":1,"protocol_version":1},
          "pre_update_command":"touch /tmp/owned"
        }"#;
        assert!(serde_json::from_slice::<PackageContract>(manifest).is_err());
    }

    #[test]
    fn credentialed_artifact_url_is_rejected() {
        let manifest = br#"{
          "package_id":"sample","version":"1.2.3","architecture":"aarch64-linux-gnu",
          "artifact":{"kind":"https","url":"https://name:secret@example.invalid/service.tar.zst"},
          "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "size":12,"service_name":"sample","listener":"none",
          "health":{"readiness":{"kind":"process_alive"},"stabilization_ms":100,"failure_threshold":1},
          "drain_timeout_ms":1000,"health_timeout_ms":1000,"stabilization_ms":100,
          "compatibility":{"minimum_updater":"0.1.0","maximum_updater":"0.1.0","package_format":1,"state_format":1,"protocol_version":1}
        }"#;
        let contract: PackageContract =
            serde_json::from_slice(manifest).expect("typed manifest parses");
        assert!(validate_contract(&contract, 1024).is_err());
    }

    #[test]
    fn unsupported_compatibility_versions_are_rejected() {
        let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("updater version");
        let supported = Compatibility {
            minimum_updater: current.clone(),
            maximum_updater: current,
            package_format: 1,
            state_format: 1,
            protocol_version: 1,
        };
        assert!(validate_compatibility(&supported).is_ok());
        let incompatible = Compatibility {
            package_format: 2,
            ..supported
        };
        assert!(validate_compatibility(&incompatible).is_err());
    }
}
