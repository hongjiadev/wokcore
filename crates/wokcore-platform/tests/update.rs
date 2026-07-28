use std::{fs, io::Write, path::Path};

#[cfg(not(windows))]
use flate2::{Compression, write::GzEncoder};
use semver::Version;
use sha2::{Digest, Sha256};
#[cfg(not(windows))]
use tar::{Builder as TarBuilder, Header};
use tempfile::tempdir;
use wokcore_platform::update::{
    UpdateArtifact, UpdateDecision, UpdateError, acquire_update_lease, current_target,
    prepare_install, verify_artifact, verify_manifest,
};
#[cfg(windows)]
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const FIXTURE_PUBLIC_KEY: &str = include_str!("fixtures/update/minisign.pub");
const FIXTURE_MANIFEST: &[u8] = include_bytes!("fixtures/update/wokcore-update-v1.json");
const FIXTURE_SIGNATURE: &[u8] = include_bytes!("fixtures/update/wokcore-update-v1.json.minisig");
const INSTALL_FIXTURE_PUBLIC_KEY: &str = include_str!("fixtures/update/install-minisign.pub");
const INSTALL_FIXTURE_MANIFEST: &[u8] =
    include_bytes!("fixtures/update/install-wokcore-update-v1.json");
const INSTALL_FIXTURE_SIGNATURE: &[u8] =
    include_bytes!("fixtures/update/install-wokcore-update-v1.json.minisig");

#[test]
fn signed_update_fixtures_remain_byte_exact() {
    assert_eq!(FIXTURE_PUBLIC_KEY.len(), 113);
    assert_eq!(
        format!("{:x}", Sha256::digest(FIXTURE_PUBLIC_KEY.as_bytes())),
        "85b51eeaea961a2cfdbb36329d42eac8711888d7005afbeda40aad690547209c"
    );
    assert_eq!(FIXTURE_MANIFEST.len(), 1701);
    assert_eq!(
        format!("{:x}", Sha256::digest(FIXTURE_MANIFEST)),
        "ce1ffbaa21831968fdc12f55f7064459b94f9f903389bf9a0b202a4ea0217a42"
    );
    assert_eq!(FIXTURE_SIGNATURE.len(), 289);
    assert_eq!(
        format!("{:x}", Sha256::digest(FIXTURE_SIGNATURE)),
        "8abe48528f8f1bf22d3dd151a2066ccd4f16fda99237f49ed4f7826456bab130"
    );
    assert_eq!(INSTALL_FIXTURE_PUBLIC_KEY.len(), 113);
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(INSTALL_FIXTURE_PUBLIC_KEY.as_bytes())
        ),
        "380eb1a56f24a4ac61acf7441d45e53f213a654f046247c295436bfceced4ab2"
    );
    assert_eq!(INSTALL_FIXTURE_MANIFEST.len(), 1701);
    assert_eq!(
        format!("{:x}", Sha256::digest(INSTALL_FIXTURE_MANIFEST)),
        "c9e61b63a38067d3e8ceed3bc3cd051cc2562175502856f5a341fa6012d930b5"
    );
    assert_eq!(INSTALL_FIXTURE_SIGNATURE.len(), 292);
    assert_eq!(
        format!("{:x}", Sha256::digest(INSTALL_FIXTURE_SIGNATURE)),
        "fbf1193e515298b3163b72e9c6bf547d0b611ab3d60bd51fd3c48806c333c7a8"
    );
}

#[test]
fn signed_manifest_selects_only_the_native_upgrade_and_rejects_downgrades() {
    let available = verify_manifest(
        FIXTURE_MANIFEST,
        FIXTURE_SIGNATURE,
        FIXTURE_PUBLIC_KEY,
        &Version::new(0, 1, 0),
        current_target(),
    )
    .unwrap();
    let UpdateDecision::Available(candidate) = available else {
        panic!("fixture must offer an upgrade");
    };
    assert_eq!(candidate.version(), &Version::new(1, 2, 3));
    assert_eq!(candidate.artifact().target(), current_target());

    assert_eq!(
        verify_manifest(
            FIXTURE_MANIFEST,
            FIXTURE_SIGNATURE,
            FIXTURE_PUBLIC_KEY,
            &Version::new(1, 2, 3),
            current_target(),
        )
        .unwrap(),
        UpdateDecision::Current,
    );
    assert_eq!(
        verify_manifest(
            FIXTURE_MANIFEST,
            FIXTURE_SIGNATURE,
            FIXTURE_PUBLIC_KEY,
            &Version::new(2, 0, 0),
            current_target(),
        )
        .unwrap_err(),
        UpdateError::DowngradeRejected,
    );
}

