use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("unknown bucket")]
    UnknownBucket,

    #[error("unknown account")]
    UnknownAccount,

    #[error("mm not connected")]
    MmNotConnected,

    #[error("not authenticated")]
    NotAuthenticated,

    #[error("malformed message: {0}")]
    Malformed(String),

    #[error("rfq deadline elapsed before any quotes arrived")]
    RfqEmpty,

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
