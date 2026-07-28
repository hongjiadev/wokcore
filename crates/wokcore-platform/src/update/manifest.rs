use std::{cmp::Ordering, str};

use base64::{Engine, engine::general_purpose::STANDARD};
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Deserialize;

pub const MAX_UPDATE_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_UPDATE_SIGNATURE_BYTES: usize = 16 * 1024;
pub const MAX_UPDATE_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PUBLIC_KEY_BYTES: usize = 4 * 1024;

const TARGETS: [TargetContract; 5] = [
    TargetContract::new("x86_64-pc-windows-msvc", "zip", "wokcore.exe"),
    TargetContract::new("x86_64-apple-darwin", "tar.gz", "wokcore"),
    TargetContract::new("aarch64-apple-darwin", "tar.gz", "wokcore"),
    TargetContract::new("x86_64-unknown-linux-gnu", "tar.gz", "wokcore"),
    TargetContract::new("aarch64-unknown-linux-gnu", "tar.gz", "wokcore"),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateDecision {
    Current,
    Available(UpdateCandidate),
    IncompatibleManifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateCandidate {
    version: Version,
    artifact: UpdateArtifact,
}

impl UpdateCandidate {
    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn artifact(&self) -> &UpdateArtifact {
        &self.artifact
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateArtifact {
    target: String,
    file: String,
    executable: String,
    size: u64,
    sha256: String,
    url: String,
}

impl UpdateArtifact {
    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn file(&self) -> &str {
        &self.file
    }

    pub fn executable(&self) -> &str {
        &self.executable
    }

    pub const fn size(&self) -> u64 {
        self.size
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    #[doc(hidden)]
    pub fn for_test(
        target: impl Into<String>,
        file: impl Into<String>,
        executable: impl Into<String>,
        size: u64,
        sha256: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            target: target.into(),
            file: file.into(),
            executable: executable.into(),
            size,
            sha256: sha256.into(),
            url: url.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UpdateError {
    #[error("the update manifest is malformed")]
    InvalidManifest,
    #[error("the update signature is invalid")]
    InvalidSignature,
    #[error("the update target does not match this executable")]
    TargetMismatch,
    #[error("the update would downgrade WokCore")]
    DowngradeRejected,
    #[error("the update artifact size does not match its manifest")]
    ArtifactSizeMismatch,
    #[error("the update artifact hash does not match its manifest")]
    ArtifactHashMismatch,
    #[error("the update archive is invalid")]
    InvalidArchive,
    #[error("the update executable could not be staged")]
    StagingFailed,
    #[error("the update executable could not be atomically replaced")]
    AtomicReplaceFailed,
    #[error("the previous executable could not be restored")]
    RollbackFailed,
    #[error(
        "the previous executable was restored but its directory entry could not be synchronized"
    )]
    RollbackDurabilityFailed,
    #[error("the update executable state requires manual recovery")]
    RecoveryRequired,
    #[error("another WokCore update is already in progress")]
    UpdateInProgress,
}

pub fn verify_manifest(
    manifest_bytes: &[u8],
    signature_bytes: &[u8],
    public_key_text: &str,
    current_version: &Version,
    target: &str,
) -> Result<UpdateDecision, UpdateError> {
    if manifest_bytes.is_empty() || manifest_bytes.len() > MAX_UPDATE_MANIFEST_BYTES {
        return Err(UpdateError::InvalidManifest);
    }
    if signature_bytes.is_empty() || signature_bytes.len() > MAX_UPDATE_SIGNATURE_BYTES {
        return Err(UpdateError::InvalidSignature);
    }
    let key_id = verify_signature(manifest_bytes, signature_bytes, public_key_text.as_bytes())?;
    let value: serde_json::Value =
        serde_json::from_slice(manifest_bytes).map_err(|_| UpdateError::InvalidManifest)?;
    let schema_version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(UpdateError::InvalidManifest)?;
    let api_major = value
        .get("api_major")
        .and_then(serde_json::Value::as_u64)
        .ok_or(UpdateError::InvalidManifest)?;
    if schema_version != 1 || api_major != 1 {
        return Ok(UpdateDecision::IncompatibleManifest);
    }
    let document = parse_manifest_document(manifest_bytes)?;
    validate_document(document, &key_id, current_version, target)
}

fn parse_manifest_document(manifest: &[u8]) -> Result<ManifestDocument, UpdateError> {
    serde_json::from_slice(manifest).map_err(|_| UpdateError::InvalidManifest)
}

fn verify_signature(
    manifest: &[u8],
    signature: &[u8],
    public_key: &[u8],
) -> Result<String, UpdateError> {
    if public_key.is_empty() || public_key.len() > MAX_PUBLIC_KEY_BYTES {
        return Err(UpdateError::InvalidSignature);
    }
    let public_key_text = str::from_utf8(public_key).map_err(|_| UpdateError::InvalidSignature)?;
    let signature_text = str::from_utf8(signature).map_err(|_| UpdateError::InvalidSignature)?;
    let key_id = public_key_id(public_key_text)?;
    if signature_text.lines().count() != 4 {
        return Err(UpdateError::InvalidSignature);
    }
    let decoded_key =
        PublicKey::decode(public_key_text).map_err(|_| UpdateError::InvalidSignature)?;
    let decoded_signature =
        Signature::decode(signature_text).map_err(|_| UpdateError::InvalidSignature)?;
    decoded_key
        .verify(manifest, &decoded_signature, false)
        .map_err(|_| UpdateError::InvalidSignature)?;
    Ok(key_id)
}

fn public_key_id(public_key: &str) -> Result<String, UpdateError> {
    let lines = public_key.lines().collect::<Vec<_>>();
    let [comment, payload] = lines.as_slice() else {
        return Err(UpdateError::InvalidSignature);
    };
    let decoded = STANDARD
        .decode(payload)
        .map_err(|_| UpdateError::InvalidSignature)?;
    if decoded.len() != 42 || decoded[..2] != [0x45, 0x64] {
        return Err(UpdateError::InvalidSignature);
    }
    let key_id = decoded[2..10]
        .iter()
        .rev()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    if *comment != format!("untrusted comment: minisign public key {key_id}") {
        return Err(UpdateError::InvalidSignature);
    }
    Ok(key_id)
}

fn validate_document(
    document: ManifestDocument,
    key_id: &str,
    current_version: &Version,
    target: &str,
) -> Result<UpdateDecision, UpdateError> {
    if document.schema_version != 1
        || document.product != "wokcore"
        || document.api_major != 1
        || document.signing_key_id != key_id
        || document.artifacts.len() != TARGETS.len()
    {
        return Err(UpdateError::InvalidManifest);
    }
    let version = Version::parse(&document.version).map_err(|_| UpdateError::InvalidManifest)?;
    if document.version.len() > 128 || version.to_string() != document.version {
        return Err(UpdateError::InvalidManifest);
    }

    let mut selected = None;
    for (artifact, contract) in document.artifacts.into_iter().zip(TARGETS) {
        let validated = validate_artifact(artifact, contract, &document.version)?;
        if contract.target == target {
            selected = Some(validated);
        }
    }
    let artifact = selected.ok_or(UpdateError::TargetMismatch)?;
    match version.cmp(current_version) {
        Ordering::Less => Err(UpdateError::DowngradeRejected),
        Ordering::Equal => Ok(UpdateDecision::Current),
        Ordering::Greater => Ok(UpdateDecision::Available(UpdateCandidate {
            version,
            artifact,
        })),
    }
}

fn validate_artifact(
    artifact: ArtifactDocument,
    contract: TargetContract,
    version: &str,
) -> Result<UpdateArtifact, UpdateError> {
    let expected_file = format!(
        "wokcore-v{version}-{}.{}",
        contract.target, contract.extension
    );
    let expected_url = format!(
        "https://github.com/hongjiadev/wokcore/releases/download/v{version}/{expected_file}"
    );
    if artifact.target != contract.target
        || artifact.file != expected_file
        || artifact.executable != contract.executable
        || artifact.size == 0
        || artifact.size > MAX_UPDATE_ARTIFACT_BYTES
        || !is_lower_hex_sha256(&artifact.sha256)
        || artifact.url != expected_url
    {
        return Err(UpdateError::InvalidManifest);
    }
    Ok(UpdateArtifact {
        target: artifact.target,
        file: artifact.file,
        executable: artifact.executable,
        size: artifact.size,
        sha256: artifact.sha256,
        url: artifact.url,
    })
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub const fn current_target() -> &'static str {
    if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "unsupported"
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocument {
    schema_version: u32,
    product: String,
    api_major: u32,
    version: String,
    signing_key_id: String,
    artifacts: Vec<ArtifactDocument>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactDocument {
    target: String,
    file: String,
    executable: String,
    size: u64,
    sha256: String,
    url: String,
}

#[derive(Clone, Copy)]
struct TargetContract {
    target: &'static str,
    extension: &'static str,
    executable: &'static str,
}

impl TargetContract {
    const fn new(target: &'static str, extension: &'static str, executable: &'static str) -> Self {
        Self {
            target,
            extension,
            executable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{UpdateError, parse_manifest_document, public_key_id};

    #[test]
    fn public_key_comment_must_match_the_payload_key_id() {
        let key = concat!(
            "untrusted comment: minisign public key E7620F1842B4E81F\n",
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n"
        );
        assert_eq!(public_key_id(key).unwrap(), "E7620F1842B4E81F");
        assert_eq!(
            public_key_id(&key.replace("E7620F1842B4E81F", "0000000000000000")).unwrap_err(),
            UpdateError::InvalidSignature,
        );
    }

    #[test]
    fn compatible_manifests_reject_duplicate_json_members() {
        let duplicate = br#"{
            "schema_version": 1,
            "product": "wokcore",
            "api_major": 1,
            "version": "1.2.3",
            "version": "1.2.4",
            "signing_key_id": "0000000000000000",
            "artifacts": []
        }"#;

        assert!(matches!(
            parse_manifest_document(duplicate),
            Err(UpdateError::InvalidManifest),
        ));
    }
}