#[test]
fn signature_and_target_mismatches_fail_closed() {
    let mut corrupted_signature = FIXTURE_SIGNATURE.to_vec();
    let signature_payload = corrupted_signature
        .split_mut(|byte| *byte == b'\n')
        .nth(1)
        .unwrap();
    let index = signature_payload
        .iter()
        .skip(20)
        .position(|byte| byte.is_ascii_alphanumeric())
        .map(|index| index + 20)
        .unwrap();
    signature_payload[index] = if signature_payload[index] == b'A' {
        b'B'
    } else {
        b'A'
    };
    assert_eq!(
        verify_manifest(
            FIXTURE_MANIFEST,
            &corrupted_signature,
            FIXTURE_PUBLIC_KEY,
            &Version::new(0, 1, 0),
            current_target(),
        )
        .unwrap_err(),
        UpdateError::InvalidSignature,
    );

    assert_eq!(
        verify_manifest(
            FIXTURE_MANIFEST,
            FIXTURE_SIGNATURE,
            FIXTURE_PUBLIC_KEY,
            &Version::new(0, 1, 0),
            "unsupported-target",
        )
        .unwrap_err(),
        UpdateError::TargetMismatch,
    );
}

#[test]
fn artifact_hash_size_and_archive_executable_are_verified_before_replacement() {
    let fixture = tempdir().unwrap();
    let archive = archive_path(fixture.path());
    write_archive(&archive, b"new executable");
    let artifact = artifact_for(&archive);

    verify_artifact(&archive, &artifact).unwrap();
    let original_archive = fs::read(&archive).unwrap();
    let mut same_size_corruption = original_archive.clone();
    let middle = same_size_corruption.len() / 2;
    same_size_corruption[middle] ^= 1;
    fs::write(&archive, same_size_corruption).unwrap();
    assert_eq!(
        verify_artifact(&archive, &artifact).unwrap_err(),
        UpdateError::ArtifactHashMismatch,
    );

    fs::write(&archive, &original_archive).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&archive)
        .unwrap()
        .write_all(b"corruption")
        .unwrap();
    assert_eq!(
        verify_artifact(&archive, &artifact).unwrap_err(),
        UpdateError::ArtifactSizeMismatch,
    );

    let wrong_archive = fixture.path().join(if cfg!(windows) {
        "wrong.zip"
    } else {
        "wrong.tar.gz"
    });
    let target = fixture.path().join(executable_name());
    fs::write(&target, b"old executable").unwrap();
    write_archive_entry(&wrong_archive, "not-wokcore", b"new executable");
    let wrong_artifact = artifact_for(&wrong_archive);
    assert_eq!(
        prepare_install(&wrong_archive, &wrong_artifact, &target).unwrap_err(),
        UpdateError::InvalidArchive,
    );
}

#[test]
fn atomic_install_rolls_back_to_the_exact_previous_executable() {
    let fixture = tempdir().unwrap();
    let archive = archive_path(fixture.path());
    let target = fixture.path().join(executable_name());
    fs::write(&target, b"old executable").unwrap();
    write_archive(&archive, b"new executable");
    let artifact = artifact_for(&archive);

    let prepared = prepare_install(&archive, &artifact, &target).unwrap();
    let transaction = prepared.begin().unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"new executable");

    transaction.rollback().unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"old executable");
}

#[test]
fn transaction_rejects_a_backup_path_replacement_before_rollback() {
    let fixture = tempdir().unwrap();
    let archive = archive_path(fixture.path());
    let target = fixture.path().join(executable_name());
    let backup = fixture
        .path()
        .join(format!(".{}.previous", executable_name()));
    fs::write(&target, b"old executable").unwrap();
    write_archive(&archive, b"new executable");
    let artifact = artifact_for(&archive);
    let transaction = prepare_install(&archive, &artifact, &target)
        .unwrap()
        .begin()
        .unwrap();
    fs::remove_file(&backup).unwrap();
    fs::write(&backup, b"untrusted executable").unwrap();

    assert_eq!(
        transaction.rollback().unwrap_err(),
        UpdateError::RecoveryRequired,
    );
    assert_eq!(fs::read(&target).unwrap(), b"new executable");
    assert_eq!(fs::read(&backup).unwrap(), b"untrusted executable");
}

#[test]
fn transaction_rejects_a_target_path_replacement_before_rollback() {
    let fixture = tempdir().unwrap();
    let archive = archive_path(fixture.path());
    let target = fixture.path().join(executable_name());
    fs::write(&target, b"old executable").unwrap();
    write_archive(&archive, b"new executable");
    let artifact = artifact_for(&archive);
    let transaction = prepare_install(&archive, &artifact, &target)
        .unwrap()
        .begin()
        .unwrap();
    fs::remove_file(&target).unwrap();
    fs::write(&target, b"untrusted executable").unwrap();

    assert_eq!(
        transaction.rollback().unwrap_err(),
        UpdateError::RecoveryRequired,
    );
    assert_eq!(fs::read(&target).unwrap(), b"untrusted executable");
}

#[test]
fn abandoning_a_prepared_install_removes_its_same_volume_candidate() {
    let fixture = tempdir().unwrap();
    let archive = archive_path(fixture.path());
    let target = fixture.path().join(executable_name());
    fs::write(&target, b"old executable").unwrap();
    write_archive(&archive, b"new executable");
    let artifact = artifact_for(&archive);
    let prepared = prepare_install(&archive, &artifact, &target).unwrap();
    let candidate = prepared.candidate_path().to_path_buf();

    drop(prepared);

    assert!(!candidate.exists());
    assert_eq!(fs::read(&target).unwrap(), b"old executable");
}

