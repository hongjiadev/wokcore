use std::{cmp::Ordering, str};

use base64::{Engine, engine::general_purpose::STANDARD};
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Deserialize;

pub const MAX_UPDATE_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_UPDATE_SIGNATURE_BYTES: usize = 16 * 1024;
pub const MAX_UPDATE_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PUBLIC_KEY_BYTES: usize = 4 * 1024;

const V1_TARGETS: [TargetContract; 5] = [
    TargetContract::legacy(
        "x86_64-pc-windows-msvc",
        "Windows",
        "x86_64",
        "zip",
        "wokcore.exe",
    ),
    TargetContract::legacy(
        "x86_64-apple-darwin",
        "macOS",
        "x86_64",
        "tar.gz",
        "wokcore",
    ),
    TargetContract::legacy(
        "aarch64-apple-darwin",
        "macOS",
        "arm64",
        "tar.gz",
        "wokcore",
    ),
    TargetContract::legacy(
        "x86_64-unknown-linux-gnu",
        "Linux",
        "x86_64",
        "tar.gz",
        "wokcore",
    ),
    TargetContract::legacy(
        "aarch64-unknown-linux-gnu",
        "Linux",
        "arm64",
        "tar.gz",
        "wokcore",
    ),
];
const V2_TARGETS: [TargetContract; 6] = [
    TargetContract::friendly(
        "x86_64-pc-windows-msvc",
        "Windows",
        "x86_64",
        "zip",
        "wokcore.exe",
    ),
    TargetContract::friendly(
        "aarch64-pc-windows-msvc",
        "Windows",
        "arm64",
        "zip",
        "wokcore.exe",
    ),
    TargetContract::friendly(
        "x86_64-apple-darwin",
        "macOS",
        "x86_64",
        "tar.gz",
        "wokcore",
    ),
    TargetContract::friendly(
        "aarch64-apple-darwin",
        "macOS",
        "arm64",
        "tar.gz",
        "wokcore",
    ),
    TargetContract::friendly(
        "x86_64-unknown-linux-gnu",
        "Linux",
        "x86_64",
        "tar.gz",
        "wokcore",
    ),
    TargetContract::friendly(
        "aarch64-unknown-linux-gnu",
        "Linux",
        "arm64",
        "tar.gz",
        "wokcore",
    ),
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
    if !matches!(schema_version, 1 | 2) || api_major != 1 {
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
    let targets: &[TargetContract] = match document.schema_version {
        1 => &V1_TARGETS,
        2 => &V2_TARGETS,
        _ => return Err(UpdateError::InvalidManifest),
    };
    if document.product != "wokcore"
        || document.api_major != 1
        || document.signing_key_id != key_id
        || document.artifacts.len() != targets.len()
    {
        return Err(UpdateError::InvalidManifest);
    }
    let version = Version::parse(&document.version).map_err(|_| UpdateError::InvalidManifest)?;
    if document.version.len() > 128 || version.to_string() != document.version {
        return Err(UpdateError::InvalidManifest);
    }

    let mut selected = None;
    for (artifact, contract) in document.artifacts.into_iter().zip(targets.iter().copied()) {
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
    let expected_file = contract.expected_file(version);
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
    } else if cfg!(all(target_arch = "aarch64", target_os = "windows")) {
        "aarch64-pc-windows-msvc"
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
    system: &'static str,
    architecture: &'static str,
    extension: &'static str,
    executable: &'static str,
    friendly_name: bool,
}

impl TargetContract {
    const fn legacy(
        target: &'static str,
        system: &'static str,
        architecture: &'static str,
        extension: &'static str,
        executable: &'static str,
    ) -> Self {
        Self {
            target,
            system,
            architecture,
            extension,
            executable,
            friendly_name: false,
        }
    }

    const fn friendly(
        target: &'static str,
        system: &'static str,
        architecture: &'static str,
        extension: &'static str,
        executable: &'static str,
    ) -> Self {
        Self {
            target,
            system,
            architecture,
            extension,
            executable,
            friendly_name: true,
        }
    }

    fn expected_file(self, version: &str) -> String {
        if !self.friendly_name {
            return format!("wokcore-v{version}-{}.{}", self.target, self.extension);
        }
        let portable = if self.system == "Windows" {
            "-Portable"
        } else {
            ""
        };
        format!(
            "WokCore-v{version}-{}-{}{}.{extension}",
            self.system,
            self.architecture,
            portable,
            extension = self.extension,
        )
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::{
        UpdateDecision, UpdateError, parse_manifest_document, public_key_id, validate_document,
    };

    #[test]
    fn manifest_v2_selects_windows_arm64() {
        let v2 = br#"{
            "schema_version": 2,
            "product": "wokcore",
            "api_major": 1,
            "version": "1.2.3",
            "signing_key_id": "0000000000000000",
            "artifacts": [
                {
                    "target": "x86_64-pc-windows-msvc",
                    "file": "WokCore-v1.2.3-Windows-x86_64-Portable.zip",
                    "executable": "wokcore.exe",
                    "size": 1,
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/WokCore-v1.2.3-Windows-x86_64-Portable.zip"
                },
                {
                    "target": "aarch64-pc-windows-msvc",
                    "file": "WokCore-v1.2.3-Windows-arm64-Portable.zip",
                    "executable": "wokcore.exe",
                    "size": 1,
                    "sha256": "1111111111111111111111111111111111111111111111111111111111111111",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/WokCore-v1.2.3-Windows-arm64-Portable.zip"
                },
                {
                    "target": "x86_64-apple-darwin",
                    "file": "WokCore-v1.2.3-macOS-x86_64.tar.gz",
                    "executable": "wokcore",
                    "size": 1,
                    "sha256": "2222222222222222222222222222222222222222222222222222222222222222",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/WokCore-v1.2.3-macOS-x86_64.tar.gz"
                },
                {
                    "target": "aarch64-apple-darwin",
                    "file": "WokCore-v1.2.3-macOS-arm64.tar.gz",
                    "executable": "wokcore",
                    "size": 1,
                    "sha256": "3333333333333333333333333333333333333333333333333333333333333333",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/WokCore-v1.2.3-macOS-arm64.tar.gz"
                },
                {
                    "target": "x86_64-unknown-linux-gnu",
                    "file": "WokCore-v1.2.3-Linux-x86_64.tar.gz",
                    "executable": "wokcore",
                    "size": 1,
                    "sha256": "4444444444444444444444444444444444444444444444444444444444444444",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/WokCore-v1.2.3-Linux-x86_64.tar.gz"
                },
                {
                    "target": "aarch64-unknown-linux-gnu",
                    "file": "WokCore-v1.2.3-Linux-arm64.tar.gz",
                    "executable": "wokcore",
                    "size": 1,
                    "sha256": "5555555555555555555555555555555555555555555555555555555555555555",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/WokCore-v1.2.3-Linux-arm64.tar.gz"
                }
            ]
        }"#;
        let decision = validate_document(
            parse_manifest_document(v2).unwrap(),
            "0000000000000000",
            &Version::new(1, 0, 0),
            "aarch64-pc-windows-msvc",
        )
        .unwrap();
        let UpdateDecision::Available(candidate) = decision else {
            panic!("v2 fixture must offer an upgrade");
        };
        assert_eq!(
            candidate.artifact().file(),
            "WokCore-v1.2.3-Windows-arm64-Portable.zip"
        );

        let v1 = br#"{
            "schema_version": 1,
            "product": "wokcore",
            "api_major": 1,
            "version": "1.2.3",
            "signing_key_id": "0000000000000000",
            "artifacts": [
                {
                    "target": "x86_64-pc-windows-msvc",
                    "file": "wokcore-v1.2.3-x86_64-pc-windows-msvc.zip",
                    "executable": "wokcore.exe",
                    "size": 1,
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/wokcore-v1.2.3-x86_64-pc-windows-msvc.zip"
                },
                {
                    "target": "x86_64-apple-darwin",
                    "file": "wokcore-v1.2.3-x86_64-apple-darwin.tar.gz",
                    "executable": "wokcore",
                    "size": 1,
                    "sha256": "1111111111111111111111111111111111111111111111111111111111111111",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/wokcore-v1.2.3-x86_64-apple-darwin.tar.gz"
                },
                {
                    "target": "aarch64-apple-darwin",
                    "file": "wokcore-v1.2.3-aarch64-apple-darwin.tar.gz",
                    "executable": "wokcore",
                    "size": 1,
                    "sha256": "2222222222222222222222222222222222222222222222222222222222222222",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/wokcore-v1.2.3-aarch64-apple-darwin.tar.gz"
                },
                {
                    "target": "x86_64-unknown-linux-gnu",
                    "file": "wokcore-v1.2.3-x86_64-unknown-linux-gnu.tar.gz",
                    "executable": "wokcore",
                    "size": 1,
                    "sha256": "3333333333333333333333333333333333333333333333333333333333333333",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/wokcore-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
                },
                {
                    "target": "aarch64-unknown-linux-gnu",
                    "file": "wokcore-v1.2.3-aarch64-unknown-linux-gnu.tar.gz",
                    "executable": "wokcore",
                    "size": 1,
                    "sha256": "4444444444444444444444444444444444444444444444444444444444444444",
                    "url": "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/wokcore-v1.2.3-aarch64-unknown-linux-gnu.tar.gz"
                }
            ]
        }"#;
        let decision = validate_document(
            parse_manifest_document(v1).unwrap(),
            "0000000000000000",
            &Version::new(1, 0, 0),
            "x86_64-pc-windows-msvc",
        )
        .unwrap();
        let UpdateDecision::Available(candidate) = decision else {
            panic!("v1 fixture must offer an upgrade");
        };
        assert_eq!(
            candidate.artifact().file(),
            "wokcore-v1.2.3-x86_64-pc-windows-msvc.zip"
        );
    }

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
