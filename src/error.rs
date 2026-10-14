/// Everything that can stop an update, a report or a verification.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// git could not be started at all.
    #[error("could not run git: {0}")]
    GitSpawn(#[source] std::io::Error),
    /// git ran and failed.
    #[error("git {args} failed: {stderr}")]
    Git { args: String, stderr: String },
    /// git printed something churncode does not understand.
    #[error("unexpected git output: {0}")]
    GitOutput(String),
    /// The request contradicts the database or the repository.
    #[error("{0}")]
    Refused(String),
    /// The stored line origins no longer fit the repository's history.
    #[error("the database does not match the repository: {0}")]
    Inconsistent(String),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}
