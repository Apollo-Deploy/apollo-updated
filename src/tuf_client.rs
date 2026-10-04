use crate::{
    contract::{PackageContract, validate_contract},
    disk::Store,
    error::UpdateError,
    settings::{AllowedPackage, Settings},
    state::PackageState,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, time::Duration};
use tokio::time::timeout;
use tough::{
    DefaultTransport, ExpirationEnforcement, HttpTransportBuilder, RepositoryLoader, TargetName,
};

#[derive(Debug)]
pub struct VerifiedPackage {
    pub contract: PackageContract,
    pub manifest: Vec<u8>,
    pub target_name: String,
    pub artifact_path: PathBuf,
    pub artifact_sha256: String,
    pub _space_reservation: Option<crate::disk::SpaceReservation>,
}

#[derive(Debug, Serialize)]
pub struct TrustAnchorExpiry {
    pub expires: String,
    pub expired: bool,
}

pub const MAX_TRUST_ROOT_BYTES: u64 = 1024 * 1024;

/// Parses and verifies the bootstrap root's root-role threshold signatures.
///
/// The TUF loader repeats this check while loading a repository. Doing it at
/// configuration load time makes a broken or substituted bootstrap root a
/// daemon startup error, before the updater opens its API socket.
pub fn validate_trust_anchor(bytes: &[u8]) -> anyhow::Result<()> {
    if bytes.len() as u64 > MAX_TRUST_ROOT_BYTES {
        anyhow::bail!("trusted TUF root exceeds configured bound");
    }
    let root: tough::schema::Signed<tough::schema::Root> = serde_json::from_slice(bytes)?;
    root.signed.verify_role(&root)?;
    Ok(())
}

pub fn validate_trust_anchor_excluding(bytes: &[u8], excluded_root: &[u8]) -> anyhow::Result<()> {
    validate_trust_anchor(bytes)?;
    validate_trust_anchor(excluded_root)?;
    let candidate = trust_root_public_keys(bytes)?;
    let excluded = trust_root_public_keys(excluded_root)?;
    if !candidate.is_disjoint(&excluded) {
        anyhow::bail!("TUF root reuses a prohibited public key");
    }
    Ok(())
}

fn trust_root_public_keys(
    bytes: &[u8],
) -> anyhow::Result<std::collections::BTreeSet<(String, Vec<u8>)>> {
    let root: tough::schema::Signed<tough::schema::Root> = serde_json::from_slice(bytes)?;
    Ok(root
        .signed
        .keys
        .into_values()
        .map(public_key_identity)
        .collect())
}

fn public_key_identity(key: tough::schema::key::Key) -> (String, Vec<u8>) {
    use tough::schema::key::Key;

    match key {
        Key::Ed25519 { keyval, .. } => ("ed25519".to_owned(), keyval.public.into_vec()),
        Key::Rsa { keyval, .. } => ("rsa".to_owned(), keyval.public.into_vec()),
        Key::Ecdsa { keyval, .. } | Key::EcdsaOld { keyval, .. } => {
            ("ecdsa".to_owned(), keyval.public.into_vec())
        }
    }
}

