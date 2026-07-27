mod environment;
mod export_destination;
pub(crate) mod file;

pub use environment::{SessionEnvironment, SessionRootOverrides, SessionRoots, SessionSourceKind};
pub(crate) use export_destination::PinnedPublishedFile;
pub use export_destination::{MAX_PINNED_EXPORT_READ_BYTES, PinnedExportDestination};
pub use file::{
    SessionDirectoryEntry, SessionDirectoryLease, SessionDirectoryPage, SessionDirectoryPageKey,
    SessionFile, SessionFileIdentity, SessionFileKind, SessionFileSnapshot, SessionRootLease,
};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("missing platform data: {name}")]
    MissingPlatformData { name: &'static str },
    #[error("session path is not a safe filesystem object")]
    UnsafePath,
    #[error("session directory enumeration exceeds the caller limit")]
    EnumerationLimitExceeded,
    #[error("diagnostic cleanup tombstone budget is exhausted")]
    CleanupLimitExceeded,
    #[error("session read exceeds the caller limit")]
    ReadLimitExceeded,
    #[error("session file changed during a bounded operation")]
    SessionFileChanged,
    #[error("session file is unavailable for the bounded operation")]
    SessionFileUnavailable,
    #[error("session I/O failed")]
    Io {
        #[from]
        source: std::io::Error,
    },
}
