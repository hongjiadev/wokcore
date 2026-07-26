mod environment;
mod export_destination;
mod file;

pub use environment::{SessionEnvironment, SessionRootOverrides, SessionRoots, SessionSourceKind};
pub use export_destination::PinnedExportDestination;
pub use file::{
    SessionDirectoryEntry, SessionDirectoryLease, SessionFile, SessionFileIdentity,
    SessionFileKind, SessionFileSnapshot, SessionRootLease,
};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("missing platform data: {name}")]
    MissingPlatformData { name: &'static str },
    #[error("session path is not a safe filesystem object")]
    UnsafePath,
    #[error("session directory enumeration exceeds the caller limit")]
    EnumerationLimitExceeded,
    #[error("session read exceeds the caller limit")]
    ReadLimitExceeded,
    #[error("session I/O failed")]
    Io {
        #[from]
        source: std::io::Error,
    },
}
