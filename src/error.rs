use thiserror::Error;

#[derive(Error, Debug)]
pub enum SessyncError {
    #[error("config error: {0}")]
    Config(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("tool adapter error: {0}")]
    Tool(String),

    #[error("keychain error: {0}")]
    Keychain(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, SessyncError>;
