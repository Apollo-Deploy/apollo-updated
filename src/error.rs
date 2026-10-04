use thiserror::Error;

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("package has no verified listener handoff contract")]
    HandoffUnavailable,
    #[error("configured supervisor cannot provide reversible generation handoff")]
    SupervisorCannotHandoff,
    #[error("package identity, architecture, or signed target does not match")]
    TargetMismatch,
    #[error("package requires an unsupported updater, package, state, or protocol version")]
    IncompatiblePackage,
    #[error("requested version is older than the highest verified target")]
    RollbackDenied,
    #[error("manifest contains unsupported or unsafe fields")]
    UnsafeManifest,
    #[error("archive member escapes or aliases the immutable version tree")]
    UnsafeArchive,
    #[error("artifact exceeds configured size or download deadline")]
    ArtifactLimit,
    #[error("insufficient disk space for artifact and retained release")]
    InsufficientDisk,
    #[error("durable state is corrupt; manual recovery is required")]
    StateCorrupt,
    #[error("the active package could not be restored safely")]
    RecoveryFailed,
    #[error("package has an unresolved update transaction and requires recovery")]
    RecoveryRequired,
    #[error("package update is already in progress")]
    Busy,
}