pub fn inspect_trust_anchor(bytes: &[u8]) -> anyhow::Result<TrustAnchorExpiry> {
    if bytes.len() > 1024 * 1024 {
        anyhow::bail!("trusted TUF root exceeds configured bound");
    }
    let root: serde_json::Value = serde_json::from_slice(&bytes)?;
    let expires = root
        .pointer("/signed/expires")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("TUF root has no signed expiry"))?;
    let expiry =
        time::OffsetDateTime::parse(expires, &time::format_description::well_known::Rfc3339)?;
    Ok(TrustAnchorExpiry {
        expires: expires.to_owned(),
        expired: expiry <= time::OffsetDateTime::now_utc(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedTargetBinding {
    package_id: String,
    version: semver::Version,
    architecture: String,
    manifest_target: String,
}

pub async fn load(settings: &Settings, store: &Store) -> anyhow::Result<tough::Repository> {
    let root = &settings.trusted_root_bytes;
    if root.len() > 1024 * 1024 {
        anyhow::bail!("trusted TUF root exceeds configured bound");
    }
    let datastore = store.root().join("trust");
    let _ = fs::create_dir_all(&datastore);
    let transport = DefaultTransport::new_with_http_settings(
        HttpTransportBuilder::new()
            .timeout(Duration::from_secs(1800))
            .connect_timeout(Duration::from_secs(10))
            .tries(2),
    );
    let repository = timeout(
        Duration::from_secs(1800),
        RepositoryLoader::new(
            root,
            settings.metadata_url.clone(),
            settings.targets_url.clone(),
        )
        .transport(transport)
        .datastore(datastore)
        .expiration_enforcement(ExpirationEnforcement::Safe)
        .load(),
    )
    .await
    .map_err(|_| UpdateError::ArtifactLimit)??;
    if repository.targets().signed.delegations.is_some() {
        anyhow::bail!("delegated targets are disabled by policy");
    }
    Ok(repository)
}

pub async fn verify_target(
    repository: &tough::Repository,
    settings: &Settings,
    store: &Store,
    allowed: &AllowedPackage,
    state: &PackageState,
) -> anyhow::Result<VerifiedPackage> {
    let prefix = "packages/";
    let mut candidates = Vec::<(String, SignedTargetBinding)>::new();
    for (name, target) in repository.all_targets() {
        if let Some(rest) = name.raw().strip_prefix(prefix) {
            let target_id = rest.split('/').next().ok_or(UpdateError::TargetMismatch)?;
            if settings.package(target_id).is_none() {
                return Err(UpdateError::TargetMismatch.into());
            }
        }
        if !name
            .raw()
            .starts_with(&format!("packages/{}/", allowed.package_id))
            || !name.raw().ends_with("/manifest.json")
        {
            continue;
        }
        let binding: SignedTargetBinding = serde_json::from_value(
            target
                .custom
                .get("apollo_package")
                .cloned()
                .ok_or(UpdateError::TargetMismatch)?,
        )?;
        candidates.push((name.raw().to_owned(), binding));
    }
    let mut candidates = candidates
        .into_iter()
        .filter(|(_, binding)| {
            binding.package_id == allowed.package_id && binding.architecture == allowed.architecture
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.1
            .version
            .cmp(&right.1.version)
            .then_with(|| left.0.cmp(&right.0))
    });
    if candidates
        .windows(2)
        .any(|pair| pair[0].1.version == pair[1].1.version)
    {
        return Err(UpdateError::TargetMismatch.into());
    }
    let (manifest_target_name, binding) = candidates
        .into_iter()
        .max_by(|left, right| left.1.version.cmp(&right.1.version))
        .ok_or(UpdateError::TargetMismatch)?;
    let expected_target = format!(
        "packages/{}/{}/{}/manifest.json",
        binding.package_id, binding.version, binding.architecture
    );
    if manifest_target_name != expected_target || binding.manifest_target != manifest_target_name {
        return Err(UpdateError::TargetMismatch.into());
    }
    let manifest_target = TargetName::try_from(manifest_target_name.clone())?;
    if binding.package_id != allowed.package_id || binding.architecture != allowed.architecture {
        return Err(UpdateError::TargetMismatch.into());
    }
    if let Some(highest) = state.highest_verified_version.as_deref() {
        if binding.version < semver::Version::parse(highest)? {
            return Err(UpdateError::RollbackDenied.into());
        }
    }
    let manifest_target_meta = repository
        .all_targets()
        .find(|(name, _)| name == &&manifest_target)
        .map(|(_, target)| target)
        .ok_or(UpdateError::TargetMismatch)?;
    if manifest_target_meta.length > 1024 * 1024 {
        return Err(UpdateError::ArtifactLimit.into());
    }
    let manifest = read_small(repository, &manifest_target, 1024 * 1024).await?;
    let contract: PackageContract = serde_json::from_slice(&manifest)?;
    validate_contract(&contract, settings.max_artifact_size)?;
    if contract.package_id != binding.package_id
        || contract.version != binding.version
        || contract.architecture != binding.architecture
        || contract.service_name != allowed.service_name
    {
        return Err(UpdateError::TargetMismatch.into());
    }
    if !allowed.allows_readiness(&contract.health.readiness) {
        return Err(UpdateError::UnsafeManifest.into());
    }
    if contract.listener == crate::contract::ListenerMode::None && !allowed.allow_listenerless {
        return Err(UpdateError::HandoffUnavailable.into());
    }
    let artifact_name = match &contract.artifact {
        crate::contract::ArtifactSource::Tuf { target } => TargetName::try_from(target.clone())?,
        crate::contract::ArtifactSource::Https { .. }
        | crate::contract::ArtifactSource::Local { .. } => {
            anyhow::bail!("only TUF role targets are enabled for artifact transfer")
        }
    };
    let artifact_meta = repository
        .all_targets()
        .find(|(name, _)| name == &&artifact_name)
        .map(|(_, target)| target)
        .ok_or(UpdateError::TargetMismatch)?;
    let artifact_custom = artifact_meta
        .custom
        .get("apollo_package")
        .ok_or(UpdateError::TargetMismatch)?;
    validate_artifact_binding(
        artifact_name.raw(),
        artifact_custom,
        &binding,
        &manifest_target_name,
    )?;
    if artifact_meta.length != contract.size || contract.size > settings.max_artifact_size {
        return Err(UpdateError::TargetMismatch.into());
    }
    let space_reservation =
        store.reserve_download_space(&allowed.package_id, contract.size, 8 * 1024 * 1024 * 1024)?;
    let signed_digest = hex::encode(artifact_meta.hashes.sha256.as_ref());
    if !signed_digest.eq_ignore_ascii_case(&contract.sha256) {
        return Err(UpdateError::TargetMismatch.into());
    }
    let Some(mut stream) = repository.read_target(&artifact_name).await? else {
        return Err(UpdateError::TargetMismatch.into());
    };
    let version = binding.version.to_string();
    let sink = store.artifact_sink(&allowed.package_id, &version, settings.max_artifact_size)?;
    let started = tokio::time::Instant::now();
    let mut sent = 0_u64;
    while let Some(chunk) = timeout(Duration::from_secs(45), stream.next())
        .await
        .map_err(|_| UpdateError::ArtifactLimit)?
    {
        let bytes = chunk?;
        sent = sent
            .checked_add(bytes.len() as u64)
            .ok_or(UpdateError::ArtifactLimit)?;
        if sent > contract.size || started.elapsed() > Duration::from_secs(1800) {
            return Err(UpdateError::ArtifactLimit.into());
        }
        sink.send(bytes.to_vec()).await?;
    }
    let (path, length, digest) = sink.finish().await?;
    if length != contract.size || !digest.eq_ignore_ascii_case(&contract.sha256) {
        let _ = fs::remove_file(path);
        return Err(UpdateError::TargetMismatch.into());
    }
    Ok(VerifiedPackage {
        contract,
        manifest,
        target_name: manifest_target_name,
        artifact_path: path,
        artifact_sha256: digest,
        _space_reservation: Some(space_reservation),
    })
}

async fn read_small(
    repository: &tough::Repository,
    target: &TargetName,
    max: u64,
) -> anyhow::Result<Vec<u8>> {
    let Some(mut stream) = repository.read_target(target).await? else {
        return Err(UpdateError::TargetMismatch.into());
    };
    let mut bytes = Vec::new();
    let started = tokio::time::Instant::now();
    while let Some(chunk) = timeout(Duration::from_secs(15), stream.next())
        .await
        .map_err(|_| UpdateError::ArtifactLimit)?
    {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) as u64 > max
            || started.elapsed() > Duration::from_secs(60)
        {
            return Err(UpdateError::ArtifactLimit.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub fn digest_file(path: &PathBuf) -> anyhow::Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn validate_artifact_binding(
    target_name: &str,
    custom: &serde_json::Value,
    expected: &SignedTargetBinding,
    manifest_target: &str,
) -> anyhow::Result<()> {
    let actual: SignedTargetBinding = serde_json::from_value(custom.clone())?;
    let prefix = format!(
        "packages/{}/{}/{}/",
        expected.package_id, expected.version, expected.architecture
    );
    if !target_name.starts_with(&prefix)
        || target_name.len() == prefix.len()
        || actual.package_id != expected.package_id
        || actual.version != expected.version
        || actual.architecture != expected.architecture
        || actual.manifest_target != manifest_target
    {
        return Err(UpdateError::TargetMismatch.into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "tuf_client_tests.rs"]
mod tests;
