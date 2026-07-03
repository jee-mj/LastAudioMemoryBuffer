use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum LambError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("control error: {0}")]
    Control(String),

    #[error("capture error: {0}")]
    Capture(String),

    #[error("export error: {0}")]
    Export(String),
}

pub type Result<T> = std::result::Result<T, LambError>;

pub fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> LambError {
    LambError::Io {
        path: path.into(),
        source,
    }
}
