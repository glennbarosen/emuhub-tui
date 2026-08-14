use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("connection to device timed out")]
    ConnectTimeout,
    #[error("authentication failed (tried empty password and 'none')")]
    AuthFailed,
    #[error("not connected to device")]
    NotConnected,
    #[error("SSH error: {0}")]
    Ssh(#[from] russh::Error),
    #[error("SFTP error: {0}")]
    Sftp(#[from] russh_sftp::client::error::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