#[test]
fn failed_atomic_install_preserves_the_original_target() {
    let fixture = tempdir().unwrap();
    let archive = archive_path(fixture.path());
    let target = fixture.path().join(executable_name());
    fs::write(&target, b"old executable").unwrap();
    write_archive(&archive, b"new executable");
    let artifact = artifact_for(&archive);

    let prepared = prepare_install(&archive, &artifact, &target).unwrap();
    fs::remove_file(prepared.candidate_path()).unwrap();
    assert_eq!(prepared.begin().unwrap_err(), UpdateError::StagingFailed,);
    assert_eq!(fs::read(&target).unwrap(), b"old executable");
}

#[test]
fn prepared_install_requires_manual_recovery_when_the_target_path_changes() {
    let fixture = tempdir().unwrap();
    let archive = archive_path(fixture.path());
    let target = fixture.path().join(executable_name());
    fs::write(&target, b"old executable").unwrap();
    write_archive(&archive, b"new executable");
    let artifact = artifact_for(&archive);
    let prepared = prepare_install(&archive, &artifact, &target).unwrap();
    let displaced = fixture.path().join("displaced-target");
    fs::rename(&target, &displaced).unwrap();
    fs::write(&target, b"untrusted executable").unwrap();

    assert_eq!(prepared.begin().unwrap_err(), UpdateError::RecoveryRequired,);
    assert_eq!(fs::read(&target).unwrap(), b"untrusted executable");
}

#[test]
fn prepared_install_rejects_a_candidate_path_replacement() {
    let fixture = tempdir().unwrap();
    let archive = archive_path(fixture.path());
    let target = fixture.path().join(executable_name());
    fs::write(&target, b"old executable").unwrap();
    write_archive(&archive, b"new executable");
    let artifact = artifact_for(&archive);
    let prepared = prepare_install(&archive, &artifact, &target).unwrap();
    let candidate = prepared.candidate_path().to_path_buf();
    let displaced = fixture.path().join("displaced-candidate");
    fs::rename(&candidate, &displaced).unwrap();
    fs::write(&candidate, b"new executable").unwrap();

    assert_eq!(prepared.begin().unwrap_err(), UpdateError::StagingFailed,);
    assert_eq!(fs::read(&target).unwrap(), b"old executable");
}

#[test]
fn update_lease_allows_only_one_installer_for_a_target() {
    let fixture = tempdir().unwrap();
    let target = fixture.path().join(executable_name());
    fs::write(&target, b"old executable").unwrap();

    let first = acquire_update_lease(&target).unwrap();
    assert_eq!(
        acquire_update_lease(&target).unwrap_err(),
        UpdateError::UpdateInProgress,
    );
    drop(first);

    acquire_update_lease(&target).unwrap();
}

#[cfg(windows)]
fn write_archive(path: &Path, executable: &[u8]) {
    write_archive_entry(path, executable_name(), executable);
}

#[cfg(windows)]
fn write_archive_entry(path: &Path, name: &str, executable: &[u8]) {
    let file = fs::File::create(path).unwrap();
    let mut archive = ZipWriter::new(file);
    archive
        .start_file(
            name,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .unwrap();
    archive.write_all(executable).unwrap();
    archive.finish().unwrap();
}

#[cfg(not(windows))]
fn write_archive(path: &Path, executable: &[u8]) {
    write_archive_entry(path, executable_name(), executable);
}

#[cfg(not(windows))]
fn write_archive_entry(path: &Path, name: &str, executable: &[u8]) {
    let output = fs::File::create(path).unwrap();
    let encoder = GzEncoder::new(output, Compression::best());
    let mut archive = TarBuilder::new(encoder);
    let mut header = Header::new_ustar();
    header.set_size(u64::try_from(executable.len()).unwrap());
    header.set_mode(0o755);
    header.set_cksum();
    archive.append_data(&mut header, name, executable).unwrap();
    archive.into_inner().unwrap().finish().unwrap();
}

fn artifact_for(path: &Path) -> UpdateArtifact {
    let bytes = fs::read(path).unwrap();
    UpdateArtifact::for_test(
        current_target(),
        path.file_name().unwrap().to_string_lossy().into_owned(),
        executable_name(),
        u64::try_from(bytes.len()).unwrap(),
        format!("{:x}", Sha256::digest(bytes)),
        "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/wokcore.zip",
    )
}

#[cfg(windows)]
fn archive_path(root: &Path) -> std::path::PathBuf {
    root.join("wokcore.zip")
}

#[cfg(not(windows))]
fn archive_path(root: &Path) -> std::path::PathBuf {
    root.join("wokcore.tar.gz")
}

#[cfg(windows)]
fn executable_name() -> &'static str {
    "wokcore.exe"
}

#[cfg(not(windows))]
fn executable_name() -> &'static str {
    "wokcore"
}
