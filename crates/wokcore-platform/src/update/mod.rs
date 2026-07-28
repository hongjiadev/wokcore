mod installer;
mod manifest;
mod rollback;

use semver::Version;

pub use installer::{
    InstallTransaction, PreparedInstall, UpdateLease, acquire_update_lease, prepare_install,
    prepare_install_file, verify_artifact, verify_artifact_file,
};
pub use manifest::{
    MAX_UPDATE_ARTIFACT_BYTES, MAX_UPDATE_MANIFEST_BYTES, MAX_UPDATE_SIGNATURE_BYTES,
    UpdateArtifact, UpdateCandidate, UpdateDecision, UpdateError, current_target, verify_manifest,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallOutcome {
    Installed { from: Version, to: Version },
    RolledBack { attempted: Version },
    ActiveRequestsRemain { count: usize },
}
