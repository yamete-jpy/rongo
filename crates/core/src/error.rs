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
    #[error("SSH tunnel: {0}")]
    Tunnel(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("not connected")]
    NotConnected,
    #[error("cancelled")]
    Cancelled,
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
