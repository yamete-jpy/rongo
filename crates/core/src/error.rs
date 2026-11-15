use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum CoreError {
    #[error("network error: {0}")]
    Network(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("server error {code}: {msg}")]
    Server { code: i32, msg: String },
    /// The write was applied on the primary, but not confirmed as its write
    /// concern asks (replication timed out, say): it may yet be rolled back.
    /// Never a refusal.
    #[error("write concern error {code}: {msg}")]
    WriteConcern { code: i32, msg: String },
    #[error("SSH tunnel: {0}")]
    Tunnel(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("not connected")]
    NotConnected,
    #[error("cancelled")]
    Cancelled,
    /// A write was attempted on a connection marked read-only; carries what was refused.
    #[error("{0}: this connection is read-only")]
    ReadOnly(String),
    /// Collection History could not be read, written or kept (disk, permissions, its key).
    #[error("Collection History: {0}")]
    History(String),
    /// A recorded write would need more history than the size limit or the
    /// free disk space allow; it was refused before anything was written.
    #[error("Collection History would need about {needed} bytes for this operation (limit {limit} bytes, {free} bytes free)")]
    HistoryTooLarge { needed: u64, limit: u64, free: u64 },
    /// The master password does not open the vault, or the file was edited or
    /// damaged: the two can't be told apart.
    #[error("wrong password, or the vault is damaged")]
    WrongPassword,
    /// Setting a master password found a vault already there (made by another
    /// rongo window, say); nothing was written.
    #[error("a vault already exists")]
    VaultExists,
    /// The vault on disk is no longer the one this window unlocked: its
    /// password was changed, or it was reset or deleted, elsewhere. Nothing was
    /// written.
    #[error("the vault was changed by another rongo window")]
    VaultChanged,
    /// Another rongo window held the vault's lock for longer than a write
    /// waits; nothing was written.
    #[error("the vault is busy in another rongo window")]
    VaultBusy,
    #[error("{0}")]
    Other(String),
}

impl From<mongodb::error::Error> for CoreError {
    fn from(e: mongodb::error::Error) -> Self {
        use mongodb::error::ErrorKind;
        let msg = e.to_string();
        match e.kind.as_ref() {
            ErrorKind::Authentication { message, .. } => CoreError::Auth(message.clone()),
            ErrorKind::ServerSelection { .. } => CoreError::Timeout(msg),
            ErrorKind::Io(_) | ErrorKind::DnsResolve { .. } | ErrorKind::ConnectionPoolCleared { .. } => {
                CoreError::Network(msg)
            }
            ErrorKind::InvalidArgument { message, .. } => CoreError::InvalidInput(message.clone()),
            ErrorKind::Command(c) => {
                if c.code == 18 {
                    CoreError::Auth(c.message.clone())
                } else {
                    CoreError::Server { code: c.code, msg: c.message.clone() }
                }
            }
            ErrorKind::Write(mongodb::error::WriteFailure::WriteError(w)) => {
                CoreError::Server { code: w.code, msg: w.message.clone() }
            }
            ErrorKind::Write(mongodb::error::WriteFailure::WriteConcernError(w)) => {
                CoreError::WriteConcern { code: w.code, msg: w.message.clone() }
            }
            // An ordered insert of many (Paste, the shell's insertMany,
            // Duplicate) that refused no document: every one went in on the
            // primary, unconfirmed.
            ErrorKind::InsertMany(f) if f.write_errors.as_ref().is_none_or(Vec::is_empty) && let Some(w) = &f.write_concern_error => {
                CoreError::WriteConcern { code: w.code, msg: w.message.clone() }
            }
            _ => CoreError::Other(msg),
        }
    }
}

impl From<bson::error::Error> for CoreError {
    fn from(e: bson::error::Error) -> Self {
        CoreError::InvalidInput(e.to_string())
    }
}

pub type CoreResult<T> = Result<T, CoreError>;
