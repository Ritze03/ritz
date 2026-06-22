use thiserror::Error;

#[derive(Debug, Error)]
pub enum RitzError {
    #[error("condition parse error: {0}")]
    Condition(String),

    #[error("invalid extension `{id}`: {reason}")]
    InvalidExtension { id: String, reason: String },

    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("json error at {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

pub type Result<T> = std::result::Result<T, RitzError>;
